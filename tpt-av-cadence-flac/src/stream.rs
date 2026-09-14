//! FLAC stream-level parsing: bit reader, CRC-8/CRC-16, STREAMINFO
//! metadata, and frame headers. (RFC 9639 §7–9.)

use tpt_av_cadence_core::{BufferedSource, CadenceError, Result};

// ---------------------------------------------------------------------------
// CRC (RFC 9639 §9.1.4: CRC-8 poly 0x07 init 0; CRC-16 poly 0x8005 init 0,
// both non-reflected, no final xor)
// ---------------------------------------------------------------------------

const fn build_crc8_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

pub static CRC8_TABLE: [u8; 256] = build_crc8_table();

const fn build_crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

pub static CRC16_TABLE: [u16; 256] = build_crc16_table();

/// One-shot CRC-8 over `bytes`.
pub fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in bytes {
        crc = CRC8_TABLE[(crc ^ b) as usize];
    }
    crc
}

/// One-shot CRC-16 over `bytes`.
pub fn crc16(bytes: &[u8]) -> u16 {
    crc16_update(0, bytes)
}

/// Streaming CRC-16: folds `bytes` into the running `crc`.
pub fn crc16_update(mut crc: u16, bytes: &[u8]) -> u16 {
    for &b in bytes {
        crc = (crc << 8) ^ CRC16_TABLE[(((crc >> 8) as u8) ^ b) as usize];
    }
    crc
}

// ---------------------------------------------------------------------------
// Bit reader (MSB-first, bounded to one frame's byte window)
// ---------------------------------------------------------------------------

/// Reads bit-level fields from a byte slice, most-significant bit first.
///
/// Every read is bounds-checked: running past the end yields
/// [`CadenceError::EndOfStream`], never a panic.
pub struct BitReader<'a> {
    bytes: &'a [u8],
    byte_pos: usize,
    bit_buf: u64,
    bit_count: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        BitReader {
            bytes,
            byte_pos: 0,
            bit_buf: 0,
            bit_count: 0,
        }
    }

    /// Whole bytes consumed so far, ignoring bits still buffered.
    pub fn byte_pos(&self) -> usize {
        self.byte_pos - (self.bit_count / 8) as usize
    }

    /// Bits still available in the window.
    pub fn bits_left(&self) -> u64 {
        8 * (self.bytes.len().saturating_sub(self.byte_pos)) as u64 + self.bit_count as u64
    }

    /// Discards bits up to the next byte boundary.
    pub fn align_to_byte(&mut self) {
        self.bit_count -= self.bit_count % 8;
    }

    fn refill(&mut self) -> Result<()> {
        if self.byte_pos >= self.bytes.len() {
            return Err(CadenceError::EndOfStream);
        }
        self.bit_buf = (self.bit_buf << 8) | self.bytes[self.byte_pos] as u64;
        self.byte_pos += 1;
        self.bit_count += 8;
        Ok(())
    }

    /// Reads `n` bits (n ≤ 56) as an unsigned value.
    pub fn read_bits(&mut self, n: u32) -> Result<u64> {
        debug_assert!(n <= 56, "read_bits supports at most 56 bits");
        if n == 0 {
            return Ok(0);
        }
        if n as u64 > self.bits_left() {
            return Err(CadenceError::EndOfStream);
        }
        while self.bit_count < n {
            self.refill()?;
        }
        let value = (self.bit_buf >> (self.bit_count - n)) & ((1u64 << n) - 1);
        self.bit_count -= n;
        Ok(value)
    }

    /// Reads `n` bits as a two's-complement signed value.
    pub fn read_signed(&mut self, n: u32) -> Result<i64> {
        let raw = self.read_bits(n)?;
        if n == 0 {
            return Ok(0);
        }
        if raw & (1 << (n - 1)) != 0 {
            Ok((raw as i64).wrapping_sub(1 << n))
        } else {
            Ok(raw as i64)
        }
    }

    /// Counts zero bits until the next one bit (which is consumed) and
    /// returns the count. This is FLAC's unary convention (RFC 9639 §9.2.7;
    /// e.g. wasted bits k=7 are coded `0000001`).
    ///
    /// The search is bounded by the window size, so hostile input can only
    /// end in a bounded [`CadenceError`], never a hang.
    pub fn read_unary(&mut self) -> Result<u32> {
        let mut zeros: u32 = 0;
        loop {
            if self.bit_count == 0 && self.refill().is_err() {
                return Err(CadenceError::CorruptData(
                    "unary code runs past the end of the frame".to_string(),
                ));
            }
            let bit = (self.bit_buf >> (self.bit_count - 1)) & 1;
            self.bit_count -= 1;
            if bit == 1 {
                return Ok(zeros);
            }
            zeros = zeros.wrapping_add(1);
        }
    }
}

// ---------------------------------------------------------------------------
// STREAMINFO (RFC 9639 §8.2)
// ---------------------------------------------------------------------------

/// Parsed STREAMINFO metadata block — the stream's ground truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub min_blocksize: u16,
    pub max_blocksize: u16,
    pub min_framesize: u32,
    pub max_framesize: u32,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// Total inter-channel sample frames; 0 means unknown.
    pub total_samples: u64,
    /// MD5 of the unencoded audio (signed little-endian, interleaved).
    pub md5: [u8; 16],
}

pub const METADATA_STREAMINFO: u8 = 0;
pub const METADATA_SEEKTABLE: u8 = 3;
pub const METADATA_VORBIS_COMMENT: u8 = 4;

/// Reads the `fLaC` marker and all metadata blocks from `source`.
/// Returns the parsed STREAMINFO and the byte offset at which audio frames
/// begin (via [`BufferedSource::consumed`]).
pub fn read_metadata(source: &mut BufferedSource) -> Result<(StreamInfo, u64)> {
    let mut magic = [0u8; 4];
    source.take_exact(&mut magic)?;
    if &magic != b"fLaC" {
        return Err(CadenceError::InvalidFormat(
            "not a FLAC stream (missing 'fLaC' marker)".to_string(),
        ));
    }

    let mut streaminfo: Option<StreamInfo> = None;
    let mut block_index = 0u32;
    loop {
        let mut header = [0u8; 4];
        source.take_exact(&mut header)?;
        let is_last = header[0] & 0x80 != 0;
        let block_type = header[0] & 0x7F;
        let length = ((header[1] as u32) << 16) | ((header[2] as u32) << 8) | header[3] as u32;

        if block_type == METADATA_STREAMINFO {
            if streaminfo.is_some() {
                return Err(CadenceError::InvalidFormat(
                    "duplicate STREAMINFO metadata block".to_string(),
                ));
            }
            if block_index != 0 {
                return Err(CadenceError::InvalidFormat(
                    "STREAMINFO must be the first metadata block".to_string(),
                ));
            }
            if length < 34 {
                return Err(CadenceError::CorruptData(format!(
                    "STREAMINFO block too small: {length} bytes (need 34)"
                )));
            }
            let mut body = [0u8; 34];
            source.take_exact(&mut body)?;
            if length > 34 {
                source.skip((length - 34) as u64)?;
            }
            streaminfo = Some(parse_streaminfo(&body)?);
        } else {
            // PADDING, APPLICATION, SEEKTABLE, VORBIS_COMMENT, CUESHEET,
            // PICTURE, … are irrelevant to decode: skip the payload.
            source.skip(length as u64)?;
        }

        block_index += 1;
        if is_last {
            break;
        }
    }

    let streaminfo = streaminfo.ok_or_else(|| {
        CadenceError::InvalidFormat("stream has no STREAMINFO metadata block".to_string())
    })?;
    Ok((streaminfo, source.consumed()))
}

fn parse_streaminfo(body: &[u8; 34]) -> Result<StreamInfo> {
    let min_blocksize = u16::from_be_bytes([body[0], body[1]]);
    let max_blocksize = u16::from_be_bytes([body[2], body[3]]);
    let min_framesize = ((body[4] as u32) << 16) | ((body[5] as u32) << 8) | body[6] as u32;
    let max_framesize = ((body[7] as u32) << 16) | ((body[8] as u32) << 8) | body[9] as u32;

    // 64 bits: sample rate (20) | channels (3) | bps (5) | total samples (36).
    let packed: u64 = (0..8)
        .map(|i| u64::from(body[10 + i]) << (56 - 8 * i))
        .fold(0, |acc, b| acc | b);
    let sample_rate = (packed >> 44) as u32;
    let channels = ((packed >> 41) & 0x7) as u16 + 1;
    let bits_per_sample = ((packed >> 36) & 0x1F) as u16 + 1;
    let total_samples = packed & ((1 << 36) - 1);

    let mut md5 = [0u8; 16];
    md5.copy_from_slice(&body[18..34]);

    let si = StreamInfo {
        min_blocksize,
        max_blocksize,
        min_framesize,
        max_framesize,
        sample_rate,
        channels,
        bits_per_sample,
        total_samples,
        md5,
    };

    // Zero means "unspecified" in broken real-world streams; accept it and
    // let the decoder fall back to the format maximum.
    for (label, v) in [
        ("min_blocksize", min_blocksize),
        ("max_blocksize", max_blocksize),
    ] {
        if v != 0 && !(16..=65535).contains(&v) {
            return Err(CadenceError::CorruptData(format!(
                "STREAMINFO {label} {v} out of range (must be 16..=65535 or 0)"
            )));
        }
    }
    if min_blocksize != 0 && max_blocksize != 0 && max_blocksize < min_blocksize {
        return Err(CadenceError::CorruptData(
            "STREAMINFO max_blocksize is smaller than min_blocksize".to_string(),
        ));
    }
    if si.sample_rate == 0 {
        return Err(CadenceError::CorruptData(
            "STREAMINFO sample rate is zero".to_string(),
        ));
    }
    if si.bits_per_sample < 4 || si.bits_per_sample > 32 {
        return Err(CadenceError::CorruptData(format!(
            "STREAMINFO bit depth {} out of range (4..=32)",
            si.bits_per_sample
        )));
    }
    Ok(si)
}

// ---------------------------------------------------------------------------
// Frame header (RFC 9639 §9.1)
// ---------------------------------------------------------------------------

/// Stereo decorrelation mode signalled by the channel assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StereoMode {
    /// Channel 0 = left, channel 1 = left − right.
    LeftSide,
    /// Channel 0 = right, channel 1 = left − right.
    RightSide,
    /// Channel 0 = (left+right)>>1, channel 1 = left − right.
    MidSide,
}

/// Parsed frame header.
#[derive(Debug, Clone)]
pub struct FrameHeader {
    /// Samples per channel in this frame (1..=65535).
    pub block_size: usize,
    /// Resolved sample rate for this frame.
    pub sample_rate: u32,
    /// Resolved channel count (always matches the stream's).
    pub channels: usize,
    /// Decorrelation mode for stereo streams.
    pub stereo_mode: Option<StereoMode>,
    /// Resolved bit depth for this frame.
    pub bits_per_sample: u16,
    /// Frame number (fixed blocksize) or first sample number (variable).
    pub number: u64,
    pub variable_blocksize: bool,
    /// Total header length in bytes, including the CRC-8.
    pub header_len: usize,
}

/// Parses and validates a frame header starting at `bytes[0]` (which must be
/// the 0xFF of the sync code). On success returns the header; the caller
/// resumes bit-reading at `bytes[header.header_len..]`.
pub fn parse_frame_header(bytes: &[u8], stream: &StreamInfo) -> Result<FrameHeader> {
    if bytes.len() < 2 {
        return Err(CadenceError::EndOfStream);
    }
    if bytes[0] != 0xFF || bytes[1] & 0xFE != 0xF8 {
        return Err(CadenceError::CorruptData(
            "frame sync code not found".to_string(),
        ));
    }

    let mut br = BitReader::new(bytes);
    br.read_bits(14)?; // sync code (already checked)
    let reserved = br.read_bits(1)?;
    if reserved != 0 {
        return Err(CadenceError::CorruptData(
            "frame header reserved bit is not zero".to_string(),
        ));
    }
    let variable_blocksize = br.read_bits(1)? == 1;

    // Codes first; note the block-size and sample-rate extension bytes are
    // stored at the END of the header (after the UTF-8 coded number, right
    // before CRC-8) — RFC 9639 §9.1.2/§9.1.3 — not inline after the codes.
    let bs_code = br.read_bits(4)?;
    let sr_code = br.read_bits(4)?;

    // Channel assignment.
    let assignment = br.read_bits(4)?;
    let (channels, stereo_mode) = match assignment {
        0..=7 => (assignment as usize + 1, None),
        8 => (2, Some(StereoMode::LeftSide)),
        9 => (2, Some(StereoMode::RightSide)),
        10 => (2, Some(StereoMode::MidSide)),
        _ => {
            return Err(CadenceError::CorruptData(format!(
                "reserved channel assignment code {assignment}"
            )))
        }
    };
    if channels != stream.channels as usize {
        return Err(CadenceError::UnsupportedFeature(
            "channel count changes between frames are not supported".to_string(),
        ));
    }

    // Sample size code.
    let bps_code = br.read_bits(3)?;
    let bits_per_sample = match bps_code {
        0 => stream.bits_per_sample,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => {
            return Err(CadenceError::CorruptData(
                "reserved sample size code 3".to_string(),
            ))
        }
    };

    let reserved = br.read_bits(1)?;
    if reserved != 0 {
        return Err(CadenceError::CorruptData(
            "frame header reserved bit (post-sample-size) is not zero".to_string(),
        ));
    }

    let number = read_utf8_number(&mut br, variable_blocksize)?;

    // Extension bytes at the end of the header: block size, then sample rate.
    let block_size = match bs_code {
        0 => {
            return Err(CadenceError::CorruptData(
                "reserved block size code 0".to_string(),
            ))
        }
        1 => 192usize,
        2..=5 => 576usize << (bs_code - 2),
        6 => (br.read_bits(8)? as usize) + 1,
        7 => (br.read_bits(16)? as usize) + 1,
        _ => 256usize << (bs_code - 8),
    };
    if block_size > 65535 {
        return Err(CadenceError::CorruptData(format!(
            "frame block size {block_size} exceeds the 65535 maximum"
        )));
    }

    let sample_rate = match sr_code {
        0 => stream.sample_rate,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => br.read_bits(8)? as u32,
        13 => br.read_bits(16)? as u32,
        14 => br.read_bits(16)? as u32 / 10,
        _ => {
            return Err(CadenceError::CorruptData(
                "invalid sample rate code 15".to_string(),
            ))
        }
    };

    // CRC-8 covers everything from the sync code up to (not including) the
    // CRC byte itself.
    let header_len = br.byte_pos() + 1;
    let stored_crc = *bytes.get(header_len - 1).ok_or(CadenceError::EndOfStream)?;
    if crc8(&bytes[..header_len - 1]) != stored_crc {
        return Err(CadenceError::CorruptData(
            "frame header CRC-8 mismatch".to_string(),
        ));
    }

    Ok(FrameHeader {
        block_size,
        sample_rate,
        channels,
        stereo_mode,
        bits_per_sample,
        number,
        variable_blocksize,
        header_len,
    })
}

/// Decodes FLAC's UTF-8-like variable-length frame/sample number
/// (1–7 bytes; RFC 9639 §9.1.7).
fn read_utf8_number(br: &mut BitReader, variable_blocksize: bool) -> Result<u64> {
    let first = br.read_bits(8)?;
    let (value, extra_bytes) = if first & 0x80 == 0 {
        (first, 0)
    } else if first & 0xE0 == 0xC0 {
        (first & 0x1F, 1)
    } else if first & 0xF0 == 0xE0 {
        (first & 0x0F, 2)
    } else if first & 0xF8 == 0xF0 {
        (first & 0x07, 3)
    } else if first & 0xFC == 0xF8 {
        (first & 0x03, 4)
    } else if first & 0xFE == 0xFC {
        (first & 0x01, 5)
    } else if first & 0xFF == 0xFE {
        (0, 6)
    } else {
        return Err(CadenceError::CorruptData(
            "invalid UTF-8-coded frame/sample number".to_string(),
        ));
    };

    let _ = variable_blocksize; // only interpretation differs, not encoding
    let mut value = value;
    for _ in 0..extra_bytes {
        let cont = br.read_bits(8)?;
        if cont & 0xC0 != 0x80 {
            return Err(CadenceError::CorruptData(
                "invalid continuation byte in UTF-8-coded frame number".to_string(),
            ));
        }
        value = (value << 6) | (cont & 0x3F);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc8_known_vector() {
        // CRC-8 with poly 0x07 (init 0): "123456789" -> 0xF4.
        assert_eq!(crc8(b"123456789"), 0xF4);
    }

    #[test]
    fn crc16_split_equals_one_shot() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let one = crc16(&data);
        let (a, b) = data.split_at(400);
        assert_eq!(crc16_update(crc16(a), b), one);
        let (a, b) = data.split_at(1);
        assert_eq!(crc16_update(crc16(a), b), one);
    }

    #[test]
    fn bitreader_bits_and_unary() {
        // Bits: 1 0 1 | 1 1 0 0 | 0 | 0 1 | 0 0 0 0 0 1
        let bytes = [0b1011_1000u8, 0b0100_0001u8];
        let mut br = BitReader::new(&bytes);
        assert_eq!(br.read_bits(3).unwrap(), 0b101);
        assert_eq!(br.read_signed(4).unwrap(), -4); // 1100 -> -4
        assert_eq!(br.read_unary().unwrap(), 2); // 0,0, then terminating 1
        assert_eq!(br.read_unary().unwrap(), 5); // 00000, then 1
        assert!(br.read_bits(1).is_err()); // past the end
    }
}
