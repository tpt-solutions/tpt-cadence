//! RIFF chunk-level parsing for WAVE files.
//!
//! The layout is `RIFF <u32 size> WAVE` followed by chunks of
//! `<4-byte id><u32 little-endian size><payload><pad byte if odd size>`.
//! Only `fmt ` and `data` are interpreted; everything else is skipped.
//!
//! This module is exposed for tests and tooling; normal users go through
//! [`crate::WavDecoder`].

use tpt_av_cadence_core::{BufferedSource, CadenceError, Result};

/// Resolved contents of a `fmt ` chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FmtChunk {
    /// Format tag: 1 = PCM, 3 = IEEE float, 0xFFFE = extensible
    /// (resolve the actual codec via [`FmtChunk::sub_format_tag`]).
    pub format_tag: u16,
    pub channels: u16,
    pub sample_rate: u32,
    /// Average bytes per second (informational).
    pub byte_rate: u32,
    /// Bytes per sample *frame* across all channels.
    pub block_align: u16,
    /// Bits per sample as declared by the header.
    pub bits_per_sample: u16,
    /// For extensible files: valid bits per sample.
    pub valid_bits_per_sample: Option<u16>,
    /// For extensible files: speaker-position channel mask.
    pub channel_mask: Option<u32>,
    /// For `WAVE_FORMAT_EXTENSIBLE` files, the codec tag named by the
    /// SubFormat GUID (e.g. 1 = PCM, 3 = IEEE float). For classic files this
    /// equals `format_tag`.
    pub sub_format_tag: u16,
}

impl FmtChunk {
    /// The codec tag that actually selects sample interpretation:
    /// the SubFormat GUID tag for extensible files, else the classic tag.
    pub fn effective_format_tag(&self) -> u16 {
        if self.format_tag == 0xFFFE {
            self.sub_format_tag
        } else {
            self.format_tag
        }
    }
}

/// One parsed RIFF chunk header.
#[derive(Debug, Clone, Copy)]
pub struct ChunkHeader {
    pub id: [u8; 4],
    pub size: u32,
}

/// Canonical 14-byte tail shared by every `KSDATAFORMAT_SUBTYPE_*` GUID.
const GUID_SUFFIX: [u8; 14] = [
    0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Builds the little-endian byte layout of a `KSDATAFORMAT_SUBTYPE_*` GUID.
pub fn subtype_guid(format_tag: u16) -> [u8; 16] {
    let mut guid = [0u8; 16];
    guid[0] = (format_tag & 0xFF) as u8;
    guid[1] = (format_tag >> 8) as u8;
    guid[2..].copy_from_slice(&GUID_SUFFIX);
    guid
}

/// True when `guid` equals the canonical subtype GUID naming `format_tag`.
pub fn guid_is_subtype(guid: &[u8], format_tag: u16) -> bool {
    guid.len() == 16
        && guid[2..] == GUID_SUFFIX
        && guid[0] == (format_tag & 0xFF) as u8
        && guid[1] == (format_tag >> 8) as u8
}

/// Reads and validates the 12-byte RIFF file header.
///
/// The RIFF size field is advisory and intentionally not enforced — plenty of
/// real-world writers get it wrong.
pub fn parse_riff_header(source: &mut BufferedSource) -> Result<()> {
    let mut header = [0u8; 12];
    source.take_exact(&mut header)?;
    if &header[0..4] != b"RIFF" {
        if &header[0..4] == b"RIFX" {
            return Err(CadenceError::UnsupportedFeature(
                "RIFX (big-endian RIFF) files are not supported".to_string(),
            ));
        }
        return Err(CadenceError::InvalidFormat(
            "not a RIFF file (missing 'RIFF' magic)".to_string(),
        ));
    }
    if &header[8..12] != b"WAVE" {
        return Err(CadenceError::InvalidFormat(
            "RIFF container is not of form type 'WAVE'".to_string(),
        ));
    }
    Ok(())
}

/// Reads the next chunk header (8 bytes: id + little-endian size).
pub fn next_chunk(source: &mut BufferedSource) -> Result<ChunkHeader> {
    let mut header = [0u8; 8];
    source.take_exact(&mut header)?;
    let mut id = [0u8; 4];
    id.copy_from_slice(&header[0..4]);
    Ok(ChunkHeader {
        id,
        size: u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
    })
}

/// Parses a `fmt ` chunk payload (source positioned at its first byte) of
/// `size` bytes. Handles the classic 16-byte form and the 40-byte
/// `WAVE_FORMAT_EXTENSIBLE` form; extra extension bytes are skipped.
pub fn parse_fmt_chunk(source: &mut BufferedSource, size: u32) -> Result<FmtChunk> {
    if size < 16 {
        return Err(CadenceError::CorruptData(format!(
            "fmt chunk too small: {size} bytes (need at least 16)"
        )));
    }

    let mut base = [0u8; 16];
    source.take_exact(&mut base)?;
    let mut fmt = FmtChunk {
        format_tag: u16::from_le_bytes([base[0], base[1]]),
        channels: u16::from_le_bytes([base[2], base[3]]),
        sample_rate: u32::from_le_bytes([base[4], base[5], base[6], base[7]]),
        byte_rate: u32::from_le_bytes([base[8], base[9], base[10], base[11]]),
        block_align: u16::from_le_bytes([base[12], base[13]]),
        bits_per_sample: u16::from_le_bytes([base[14], base[15]]),
        valid_bits_per_sample: None,
        channel_mask: None,
        sub_format_tag: 0,
    };
    let mut sub_tag = fmt.format_tag;

    if fmt.format_tag == 0xFFFE {
        // WAVE_FORMAT_EXTENSIBLE: cbSize (>= 22), valid bits, channel mask,
        // SubFormat GUID.
        let mut ext = [0u8; 24];
        if size < 40 {
            return Err(CadenceError::CorruptData(
                "extensible fmt chunk too small (need at least 40 bytes)".to_string(),
            ));
        }
        source.take_exact(&mut ext)?;
        let cb_size = u16::from_le_bytes([ext[0], ext[1]]);
        if cb_size < 22 {
            return Err(CadenceError::CorruptData(format!(
                "extensible fmt cbSize {cb_size} is smaller than the required 22"
            )));
        }
        fmt.valid_bits_per_sample = Some(u16::from_le_bytes([ext[2], ext[3]]));
        fmt.channel_mask = Some(u32::from_le_bytes([ext[4], ext[5], ext[6], ext[7]]));
        sub_tag = u16::from_le_bytes([ext[8], ext[9]]);
        if !guid_is_subtype(&ext[8..24], sub_tag) {
            return Err(CadenceError::UnsupportedFeature(format!(
                "extensible SubFormat GUID is not a canonical KSDATAFORMAT subtype (tag 0x{sub_tag:04X})"
            )));
        }
    }

    fmt.sub_format_tag = sub_tag;

    // Skip any additional extension bytes the writer appended.
    let parsed = if fmt.format_tag == 0xFFFE { 40 } else { 16 };
    if size > parsed {
        source.skip((size - parsed) as u64)?;
    }
    Ok(fmt)
}
