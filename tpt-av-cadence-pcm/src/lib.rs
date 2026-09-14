//! # tpt-av-cadence-pcm
//!
//! Raw headerless PCM reader.
//!
//! Headerless PCM carries no metadata: the caller must supply the sample
//! format, byte order, channel count, and sample rate up front. This is
//! essential for testing, interop with DSP pipelines, and reading `.raw`,
//! `.pcm`, and `.dat` audio dumps.
//!
//! # Example
//!
//! ```no_run
//! use std::fs::File;
//! use tpt_av_cadence_core::{Decoder, SampleFormat};
//! use tpt_av_cadence_pcm::{ByteOrder, PcmDecoder, PcmFormat};
//!
//! # fn main() -> Result<(), tpt_av_cadence_core::CadenceError> {
//! let format = PcmFormat {
//!     sample_format: SampleFormat::Int16,
//!     byte_order: ByteOrder::Little,
//!     channels: 2,
//!     sample_rate: 48_000,
//! };
//! let file = File::open("audio.raw")?;
//! let mut decoder = PcmDecoder::from_source(Box::new(file), format)?;
//!
//! let mut buf = vec![0.0f32; 1024 * 2];
//! loop {
//!     let frames = decoder.decode(&mut buf)?;
//!     if frames == 0 { break; }
//!     // interleaved PCM in buf[..frames * 2]
//! }
//! # Ok(())
//! # }
//! ```

use std::io::Read;

use tpt_av_cadence_core::{
    int_to_f32, BufferedSource, ByteSource, CadenceError, Decoder, Format, SampleFormat,
    StreamInfo, Unseekable,
};

/// Byte order of samples in a headerless PCM stream.
///
/// (Container formats carry their own endianness: WAV/RIFF is little-endian,
/// AIFF/IFF is big-endian — those crates handle it internally.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    Little,
    Big,
}

/// Full description of a headerless PCM stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    /// How individual samples are encoded.
    pub sample_format: SampleFormat,
    /// Byte order of multi-byte samples.
    pub byte_order: ByteOrder,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Sample rate in Hz.
    pub sample_rate: u32,
}

impl PcmFormat {
    /// Bytes per sample *frame* (one sample per channel).
    pub fn block_align(&self) -> usize {
        self.channels as usize * self.sample_format.bytes_per_sample() as usize
    }

    fn validate(&self) -> Result<(), CadenceError> {
        if self.channels == 0 {
            return Err(CadenceError::InvalidFormat(
                "PCM stream must have at least 1 channel".to_string(),
            ));
        }
        if self.sample_rate == 0 {
            return Err(CadenceError::InvalidFormat(
                "PCM stream must have a non-zero sample rate".to_string(),
            ));
        }
        Ok(())
    }
}

/// Decoder for headerless PCM.
///
/// Implements [`tpt_av_cadence_core::Decoder`]; all allocation happens in
/// [`PcmDecoder::from_source`].
pub struct PcmDecoder {
    source: BufferedSource,
    format: PcmFormat,
    info: StreamInfo,
    /// One sample frame of raw bytes; allocated once at open time.
    frame: Box<[u8]>,
}

impl PcmDecoder {
    /// Opens a headerless PCM stream over a seekable or unseekable byte source.
    pub fn from_source(
        source: Box<dyn ByteSource>,
        format: PcmFormat,
    ) -> Result<Self, CadenceError> {
        format.validate()?;
        let block_align = format.block_align();
        let mut info = StreamInfo::new(
            Format::RawPcm,
            format.sample_rate,
            format.channels,
            format.sample_format.bit_depth(),
        );
        // Total frames are unknowable without seeking to measure the stream
        // length; leave `None` (live-stream semantics).
        info.total_frames = None;
        info.validate()?;
        Ok(PcmDecoder {
            source: BufferedSource::new(source, 8192),
            format,
            info,
            frame: vec![0u8; block_align].into_boxed_slice(),
        })
    }

    /// Convenience constructor over a plain readable (unseekable) source.
    pub fn open(source: Box<dyn Read + Send>, format: PcmFormat) -> Result<Self, CadenceError> {
        Self::from_source(Box::new(Unseekable(source)), format)
    }

    /// Converts one raw sample (held in `bytes`) to `f32`.
    fn convert_sample(&self, bytes: &[u8]) -> f32 {
        let le = self.format.byte_order == ByteOrder::Little;
        match self.format.sample_format {
            SampleFormat::Int8 => int_to_f32(bytes[0] as i8 as i64, 8),
            SampleFormat::Int16 => {
                let v = if le {
                    i16::from_le_bytes([bytes[0], bytes[1]])
                } else {
                    i16::from_be_bytes([bytes[0], bytes[1]])
                };
                int_to_f32(v as i64, 16)
            }
            SampleFormat::Int24 => {
                let (b0, b1, b2) = if le {
                    (bytes[0], bytes[1], bytes[2])
                } else {
                    (bytes[2], bytes[1], bytes[0])
                };
                let mut v = (b0 as i32) | ((b1 as i32) << 8) | ((b2 as i32) << 16);
                if v >= 1 << 23 {
                    v -= 1 << 24;
                }
                int_to_f32(v as i64, 24)
            }
            SampleFormat::Int32 => {
                let b = [bytes[0], bytes[1], bytes[2], bytes[3]];
                let v = if le {
                    i32::from_le_bytes(b)
                } else {
                    i32::from_be_bytes(b)
                };
                int_to_f32(v as i64, 32)
            }
            SampleFormat::Float32 => {
                let b = [bytes[0], bytes[1], bytes[2], bytes[3]];
                if le {
                    f32::from_le_bytes(b)
                } else {
                    f32::from_be_bytes(b)
                }
            }
            SampleFormat::Float64 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&bytes[..8]);
                if le {
                    f64::from_le_bytes(b) as f32
                } else {
                    f64::from_be_bytes(b) as f32
                }
            }
        }
    }
}

impl Decoder for PcmDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, frame: u64) -> Result<(), CadenceError> {
        let byte = frame.checked_mul(self.format.block_align() as u64).ok_or(
            CadenceError::SeekOutOfRange {
                requested: frame,
                total: u64::MAX,
            },
        )?;
        self.source.seek_to(byte)?;
        self.source.set_limit(None);
        Ok(())
    }

    fn decode(&mut self, buffer: &mut [f32]) -> Result<usize, CadenceError> {
        let channels = self.format.channels as usize;
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
                    let bps = self.format.sample_format.bytes_per_sample() as usize;
                    let out = &mut buffer[frames * channels..(frames + 1) * channels];
                    for (ch, slot) in out.iter_mut().enumerate() {
                        *slot = self.convert_sample(&self.frame[ch * bps..(ch + 1) * bps]);
                    }
                    frames += 1;
                }
                Err(CadenceError::EndOfStream) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(frames)
    }
}

/// Reader wrapper mirroring [`tpt_av_cadence_core::FormatReader`]'s shape.
///
/// Headerless PCM cannot implement [`tpt_av_cadence_core::FormatReader`] itself
/// because `open` takes no format parameters — use
/// [`PcmReader::open_with_format`].
pub struct PcmReader {
    decoder: PcmDecoder,
}

impl PcmReader {
    /// Opens a raw PCM stream, supplying the parameters the header omits.
    pub fn open_with_format(
        source: Box<dyn Read + Send>,
        format: PcmFormat,
    ) -> Result<Self, CadenceError> {
        Ok(PcmReader {
            decoder: PcmDecoder::open(source, format)?,
        })
    }

    /// Returns the underlying decoder.
    pub fn decoder(&mut self) -> &mut dyn Decoder {
        &mut self.decoder
    }

    /// Returns stream metadata.
    pub fn info(&self) -> &StreamInfo {
        &self.decoder.info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn decode_all(mut dec: PcmDecoder, cap_frames: usize) -> Vec<f32> {
        let ch = dec.info().channels as usize;
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; cap_frames * ch];
        loop {
            let frames = dec.decode(&mut buf).expect("decode must not fail");
            if frames == 0 {
                break;
            }
            out.extend_from_slice(&buf[..frames * ch]);
        }
        out
    }

    #[test]
    fn int16_little_endian_mono() {
        let data: Vec<u8> = [0i16, 16384, -16384, -32768, 32767]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let fmt = PcmFormat {
            sample_format: SampleFormat::Int16,
            byte_order: ByteOrder::Little,
            channels: 1,
            sample_rate: 44_100,
        };
        let dec = PcmDecoder::from_source(Box::new(Cursor::new(data)), fmt).unwrap();
        assert_eq!(dec.info().bit_depth, 16);
        let got = decode_all(dec, 4);
        let want = [0.0, 0.5, -0.5, -1.0, 32767.0 / 32768.0];
        assert_eq!(got, want);
    }

    #[test]
    fn int24_big_endian_stereo() {
        // Left channel: 0x400000 (0.5), right: 0xC00000 (-0.5).
        let data = vec![0x40, 0x00, 0x00, 0xC0, 0x00, 0x00];
        let fmt = PcmFormat {
            sample_format: SampleFormat::Int24,
            byte_order: ByteOrder::Big,
            channels: 2,
            sample_rate: 48_000,
        };
        let dec = PcmDecoder::from_source(Box::new(Cursor::new(data)), fmt).unwrap();
        let got = decode_all(dec, 8);
        assert_eq!(got, vec![0.5, -0.5]);
    }

    #[test]
    fn float32_big_endian() {
        let data = 0.25f32.to_be_bytes().to_vec();
        let fmt = PcmFormat {
            sample_format: SampleFormat::Float32,
            byte_order: ByteOrder::Big,
            channels: 1,
            sample_rate: 48_000,
        };
        let dec = PcmDecoder::from_source(Box::new(Cursor::new(data)), fmt).unwrap();
        assert_eq!(decode_all(dec, 4), vec![0.25]);
    }

    #[test]
    fn truncated_frame_is_dropped() {
        // 2.5 frames of int16 mono: the trailing lone byte is dropped.
        let mut data = vec![0u8; 5];
        data[0] = 0x01;
        let fmt = PcmFormat {
            sample_format: SampleFormat::Int16,
            byte_order: ByteOrder::Little,
            channels: 1,
            sample_rate: 8_000,
        };
        let dec = PcmDecoder::from_source(Box::new(Cursor::new(data)), fmt).unwrap();
        assert_eq!(decode_all(dec, 16).len(), 2);
    }

    #[test]
    fn seek_then_decode() {
        let data: Vec<u8> = (0i16..16).flat_map(|v| v.to_le_bytes()).collect();
        let fmt = PcmFormat {
            sample_format: SampleFormat::Int16,
            byte_order: ByteOrder::Little,
            channels: 1,
            sample_rate: 8_000,
        };
        let mut dec = PcmDecoder::from_source(Box::new(Cursor::new(data)), fmt).unwrap();
        dec.seek(10).unwrap();
        let mut buf = [0.0f32; 2];
        assert_eq!(dec.decode(&mut buf).unwrap(), 2);
        assert_eq!(buf[0], 10.0 / 32768.0);
        assert_eq!(buf[1], 11.0 / 32768.0);
    }

    #[test]
    fn buffer_not_multiple_of_channels_is_error() {
        let fmt = PcmFormat {
            sample_format: SampleFormat::Int16,
            byte_order: ByteOrder::Little,
            channels: 2,
            sample_rate: 8_000,
        };
        let mut dec = PcmDecoder::from_source(Box::new(Cursor::new(vec![0u8; 64])), fmt).unwrap();
        let mut buf = [0.0f32; 3]; // odd length, 2 channels
        assert!(dec.decode(&mut buf).is_err());
    }

    #[test]
    fn empty_stream_decodes_zero_frames() {
        let fmt = PcmFormat {
            sample_format: SampleFormat::Int16,
            byte_order: ByteOrder::Little,
            channels: 1,
            sample_rate: 8_000,
        };
        let mut dec = PcmDecoder::from_source(Box::new(Cursor::new(Vec::new())), fmt).unwrap();
        let mut buf = [0.0f32; 8];
        assert_eq!(dec.decode(&mut buf).unwrap(), 0);
    }

    #[test]
    fn zero_channel_format_rejected() {
        let fmt = PcmFormat {
            sample_format: SampleFormat::Int16,
            byte_order: ByteOrder::Little,
            channels: 0,
            sample_rate: 8_000,
        };
        assert!(PcmDecoder::from_source(Box::new(Cursor::new(Vec::new())), fmt).is_err());
    }
}
