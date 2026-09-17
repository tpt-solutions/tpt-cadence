//! MPEG audio frame header parsing, CRC-16 protection, and frame geometry.

use tpt_av_cadence_core::{CadenceError, Result};

use crate::tables::{BASE_HZ, HALF_RATE};

/// A parsed MPEG-1/2/2.5 Layer III frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Raw 4 header bytes (needed for CRC and stereo-mode helpers).
    pub bytes: [u8; 4],
    /// Bitrate in kbps (from the tables; `0` = free format, unsupported).
    pub bitrate_kbps: u32,
    /// Sample rate in Hz.
    pub sample_rate_hz: u32,
    /// true for MPEG-1 (frame holds two granules of 576 samples).
    pub mpeg1: bool,
    /// true for MPEG-2/2.5 (the "LSF" low-sample-frequency family).
    pub lsf: bool,
    /// Mono (channel mode 3).
    pub mono: bool,
    /// Joint stereo (channel mode 1).
    pub joint_stereo: bool,
    /// Mid/side stereo enabled (mode_ext bit 2).
    pub ms_stereo: bool,
    /// Intensity stereo enabled (mode_ext bit 1).
    pub i_stereo: bool,
    /// Frame carries a 16-bit CRC after the header.
    pub crc: bool,
    /// Total samples per frame (1152 MPEG-1, 576 MPEG-2/2.5).
    pub samples_per_frame: u32,
    /// Frame payload bytes (without padding).
    pub frame_bytes: u32,
    /// Padding bytes appended to this frame.
    pub padding: u32,
}

fn bitrate_index(h: &[u8]) -> u32 {
    ((h[2] >> 4) as u32) & 0xF
}

pub(crate) fn sample_rate_index(h: &[u8]) -> u32 {
    ((h[2] >> 2) as u32) & 0x3
}

fn layer_bits(h: &[u8]) -> u32 {
    ((h[1] >> 1) as u32) & 0x3
}

fn mode(h: &[u8]) -> u32 {
    (h[3] >> 6) as u32 & 0x3
}

fn mode_ext(h: &[u8]) -> u32 {
    (h[3] >> 4) as u32 & 0x3
}

fn half_rate_index(h: &[u8]) -> usize {
    // [mpeg1][layer - 1][index]: Layer III is `layer_bits == 1`.
    (mpeg1_flag(h) as usize) * 45 + ((layer_bits(h) - 1) as usize) * 15 + bitrate_index(h) as usize
}

fn mpeg1_flag(h: &[u8]) -> bool {
    h[1] & 0x08 != 0
}

fn not_mpeg25_flag(h: &[u8]) -> bool {
    h[1] & 0x10 != 0
}

/// Syntactic validity of a 4-byte candidate header (Layer III only).
pub fn is_valid_header(h: &[u8]) -> bool {
    h.len() >= 4
        && h[0] == 0xFF
        // sync + ID + (MPEG 2.5 allowed) + layer != 0
        && ((h[1] & 0xF0) == 0xF0 || (h[1] & 0xFE) == 0xE2)
        && layer_bits(h) == 1
        && bitrate_index(h) != 15
        && bitrate_index(h) != 0 // free format unsupported
        && sample_rate_index(h) != 3
}

/// Parses a valid Layer III header into its geometry.
pub fn parse_header(h: &[u8]) -> Result<FrameHeader> {
    if !is_valid_header(h) {
        return Err(CadenceError::InvalidFormat(
            "not a Layer III frame sync".into(),
        ));
    }
    let mpeg1 = mpeg1_flag(h);
    let bitrate_kbps = 2 * HALF_RATE[half_rate_index(h)] as u32;
    let sample_rate_hz =
        BASE_HZ[sample_rate_index(h) as usize] >> (!mpeg1 as u32) >> (!not_mpeg25_flag(h) as u32);
    let frame_bytes = (if mpeg1 { 1152 } else { 576 }) * bitrate_kbps * 125 / sample_rate_hz;
    let md = mode(h);
    let joint = md == 1;
    Ok(FrameHeader {
        bytes: [h[0], h[1], h[2], h[3]],
        bitrate_kbps,
        sample_rate_hz,
        mpeg1,
        lsf: !mpeg1,
        mono: md == 3,
        joint_stereo: joint,
        ms_stereo: joint && mode_ext(h) & 2 != 0,
        i_stereo: joint && mode_ext(h) & 1 != 0,
        crc: h[1] & 1 == 0,
        samples_per_frame: if mpeg1 { 1152 } else { 576 },
        frame_bytes,
        padding: ((h[2] >> 1) & 1) as u32,
    })
}

impl FrameHeader {
    /// Total on-wire span to the next frame: the standard frame-length
    /// formula already counts the 4-byte header (and any CRC bytes).
    pub fn total_bytes(&self) -> usize {
        self.frame_bytes as usize + self.padding as usize
    }
}

/// MPEG audio CRC-16 per ISO 11172-3 §2.4.3.1: generator polynomial
/// x^16 + x^15 + x^2 + 1 (0x8005), initial value 0xFFFF, MSB-first,
/// no final inversion. Covers header bytes 2–3 followed by the side info.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x8005;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mpeg1_stereo_128k() {
        // 0xFF 0xFB 0x90 0x64: MPEG1 Layer III, 128 kbps, 44100, joint stereo
        let h = parse_header(&[0xFF, 0xFB, 0x90, 0x64]).unwrap();
        assert_eq!(h.bitrate_kbps, 128);
        assert_eq!(h.sample_rate_hz, 44100);
        assert!(h.mpeg1);
        assert!(!h.lsf);
        assert!(h.joint_stereo);
        assert_eq!(h.samples_per_frame, 1152);
        assert_eq!(h.frame_bytes, 1152 * 128 * 125 / 44100);
        assert!(!h.crc);
    }

    #[test]
    fn parses_mpeg2_mono() {
        // 0xFF 0xF3 0x38 0xC4: MPEG-2 Layer III, 24 kbps, 16000, mono
        let h = parse_header(&[0xFF, 0xF3, 0x38, 0xC4]).unwrap();
        assert_eq!(h.bitrate_kbps, 24);
        assert_eq!(h.sample_rate_hz, 16000);
        assert!(!h.mpeg1);
        assert!(h.lsf);
        assert!(h.mono);
        assert_eq!(h.samples_per_frame, 576);
    }

    #[test]
    fn mpeg25_divides_twice() {
        // 0xFF 0xE3 0x18 0xC4: MPEG-2.5 Layer III, 24 kbps, 8000, mono
        let h = parse_header(&[0xFF, 0xE3, 0x18, 0xC4]).unwrap();
        assert_eq!(h.sample_rate_hz, 8000);
        // sample-rate index 1 (12000) in MPEG-2.5
        let h = parse_header(&[0xFF, 0xE3, 0x14, 0xC4]).unwrap();
        assert_eq!(h.sample_rate_hz, 12000);
    }

    #[test]
    fn rejects_bad_sync() {
        assert!(!is_valid_header(&[0xFA, 0xFB, 0x90, 0x64]));
        assert!(!is_valid_header(&[0xFF, 0xFB, 0xF0, 0x64])); // bitrate 15
        assert!(!is_valid_header(&[0xFF, 0xFB, 0x04, 0x64])); // free format
        assert!(!is_valid_header(&[0xFF, 0xFB, 0x9C, 0x64])); // reserved rate
        assert!(!is_valid_header(&[0xFF, 0xFC, 0x90, 0x64])); // layer 2
    }

    #[test]
    fn crc16_matches_independent_vectors() {
        // MSB-first CRC-16, poly 0x8005, init 0xFFFF — vectors computed
        // independently with a bitwise reference model.
        assert_eq!(crc16(b"123456789"), 0xAEE7);
        assert_eq!(crc16(&[0x00]), 0xFD02);
        assert_eq!(crc16(&[0xFF, 0xFB]), 0x801B);
    }
}
