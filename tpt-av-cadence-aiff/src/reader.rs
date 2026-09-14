//! IFF chunk-level parsing for AIFF/AIFC files.
//!
//! The layout is `FORM <u32 size> AIFF|AIFC` followed by chunks of
//! `<4-byte id><u32 big-endian size><payload><pad byte if odd size>`.
//! `COMM` is interpreted here; `SSND` is located by the decoder.

use tpt_av_cadence_core::{BufferedSource, CadenceError, Result};

use crate::ext_float::extended_to_f64;

/// Resolved contents of a `COMM` chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct CommChunk {
    pub channels: u16,
    /// Frame count as declared by the header.
    pub num_sample_frames: u32,
    /// Bits per sample as declared (1–32).
    pub sample_size: u16,
    /// Sample rate, decoded from the 80-bit extended field.
    pub sample_rate: f64,
    /// Four-character compression type; `NONE` for classic AIFF.
    pub compression_type: [u8; 4],
}

/// One parsed IFF chunk header.
#[derive(Debug, Clone, Copy)]
pub struct ChunkHeader {
    pub id: [u8; 4],
    pub size: u32,
}

/// Reads and validates the 12-byte FORM header. Returns the form type
/// (`AIFF` or `AIFC`).
pub fn parse_form_header(source: &mut BufferedSource) -> Result<[u8; 4]> {
    let mut header = [0u8; 12];
    source.take_exact(&mut header)?;
    if &header[0..4] != b"FORM" {
        return Err(CadenceError::InvalidFormat(
            "not an IFF file (missing 'FORM' magic)".to_string(),
        ));
    }
    let mut form_type = [0u8; 4];
    form_type.copy_from_slice(&header[8..12]);
    if &form_type != b"AIFF" && &form_type != b"AIFC" {
        return Err(CadenceError::InvalidFormat(format!(
            "IFF form type '{}' is not AIFF/AIFC",
            String::from_utf8_lossy(&form_type)
        )));
    }
    Ok(form_type)
}

/// Reads the next chunk header (id + big-endian size).
pub fn next_chunk(source: &mut BufferedSource) -> Result<ChunkHeader> {
    let mut header = [0u8; 8];
    source.take_exact(&mut header)?;
    let mut id = [0u8; 4];
    id.copy_from_slice(&header[0..4]);
    Ok(ChunkHeader {
        id,
        size: u32::from_be_bytes([header[4], header[5], header[6], header[7]]),
    })
}

/// Parses a `COMM` chunk payload of `size` bytes.
pub fn parse_comm_chunk(source: &mut BufferedSource, size: u32) -> Result<CommChunk> {
    // Base: channels(2) + frames(4) + sample size(2) + rate(10) = 18 bytes.
    if size < 18 {
        return Err(CadenceError::CorruptData(format!(
            "COMM chunk too small: {size} bytes (need at least 18)"
        )));
    }

    let mut base = [0u8; 18];
    source.take_exact(&mut base)?;
    let channels = u16::from_be_bytes([base[0], base[1]]);
    let num_sample_frames = u32::from_be_bytes([base[2], base[3], base[4], base[5]]);
    let sample_size = u16::from_be_bytes([base[6], base[7]]);
    let mut rate_bytes = [0u8; 10];
    rate_bytes.copy_from_slice(&base[8..18]);
    let sample_rate = extended_to_f64(&rate_bytes).ok_or_else(|| {
        CadenceError::CorruptData("COMM sample rate is NaN or infinite".to_string())
    })?;

    let mut compression_type = *b"NONE";
    if size >= 22 {
        // AIFF-C: 4CC compression type plus a Pascal-string name
        // (1 count byte + count bytes, padded to an even field length).
        let mut fourcc = [0u8; 4];
        source.take_exact(&mut fourcc)?;
        compression_type = fourcc;

        let mut count = [0u8; 1];
        source.take_exact(&mut count)?;
        // Clamp a malformed count so we never read past the chunk.
        let count = (count[0] as u32).min(size - 22);
        source.skip(count as u64)?;

        let field_len = (1 + count + 1) & !1;
        let pad = field_len - 1 - count;
        source.skip(pad as u64)?;

        // Skip any trailing bytes beyond the defined COMM layout.
        let defined = 18 + 4 + field_len;
        if size > defined {
            source.skip((size - defined) as u64)?;
        }
    }

    Ok(CommChunk {
        channels,
        num_sample_frames,
        sample_size,
        sample_rate,
        compression_type,
    })
}
