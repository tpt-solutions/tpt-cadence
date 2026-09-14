//! ADTS (Audio Data Transport Stream) header parsing (ISO/IEC 13818-7).

use crate::CadenceError;

/// Sampling frequencies by index (ISO/IEC 14496-3 Table 1.16).
pub const SAMPLING_FREQUENCIES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Parsed ADTS fixed+variable header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtsHeader {
    /// MPEG-2 marker (`id` bit) — 1 = MPEG-2, 0 = MPEG-4.
    pub mpeg2: bool,
    /// `profile` field + 1 equals the audio object type (2 = AAC-LC).
    pub object_type: u8,
    pub sampling_frequency_index: u8,
    pub channel_configuration: u8,
    /// Total frame length in bytes, including this header.
    pub frame_length: usize,
    /// Number of raw data blocks in this frame minus one.
    pub raw_blocks_minus_one: u8,
    /// Header length in bytes (7, or 9 with CRC).
    pub header_len: usize,
}

impl AdtsHeader {
    /// Parses the 7 fixed header bytes. `buf[0] == 0xFF` is assumed.
    pub fn parse(buf: &[u8]) -> Result<Self, CadenceError> {
        if buf.len() < 7 {
            return Err(CadenceError::EndOfStream);
        }
        if buf[0] != 0xFF || buf[1] & 0xF0 != 0xF0 {
            return Err(CadenceError::CorruptData(
                "ADTS syncword not found".to_string(),
            ));
        }
        let mpeg2 = buf[1] & 0x08 != 0;
        let layer = buf[1] & 0x06;
        if layer != 0 {
            return Err(CadenceError::UnsupportedFeature(
                "ADTS layer is not zero (not AAC)".to_string(),
            ));
        }
        let protection_absent = buf[1] & 0x01 != 0;
        let object_type = ((buf[2] & 0xC0) >> 6) + 1;
        let sampling_frequency_index = (buf[2] & 0x3C) >> 2;
        let channel_configuration = ((buf[2] & 0x01) << 2) | ((buf[3] & 0xC0) >> 6);
        let frame_length =
            ((buf[3] as usize) & 0x03) << 11 | (buf[4] as usize) << 3 | (buf[5] as usize) >> 5;
        let raw_blocks_minus_one = buf[6] & 0x03;

        if object_type != 2 {
            return Err(CadenceError::UnsupportedFeature(format!(
                "ADTS profile {object_type} is not AAC-LC (2)"
            )));
        }
        if sampling_frequency_index as usize >= SAMPLING_FREQUENCIES.len() {
            return Err(CadenceError::UnsupportedFeature(
                "ADTS sampling frequency index is reserved".to_string(),
            ));
        }
        if !protection_absent {
            return Err(CadenceError::UnsupportedFeature(
                "ADTS frames with CRC protection are not supported".to_string(),
            ));
        }
        if frame_length < 7 {
            return Err(CadenceError::CorruptData(
                "ADTS frame length is smaller than the header".to_string(),
            ));
        }

        Ok(AdtsHeader {
            mpeg2,
            object_type,
            sampling_frequency_index,
            channel_configuration,
            frame_length,
            raw_blocks_minus_one,
            header_len: 7,
        })
    }
}
