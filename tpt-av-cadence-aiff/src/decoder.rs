//! AIFF/AIFC decoder implementation.

use std::io::Read;

use tpt_av_cadence_core::{
    int_to_f32, BufferedSource, ByteSource, CadenceError, Decoder, Format, FormatReader,
    SampleFormat, StreamInfo, Unseekable,
};

use crate::reader;

/// Sample byte order within the sound data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endian {
    Big,
    Little,
}

/// Decoder for AIFF/AIFC audio.
///
/// All allocation happens in [`AiffDecoder::from_source`]/[`AiffDecoder::open`];
/// [`Decoder::decode`] is allocation-free, lock-free, and panic-free.
pub struct AiffDecoder {
    source: BufferedSource,
    info: StreamInfo,
    sample_format: SampleFormat,
    endian: Endian,
    /// Absolute byte offset of the first sample (SSND data + `offset`).
    data_start: u64,
    /// Bounded readable bytes: the SSND payload clipped to what the COMM
    /// frame count implies.
    data_len: u64,
    /// min(declared frames, frames actually available).
    total_frames: u64,
    frames_decoded: u64,
    block_align: usize,
    /// One sample frame of raw bytes; allocated once at open time.
    frame: Box<[u8]>,
}

impl AiffDecoder {
    /// Opens an AIFF/AIFC file over a byte source. Seekable sources
    /// (`Read + Seek + Send`) get working `seek()`.
    pub fn from_source(source: Box<dyn ByteSource>) -> Result<Self, CadenceError> {
        let mut source = BufferedSource::new(source, 8192);
        reader::parse_form_header(&mut source)?;

        let mut comm = None;
        let mut data_start = 0u64;
        let mut available = 0u64;
        let mut found_ssnd = false;

        while !found_ssnd {
            let chunk = reader::next_chunk(&mut source)?;
            match &chunk.id {
                b"FVER" => {
                    // AIFC format version; 0xA2805140 is the only defined one.
                    // Lenient: skip without validating.
                    source.skip(chunk.size as u64 + (chunk.size & 1) as u64)?;
                }
                b"COMM" => {
                    if comm.is_some() {
                        return Err(CadenceError::InvalidFormat(
                            "duplicate COMM chunk".to_string(),
                        ));
                    }
                    comm = Some(reader::parse_comm_chunk(&mut source, chunk.size)?);
                }
                b"SSND" => {
                    let _comm = comm.as_ref().ok_or_else(|| {
                        CadenceError::InvalidFormat(
                            "SSND chunk appears before the COMM chunk".to_string(),
                        )
                    })?;
                    if chunk.size < 8 {
                        return Err(CadenceError::CorruptData(
                            "SSND chunk too small for its offset/blockSize fields".to_string(),
                        ));
                    }
                    let mut header = [0u8; 8];
                    source.take_exact(&mut header)?;
                    let sound_offset = u64::from(u32::from_be_bytes([
                        header[0], header[1], header[2], header[3],
                    ]));
                    // blockSize is a hint (usually 0) and is ignored.
                    if sound_offset + 8 > chunk.size as u64 {
                        return Err(CadenceError::CorruptData(
                            "SSND data offset exceeds the chunk size".to_string(),
                        ));
                    }
                    available = chunk.size as u64 - 8 - sound_offset;
                    source.skip(sound_offset)?;
                    data_start = source.consumed();
                    found_ssnd = true;
                }
                _ => {
                    // ANNO, NAME, AUTH, MARK, INST, MIDI, APPL, …
                    source.skip(chunk.size as u64 + (chunk.size & 1) as u64)?;
                }
            }
        }

        let comm = comm.expect("found_ssnd implies comm was parsed");
        if comm.channels == 0 {
            return Err(CadenceError::CorruptData(
                "COMM declares zero channels".to_string(),
            ));
        }
        if comm.sample_rate <= 0.0 {
            return Err(CadenceError::CorruptData(format!(
                "COMM sample rate {} is not a positive number",
                comm.sample_rate
            )));
        }
        let (sample_format, endian) = resolve_encoding(&comm)?;

        let block_align = comm.channels as usize * sample_format.bytes_per_sample() as usize;
        let declared = comm.num_sample_frames as u64;
        let total_frames = declared.min(available / block_align as u64);
        // Decode is bounded by both the SSND payload and the declared count.
        let data_len = available.min(declared.saturating_mul(block_align as u64));

        let mut info = StreamInfo::new(
            Format::Aiff,
            comm.sample_rate.round() as u32,
            comm.channels,
            sample_format.bit_depth(),
        );
        info.total_frames = Some(total_frames);
        info.validate()?;

        Ok(AiffDecoder {
            source,
            info,
            sample_format,
            endian,
            data_start,
            data_len,
            total_frames,
            frames_decoded: 0,
            block_align,
            frame: vec![0u8; block_align].into_boxed_slice(),
        })
    }

    /// Convenience constructor over a plain readable (unseekable) source.
    pub fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Self::from_source(Box::new(Unseekable(source)))
    }

    /// Converts one raw sample into `f32` according to the resolved encoding.
    fn convert_sample(&self, bytes: &[u8]) -> f32 {
        let be = self.endian == Endian::Big;
        match self.sample_format {
            // AIFF stores 8-bit PCM *signed* (unlike WAV's unsigned form).
            SampleFormat::Int8 => int_to_f32(bytes[0] as i8 as i64, 8),
            SampleFormat::Int16 => {
                let v = if be {
                    i16::from_be_bytes([bytes[0], bytes[1]])
                } else {
                    i16::from_le_bytes([bytes[0], bytes[1]])
                };
                int_to_f32(v as i64, 16)
            }
            SampleFormat::Int24 => {
                let (hi, mid, lo) = if be {
                    (bytes[0], bytes[1], bytes[2])
                } else {
                    (bytes[2], bytes[1], bytes[0])
                };
                let mut v = (lo as i32) | ((mid as i32) << 8) | ((hi as i32) << 16);
                if v >= 1 << 23 {
                    v -= 1 << 24;
                }
                int_to_f32(v as i64, 24)
            }
            SampleFormat::Int32 => {
                let b = [bytes[0], bytes[1], bytes[2], bytes[3]];
                let v = if be {
                    i32::from_be_bytes(b)
                } else {
                    i32::from_le_bytes(b)
                };
                int_to_f32(v as i64, 32)
            }
            SampleFormat::Float32 => {
                let b = [bytes[0], bytes[1], bytes[2], bytes[3]];
                if be {
                    f32::from_be_bytes(b)
                } else {
                    f32::from_le_bytes(b)
                }
            }
            SampleFormat::Float64 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&bytes[..8]);
                if be {
                    f64::from_be_bytes(b) as f32
                } else {
                    f64::from_le_bytes(b) as f32
                }
            }
        }
    }
}

/// Maps an AIFF/AIFC compression type + sample size to the concrete encoding.
fn resolve_encoding(comm: &reader::CommChunk) -> Result<(SampleFormat, Endian), CadenceError> {
    let int_format = |bits: u16| match bits {
        8 => Ok(SampleFormat::Int8),
        16 => Ok(SampleFormat::Int16),
        24 => Ok(SampleFormat::Int24),
        32 => Ok(SampleFormat::Int32),
        other => Err(CadenceError::UnsupportedFeature(format!(
            "AIFF integer sample size of {other} bits is not supported (8/16/24/32 only)"
        ))),
    };

    match &comm.compression_type {
        b"NONE" | b"twos" => Ok((int_format(comm.sample_size)?, Endian::Big)),
        b"sowt" => Ok((int_format(comm.sample_size)?, Endian::Little)),
        b"FL32" | b"fl32" => Ok((SampleFormat::Float32, Endian::Big)),
        b"FL64" | b"fl64" => Ok((SampleFormat::Float64, Endian::Big)),
        b"in24" => Ok((SampleFormat::Int24, Endian::Big)),
        b"ni24" => Ok((SampleFormat::Int24, Endian::Little)),
        other => Err(CadenceError::UnsupportedFeature(format!(
            "AIFF-C compression '{}' is not supported (supported: NONE, twos, sowt, \
             FL32, FL64, in24, ni24)",
            String::from_utf8_lossy(other)
        ))),
    }
}

impl std::fmt::Debug for AiffDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiffDecoder")
            .field("info", &self.info)
            .field("sample_format", &self.sample_format)
            .field("endian", &self.endian)
            .field("total_frames", &self.total_frames)
            .finish()
    }
}

impl Decoder for AiffDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, frame: u64) -> Result<(), CadenceError> {
        if frame > self.total_frames {
            return Err(CadenceError::SeekOutOfRange {
                requested: frame,
                total: self.total_frames,
            });
        }
        let byte =
            frame
                .checked_mul(self.block_align as u64)
                .ok_or(CadenceError::SeekOutOfRange {
                    requested: frame,
                    total: self.total_frames,
                })?;
        self.source.seek_to(self.data_start + byte)?;
        self.source.set_limit(Some(self.data_len - byte));
        self.frames_decoded = frame;
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
        let want_frames =
            (buffer.len() / channels).min((self.total_frames - self.frames_decoded) as usize);
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
                    self.frames_decoded += 1;
                }
                // Truncated sound data ends the stream.
                Err(CadenceError::EndOfStream) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(frames)
    }
}

/// Reader wrapper implementing [`FormatReader`] for AIFF/AIFC files.
pub struct AiffReader {
    decoder: AiffDecoder,
}

impl FormatReader for AiffReader {
    fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Ok(AiffReader {
            decoder: AiffDecoder::open(source)?,
        })
    }

    fn decoder(&mut self) -> &mut dyn Decoder {
        &mut self.decoder
    }

    fn info(&self) -> &StreamInfo {
        self.decoder.info()
    }
}
