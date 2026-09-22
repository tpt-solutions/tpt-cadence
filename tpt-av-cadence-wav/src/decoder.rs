//! PCM / IEEE-float decoder for RIFF/WAVE files.

use std::io::Read;

use tpt_av_cadence_core::{
    int_to_f32, BufferedSource, ByteSource, CadenceError, Decoder, Format, FormatReader,
    SampleFormat, StreamInfo, Unseekable,
};

use crate::reader;

const FORMAT_TAG_PCM: u16 = 0x0001;
const FORMAT_TAG_IEEE_FLOAT: u16 = 0x0003;

/// Decoder for RIFF/WAVE audio.
///
/// All allocation happens in [`WavDecoder::from_source`]/[`WavDecoder::open`];
/// [`Decoder::decode`] is allocation-free, lock-free, and panic-free.
pub struct WavDecoder {
    source: BufferedSource,
    info: StreamInfo,
    sample_format: SampleFormat,
    /// WAV stores 8-bit PCM *unsigned*; every other encoding is signed.
    unsigned_8bit: bool,
    /// Absolute byte offset of the first sample in the `data` payload.
    data_start: u64,
    /// Total bytes of sample data (`u64::MAX` when the header declared the
    /// streaming sentinel 0xFFFFFFFF, meaning "read to end of file").
    data_len: u64,
    block_align: usize,
    /// One sample frame of raw bytes; allocated once at open time.
    frame: Box<[u8]>,
}

impl WavDecoder {
    /// Opens a WAVE file over a byte source. Files and in-memory cursors
    /// (anything implementing `Read + Seek + Send`) get working `seek()`;
    /// sources opened through this constructor's `Read`-only sibling do not.
    pub fn from_source(source: Box<dyn ByteSource>) -> Result<Self, CadenceError> {
        let mut source = BufferedSource::new(source, 8192);
        reader::parse_riff_header(&mut source)?;

        let mut fmt = None;
        let mut data_start = 0u64;
        let mut data_len = 0u64;
        let mut found_data = false;

        while !found_data {
            let chunk = reader::next_chunk(&mut source)?;
            match &chunk.id {
                b"fmt " => {
                    if fmt.is_some() {
                        return Err(CadenceError::InvalidFormat(
                            "duplicate fmt chunk".to_string(),
                        ));
                    }
                    fmt = Some(reader::parse_fmt_chunk(&mut source, chunk.size)?);
                }
                b"data" => {
                    let fmt = fmt.as_ref().ok_or_else(|| {
                        CadenceError::InvalidFormat(
                            "data chunk appears before the fmt chunk".to_string(),
                        )
                    })?;
                    // Bytes-per-frame is recomputed from the resolved format
                    // rather than trusted from the header (broken writers).
                    let bps = match fmt.effective_format_tag() {
                        FORMAT_TAG_PCM | FORMAT_TAG_IEEE_FLOAT => fmt.bits_per_sample as usize / 8,
                        tag => {
                            return Err(CadenceError::UnsupportedFeature(format!(
                                "WAV codec tag 0x{tag:04X} is not supported \
                                 (supported: PCM, IEEE float)"
                            )));
                        }
                    };
                    if fmt.bits_per_sample % 8 != 0 || bps == 0 {
                        return Err(CadenceError::UnsupportedFeature(format!(
                            "unsupported bit depth {} (must be a multiple of 8)",
                            fmt.bits_per_sample
                        )));
                    }
                    if fmt.channels == 0 {
                        return Err(CadenceError::CorruptData(
                            "fmt chunk declares zero channels".to_string(),
                        ));
                    }
                    let computed_align = fmt.channels as usize * bps;
                    if fmt.block_align as usize != computed_align {
                        log::warn!(
                            "WAV header block_align {} disagrees with the computed \
                             {} (using the computed value)",
                            fmt.block_align,
                            computed_align
                        );
                    }
                    data_start = source.consumed();
                    // 0xFFFFFFFF is the streaming sentinel for "unknown length".
                    data_len = if chunk.size == 0xFFFF_FFFF {
                        u64::MAX
                    } else {
                        chunk.size as u64
                    };
                    source.set_limit(if data_len == u64::MAX {
                        None
                    } else {
                        Some(data_len)
                    });
                    found_data = true;
                }
                _ => {
                    // Unknown chunk (LIST, fact, cue, bext, …): skip payload
                    // plus the pad byte that follows odd-sized payloads.
                    source.skip(chunk.size as u64 + (chunk.size & 1) as u64)?;
                }
            }
        }

        // Safe: the `b"data"` arm above returns early with an error whenever
        // `fmt` is still `None`, so `found_data` can only become true once
        // `fmt` is `Some`.
        let fmt = fmt.expect("found_data implies fmt was parsed");
        let tag = fmt.effective_format_tag();
        let (sample_format, unsigned_8bit) = match (tag, fmt.bits_per_sample) {
            (FORMAT_TAG_PCM, 8) => (SampleFormat::Int8, true),
            (FORMAT_TAG_PCM, 16) => (SampleFormat::Int16, false),
            (FORMAT_TAG_PCM, 24) => (SampleFormat::Int24, false),
            (FORMAT_TAG_PCM, 32) => (SampleFormat::Int32, false),
            (FORMAT_TAG_IEEE_FLOAT, 32) => (SampleFormat::Float32, false),
            (FORMAT_TAG_IEEE_FLOAT, 64) => (SampleFormat::Float64, false),
            (tag, bits) => {
                return Err(CadenceError::UnsupportedFeature(format!(
                    "WAV codec tag 0x{tag:04X} at {bits}-bit is not supported \
                     (supported: PCM 8/16/24/32-bit, IEEE float 32/64-bit)"
                )));
            }
        };

        let block_align = fmt.channels as usize * sample_format.bytes_per_sample() as usize;
        let total_frames = if data_len == u64::MAX {
            None
        } else {
            Some(data_len / block_align as u64)
        };

        let mut info = StreamInfo::new(
            Format::Wav,
            fmt.sample_rate,
            fmt.channels,
            sample_format.bit_depth(),
        );
        info.total_frames = total_frames;
        info.validate()?;

        Ok(WavDecoder {
            source,
            info,
            sample_format,
            unsigned_8bit,
            data_start,
            data_len,
            block_align,
            frame: vec![0u8; block_align].into_boxed_slice(),
        })
    }

    /// Convenience constructor over a plain readable (unseekable) source.
    /// `seek()` will return [`CadenceError::UnsupportedFeature`].
    pub fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Self::from_source(Box::new(Unseekable(source)))
    }

    /// Converts one raw little-endian sample into `f32`.
    fn convert_sample(&self, bytes: &[u8]) -> f32 {
        match self.sample_format {
            SampleFormat::Int8 => {
                if self.unsigned_8bit {
                    ((bytes[0] as i32) - 128) as f32 / 128.0
                } else {
                    int_to_f32(bytes[0] as i8 as i64, 8)
                }
            }
            SampleFormat::Int16 => int_to_f32(i16::from_le_bytes([bytes[0], bytes[1]]) as i64, 16),
            SampleFormat::Int24 => {
                let mut v =
                    (bytes[0] as i32) | ((bytes[1] as i32) << 8) | ((bytes[2] as i32) << 16);
                if v >= 1 << 23 {
                    v -= 1 << 24;
                }
                int_to_f32(v as i64, 24)
            }
            SampleFormat::Int32 => int_to_f32(
                i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i64,
                32,
            ),
            SampleFormat::Float32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            SampleFormat::Float64 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(bytes);
                f64::from_le_bytes(b) as f32
            }
        }
    }
}

impl std::fmt::Debug for WavDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WavDecoder")
            .field("info", &self.info)
            .field("sample_format", &self.sample_format)
            .field("unsigned_8bit", &self.unsigned_8bit)
            .field("data_len", &self.data_len)
            .finish()
    }
}

impl Decoder for WavDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, frame: u64) -> Result<(), CadenceError> {
        let total = self.info.total_frames.unwrap_or(u64::MAX);
        if frame > total {
            return Err(CadenceError::SeekOutOfRange {
                requested: frame,
                total,
            });
        }
        let byte =
            frame
                .checked_mul(self.block_align as u64)
                .ok_or(CadenceError::SeekOutOfRange {
                    requested: frame,
                    total,
                })?;
        self.source.seek_to(self.data_start + byte)?;
        self.source.set_limit(if self.data_len == u64::MAX {
            None
        } else {
            Some(self.data_len - byte)
        });
        Ok(())
    }

    fn decode(&mut self, buffer: &mut [f32]) -> Result<usize, CadenceError> {
        let channels = self.info.channels as usize;
        if buffer.len() % channels != 0 {
            return Err(CadenceError::InvalidFormat(format!(
                "buffer length {} is not a multiple of the channel count {}",
                buffer.len(),
                channels
            )));
        }
        let want_frames = buffer.len() / channels;
        let mut frames = 0;
        while frames < want_frames {
            match self.source.take_exact(&mut self.frame) {
                Ok(()) => {
                    let bps = self.sample_format.bytes_per_sample() as usize;
                    let out = &mut buffer[frames * channels..(frames + 1) * channels];
                    for (ch, slot) in out.iter_mut().enumerate() {
                        *slot = self.convert_sample(&self.frame[ch * bps..(ch + 1) * bps]);
                    }
                    frames += 1;
                }
                // A trailing partial frame (truncated file) ends the stream.
                Err(CadenceError::EndOfStream) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(frames)
    }
}

/// Reader wrapper implementing [`FormatReader`] for WAVE files.
pub struct WavReader {
    decoder: WavDecoder,
}

impl FormatReader for WavReader {
    fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Ok(WavReader {
            decoder: WavDecoder::open(source)?,
        })
    }

    fn decoder(&mut self) -> &mut dyn Decoder {
        &mut self.decoder
    }

    fn info(&self) -> &StreamInfo {
        self.decoder.info()
    }
}
