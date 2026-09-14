//! Opus packet framing (RFC 6716 §3): TOC byte, frame packing codes 0–3,
//! padding, and the configuration tables.
//!
//! Parsing allocates nothing: frame boundaries are written into a fixed
//! 48-entry array (the RFC caps a packet at 48 frames), so this is safe to
//! use on a real-time thread.

use tpt_av_cadence_core::{CadenceError, Result};

/// Maximum number of frames in one Opus packet (2.5 ms frames, 120 ms).
pub const MAX_FRAMES_PER_PACKET: usize = 48;

/// Operating mode selected by the TOC configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// SILK-only (LP-based, speech-optimized).
    Silk,
    /// Hybrid SILK + CELT.
    Hybrid,
    /// CELT-only (MDCT-based, music-optimized).
    Celt,
}

/// Audio bandwidth selected by the TOC configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bandwidth {
    Narrowband,
    Mediumband,
    Wideband,
    Superwideband,
    Fullband,
}

/// Frame duration selected by the TOC configuration, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDuration {
    Ms2_5,
    Ms5,
    Ms10,
    Ms20,
    Ms40,
    Ms60,
}

impl FrameDuration {
    /// Duration in microseconds.
    pub fn micros(self) -> u64 {
        match self {
            FrameDuration::Ms2_5 => 2_500,
            FrameDuration::Ms5 => 5_000,
            FrameDuration::Ms10 => 10_000,
            FrameDuration::Ms20 => 20_000,
            FrameDuration::Ms40 => 40_000,
            FrameDuration::Ms60 => 60_000,
        }
    }
}

/// The decoded TOC byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toc {
    pub config: u8,
    pub stereo: bool,
    /// Frame count code (0–3), i.e. the low two bits.
    pub code: u8,
}

impl Toc {
    pub fn from_byte(b: u8) -> Self {
        Toc {
            config: b >> 3,
            stereo: b & 0x04 != 0,
            code: b & 0x03,
        }
    }

    pub fn to_byte(self) -> u8 {
        (self.config << 3) | ((self.stereo as u8) << 2) | self.code
    }

    /// Operating mode for this configuration.
    pub fn mode(&self) -> Mode {
        match self.config {
            0..=11 => Mode::Silk,
            12..=15 => Mode::Hybrid,
            _ => Mode::Celt,
        }
    }

    /// Audio bandwidth for this configuration.
    pub fn bandwidth(&self) -> Bandwidth {
        match self.config {
            0..=3 => Bandwidth::Narrowband,
            4..=7 => Bandwidth::Mediumband,
            8..=11 => Bandwidth::Wideband,
            12..=13 => Bandwidth::Superwideband,
            14..=15 => Bandwidth::Fullband,
            16..=19 => Bandwidth::Narrowband,
            20..=23 => Bandwidth::Wideband,
            24..=27 => Bandwidth::Superwideband,
            _ => Bandwidth::Fullband,
        }
    }

    /// Frame duration for this configuration.
    pub fn frame_duration(&self) -> FrameDuration {
        match self.config {
            // SILK: 10, 20, 40, 60 ms.
            0..=11 => match self.config % 4 {
                0 => FrameDuration::Ms10,
                1 => FrameDuration::Ms20,
                2 => FrameDuration::Ms40,
                _ => FrameDuration::Ms60,
            },
            // Hybrid: 10, 20 ms.
            12..=15 => {
                if self.config % 2 == 0 {
                    FrameDuration::Ms10
                } else {
                    FrameDuration::Ms20
                }
            }
            // CELT: 2.5, 5, 10, 20 ms.
            _ => match self.config % 4 {
                0 => FrameDuration::Ms2_5,
                1 => FrameDuration::Ms5,
                2 => FrameDuration::Ms10,
                _ => FrameDuration::Ms20,
            },
        }
    }
}

/// A parsed Opus packet: the TOC plus the byte range of each contained frame.
///
/// Frame ranges index into the original payload slice. A zero-length range
/// represents a DTX (discontinuous transmission) frame, which the decoder
/// treats as packet loss.
#[derive(Debug, Clone, Copy)]
pub struct Packet {
    pub toc: Toc,
    /// Count of padding bytes signalled by a code 3 packet.
    pub padding_bytes: usize,
    frame_ranges: [(usize, usize); MAX_FRAMES_PER_PACKET],
    frame_count: usize,
}

impl Packet {
    /// Number of frames in this packet (ranges may be empty for DTX).
    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Returns the byte range of frame `i` in the original payload.
    pub fn frame_range(&self, i: usize) -> Option<(usize, usize)> {
        self.frame_ranges.get(i).copied()
    }

    /// Maximum number of frames allowed for this TOC's duration (120 ms cap).
    pub fn max_frames_for(toc: Toc) -> usize {
        let ms = toc.frame_duration().micros() / 1000;
        (120 / ms.max(1)) as usize
    }
}

fn read_frame_length(payload: &[u8], pos: &mut usize) -> Result<usize> {
    let b = *payload.get(*pos).ok_or_else(|| {
        CadenceError::CorruptData("packet ends inside a frame length".to_string())
    })?;
    *pos += 1;
    if b < 252 {
        Ok(b as usize)
    } else {
        let b2 = *payload.get(*pos).ok_or_else(|| {
            CadenceError::CorruptData("packet ends inside a two-byte frame length".to_string())
        })?;
        *pos += 1;
        Ok(b2 as usize * 4 + b as usize)
    }
}

/// Parses an Opus packet into its frames (RFC 6716 §3.2).
pub fn parse_packet(payload: &[u8]) -> Result<Packet> {
    if payload.is_empty() {
        return Err(CadenceError::InvalidFormat("empty Opus packet".to_string()));
    }
    let toc = Toc::from_byte(payload[0]);
    let mut packet = Packet {
        toc,
        padding_bytes: 0,
        frame_ranges: [(0, 0); MAX_FRAMES_PER_PACKET],
        frame_count: 0,
    };
    let n = payload.len();
    let mut pos = 1usize;

    let push_frame = |packet: &mut Packet, start: usize, end: usize| -> Result<()> {
        if packet.frame_count >= MAX_FRAMES_PER_PACKET {
            return Err(CadenceError::CorruptData(
                "packet exceeds 48 frames".to_string(),
            ));
        }
        packet.frame_ranges[packet.frame_count] = (start, end);
        packet.frame_count += 1;
        Ok(())
    };

    match toc.code {
        0 => {
            push_frame(&mut packet, 1, n)?;
        }
        1 => {
            if (n - 1) % 2 != 0 {
                return Err(CadenceError::CorruptData(
                    "code 1 packet has an odd payload length".to_string(),
                ));
            }
            let half = 1 + (n - 1) / 2;
            push_frame(&mut packet, 1, half)?;
            push_frame(&mut packet, half, n)?;
        }
        2 => {
            let len0 = read_frame_length(payload, &mut pos)?;
            let end = pos + len0;
            if end > n {
                return Err(CadenceError::CorruptData(
                    "code 2 packet's first frame exceeds the packet size".to_string(),
                ));
            }
            push_frame(&mut packet, pos, end)?;
            push_frame(&mut packet, end, n)?;
        }
        _ => {
            // Code 3: frame count byte (v p M MMMMMM), optional padding
            // length, then frames.
            if n < 2 {
                return Err(CadenceError::CorruptData(
                    "code 3 packet must have at least 2 bytes".to_string(),
                ));
            }
            let count_byte = payload[1];
            let vbr = count_byte & 0x01 != 0;
            let has_padding = count_byte & 0x02 != 0;
            let frame_count = (count_byte >> 2) as usize;
            pos = 2;
            if frame_count == 0 {
                return Err(CadenceError::CorruptData(
                    "code 3 packet declares zero frames".to_string(),
                ));
            }
            if frame_count > Packet::max_frames_for(toc) {
                return Err(CadenceError::CorruptData(format!(
                    "code 3 packet declares {frame_count} frames, exceeding the 120 ms cap"
                )));
            }

            if has_padding {
                // Padding length: 0..254 = that many bytes; 255 = 254 plus
                // the next byte's value (repeatable).
                let mut padding = 0usize;
                loop {
                    let b = *payload.get(pos).ok_or_else(|| {
                        CadenceError::CorruptData(
                            "packet ends inside the padding length".to_string(),
                        )
                    })?;
                    pos += 1;
                    if b == 255 {
                        // 255 means "254 bytes, plus the next byte's value".
                        padding += 254;
                    } else {
                        padding += b as usize;
                        break;
                    }
                }
                packet.padding_bytes = padding;
                if padding > n - pos {
                    return Err(CadenceError::CorruptData(
                        "padding length exceeds the packet size".to_string(),
                    ));
                }
            }

            let data_end = n - packet.padding_bytes;
            if vbr {
                // All M-1 frame lengths come first (RFC 6716 §3.2.5,
                // Figure 7), then the frames follow back-to-back.
                let mut lengths = [0usize; MAX_FRAMES_PER_PACKET];
                for len in lengths.iter_mut().take(frame_count - 1) {
                    *len = read_frame_length(payload, &mut pos)?;
                }
                if pos > data_end {
                    return Err(CadenceError::CorruptData(
                        "VBR code 3 lengths exceed the packet data".to_string(),
                    ));
                }
                for (i, &len) in lengths.iter().enumerate().take(frame_count) {
                    let end = if i == frame_count - 1 {
                        data_end
                    } else {
                        pos + len
                    };
                    if end > data_end {
                        return Err(CadenceError::CorruptData(
                            "VBR code 3 frame exceeds the packet data".to_string(),
                        ));
                    }
                    push_frame(&mut packet, pos, end)?;
                    pos = end;
                }
            } else {
                if pos > data_end {
                    return Err(CadenceError::CorruptData(
                        "CBR code 3 packet has no frame data".to_string(),
                    ));
                }
                let size = (data_end - pos) / frame_count;
                for _ in 0..frame_count {
                    let end = pos + size;
                    push_frame(&mut packet, pos, end)?;
                    pos = end;
                }
            }
        }
    }
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_roundtrip_and_tables() {
        for config in 0u8..32 {
            let toc = Toc::from_byte((config << 3) | 0x04 | 0x02);
            assert_eq!(toc.config, config);
            assert!(toc.stereo);
            assert_eq!(toc.code, 2);
            assert_eq!(toc.to_byte(), (config << 3) | 0x06);
        }
        // Spot-check Table 2.
        assert_eq!(Toc::from_byte(0).mode(), Mode::Silk);
        assert_eq!(Toc::from_byte(0).bandwidth(), Bandwidth::Narrowband);
        assert_eq!(Toc::from_byte(0).frame_duration(), FrameDuration::Ms10);
        assert_eq!(Toc::from_byte(3 << 3).frame_duration(), FrameDuration::Ms60);
        assert_eq!(
            Toc::from_byte(13 << 3).bandwidth(),
            Bandwidth::Superwideband
        );
        assert_eq!(Toc::from_byte(13 << 3).mode(), Mode::Hybrid);
        assert_eq!(
            Toc::from_byte(16 << 3).frame_duration(),
            FrameDuration::Ms2_5
        );
        assert_eq!(Toc::from_byte(31 << 3).mode(), Mode::Celt);
        assert_eq!(Toc::from_byte(31 << 3).bandwidth(), Bandwidth::Fullband);
        assert_eq!(
            Toc::from_byte(31 << 3).frame_duration(),
            FrameDuration::Ms20
        );
    }

    #[test]
    fn code0_single_frame() {
        let payload = [0xFC, 1, 2, 3, 4];
        let packet = parse_packet(&payload).unwrap();
        assert_eq!(packet.frame_count(), 1);
        assert_eq!(packet.frame_range(0), Some((1, 5)));
    }

    #[test]
    fn code1_two_equal_frames() {
        let payload = [0x01, 1, 2, 3, 4];
        let packet = parse_packet(&payload).unwrap();
        assert_eq!(packet.frame_count(), 2);
        assert_eq!(packet.frame_range(0), Some((1, 3)));
        assert_eq!(packet.frame_range(1), Some((3, 5)));
    }

    #[test]
    fn code1_odd_length_is_rejected() {
        assert!(parse_packet(&[0x01, 1, 2, 3]).is_err());
    }

    #[test]
    fn code2_two_frames_with_lengths() {
        // Length 3 (1 byte), then frame, then remainder.
        let payload = [0x02, 3, 10, 11, 12, 20, 21];
        let packet = parse_packet(&payload).unwrap();
        assert_eq!(packet.frame_count(), 2);
        assert_eq!(packet.frame_range(0), Some((2, 5)));
        assert_eq!(packet.frame_range(1), Some((5, 7)));

        // Two-byte length: 252 + second byte 4 => 4*4 + 252 = 268.
        let mut payload = vec![0x02, 252, 4];
        payload.extend(std::iter::repeat_n(0u8, 268 + 2));
        let packet = parse_packet(&payload).unwrap();
        assert_eq!(packet.frame_range(0), Some((3, 271)));
        assert_eq!(packet.frame_range(1), Some((271, 273)));
    }

    #[test]
    fn code2_overflow_is_rejected() {
        // First frame longer than the packet.
        let payload = [0x02, 200, 1, 2];
        assert!(parse_packet(&payload).is_err());
        // Two-byte length cut short.
        assert!(parse_packet(&[0x02, 252]).is_err());
    }

    #[test]
    fn code3_cbr_with_padding() {
        // Code 3, CBR (v=0), padding (p=1), M=3; padding length 5;
        // 3 frames of 4 bytes + 5 padding.
        let mut payload = vec![0x03, 0x02 | 0x04 | (3 << 2), 5];
        payload.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        payload.extend_from_slice(&[0u8; 5]);
        let packet = parse_packet(&payload).unwrap();
        assert_eq!(packet.frame_count(), 3);
        assert_eq!(packet.padding_bytes, 5);
        assert_eq!(packet.frame_range(0), Some((3, 7)));
        assert_eq!(packet.frame_range(1), Some((7, 11)));
        assert_eq!(packet.frame_range(2), Some((11, 15)));
    }

    #[test]
    fn code3_vbr_with_dtx() {
        // Code 3, VBR (v=1), no padding, M=3; lengths 4, 0 (DTX) up front,
        // then frames back-to-back: 4 bytes, 0 bytes, remainder 2.
        let payload = [0x03, 0x01 | (3 << 2), 4, 0, 5, 6, 7, 8, 9, 10];
        let packet = parse_packet(&payload).unwrap();
        assert_eq!(packet.frame_count(), 3);
        assert_eq!(packet.frame_range(0), Some((4, 8)));
        assert_eq!(packet.frame_range(1), Some((8, 8))); // DTX
        assert_eq!(packet.frame_range(2), Some((8, 10)));
    }

    #[test]
    fn code3_extended_padding_length() {
        // Padding length 255 -> 254 + next value (1) = 255 bytes.
        let mut payload = vec![0x03, 0x02 | 0x04 | (1 << 2), 255, 1];
        payload.push(0xAA); // one frame byte
        payload.extend(std::iter::repeat_n(0u8, 255));
        let packet = parse_packet(&payload).unwrap();
        assert_eq!(packet.padding_bytes, 255);
        assert_eq!(packet.frame_count(), 1);
        assert_eq!(packet.frame_range(0), Some((4, 5)));
    }

    #[test]
    fn code3_zero_frame_count_is_rejected() {
        assert!(parse_packet(&[0x03, 0x00]).is_err());
    }

    #[test]
    fn empty_packet_is_rejected() {
        assert!(parse_packet(&[]).is_err());
    }
}
