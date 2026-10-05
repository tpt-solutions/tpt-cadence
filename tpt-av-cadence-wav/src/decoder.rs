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
    /// Bulk-read slab for [`Decoder::decode`] (a whole number of frames);
    /// allocated once at open time.
    slab: Box<[u8]>,
}

impl WavDecoder {
    /// Opens a WAVE file over a byte source. Files and in-memory cursors
    /// (anything implementing `Read + Seek + Send`) get working `seek()`;
    /// sources opened through this constructor's `Read`-only sibling do not.
    pub fn from_source(source: Box<dyn ByteSource>) -> Result<Self, CadenceError> {
        let mut source = BufferedSource::new(source, 64 * 1024);
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
            // ~4096 frames per bulk read, capped so wide/high-depth formats
            // don't push the slab past 256 KiB.
            slab: {
                let slab_frames = (256 * 1024 / block_align).clamp(1, 4096);
                vec![0u8; block_align * slab_frames].into_boxed_slice()
            },
        })
    }

    /// Convenience constructor over a plain readable (unseekable) source.
    /// `seek()` will return [`CadenceError::UnsupportedFeature`].
    pub fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Self::from_source(Box::new(Unseekable(source)))
    }

    /// Converts a whole slab of interleaved little-endian samples into `out`
    /// (also interleaved, `slab.len() / bytes_per_sample` samples). The
    /// format match is hoisted out of the per-sample loop; the arithmetic is
    /// unchanged from the old per-sample `convert_sample`, so output is
    /// bit-identical.
    fn convert_slab(&self, slab: &[u8], out: &mut [f32]) {
        match self.sample_format {
            SampleFormat::Int8 => {
                if self.unsigned_8bit {
                    for (o, b) in out.iter_mut().zip(slab) {
                        *o = ((*b as i32) - 128) as f32 / 128.0;
                    }
                } else {
                    for (o, b) in out.iter_mut().zip(slab) {
                        *o = int_to_f32(*b as i8 as i64, 8);
                    }
                }
            }
            SampleFormat::Int16 => {
                for (o, b) in out.iter_mut().zip(slab.chunks_exact(2)) {
                    *o = int_to_f32(i16::from_le_bytes([b[0], b[1]]) as i64, 16);
                }
            }
            SampleFormat::Int24 => {
                for (o, b) in out.iter_mut().zip(slab.chunks_exact(3)) {
                    let mut v = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
                    if v >= 1 << 23 {
                        v -= 1 << 24;
                    }
                    *o = int_to_f32(v as i64, 24);
                }
            }
            SampleFormat::Int32 => {
                for (o, b) in out.iter_mut().zip(slab.chunks_exact(4)) {
                    *o = int_to_f32(i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64, 32);
                }
            }
            SampleFormat::Float32 => {
                for (o, b) in out.iter_mut().zip(slab.chunks_exact(4)) {
                    *o = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                }
            }
            SampleFormat::Float64 => {
                for (o, b) in out.iter_mut().zip(slab.chunks_exact(8)) {
                    *o =
                        f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f32;
                }
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
        // Bulk-read whole frames into the slab, then convert in one pass —
        // the format dispatch happens once per call, not once per sample.
        // A trailing partial frame (truncated file) ends the stream.
        while frames < want_frames {
            let want_bytes = ((want_frames - frames) * self.block_align).min(self.slab.len());
            let got = self.source.take_up_to(&mut self.slab[..want_bytes])?;
            let whole = got - got % self.block_align;
            if whole == 0 {
                break;
            }
            let got_frames = whole / self.block_align;
            let out = &mut buffer[frames * channels..(frames + got_frames) * channels];
            self.convert_slab(&self.slab[..whole], out);
            frames += got_frames;
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
