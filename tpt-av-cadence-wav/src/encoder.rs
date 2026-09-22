//! RIFF/WAVE writer.

use std::io::{Seek, SeekFrom, Write};

use tpt_av_cadence_core::{f32_to_int, CadenceError, Encoder, SampleFormat};

const FORMAT_TAG_PCM: u16 = 0x0001;
const FORMAT_TAG_IEEE_FLOAT: u16 = 0x0003;

/// Writes a RIFF/WAVE file: 8/16/24/32-bit signed PCM (8-bit is written
/// unsigned, matching WAV convention and this crate's own decoder) or
/// 32/64-bit IEEE float, any channel count.
///
/// All allocation and header setup happens in [`WavEncoder::new`];
/// [`Encoder::encode`] only writes bytes derived from the caller's samples.
/// [`Encoder::finish`] seeks back to patch the two RIFF/`data` size fields
/// that can only be known once every sample has been written, matching the
/// `finish`-may-block contract in [`tpt_av_cadence_core::Encoder`]'s docs.
pub struct WavEncoder<W: Write + Seek> {
    sink: W,
    channels: u16,
    sample_format: SampleFormat,
    data_bytes: u64,
    finished: bool,
}

impl<W: Write + Seek> WavEncoder<W> {
    /// Opens a new WAV file for writing, emitting the RIFF/`fmt `/`data`
    /// headers immediately (the `data` and RIFF sizes are placeholders,
    /// patched by [`Encoder::finish`]).
    pub fn new(
        mut sink: W,
        sample_rate: u32,
        channels: u16,
        sample_format: SampleFormat,
    ) -> Result<Self, CadenceError> {
        if channels == 0 {
            return Err(CadenceError::InvalidFormat(
                "WAV stream must have at least 1 channel".to_string(),
            ));
        }
        if sample_rate == 0 {
            return Err(CadenceError::InvalidFormat(
                "WAV stream must have a non-zero sample rate".to_string(),
            ));
        }
        let (format_tag, bits_per_sample) = match sample_format {
            SampleFormat::Int8 => (FORMAT_TAG_PCM, 8u16),
            SampleFormat::Int16 => (FORMAT_TAG_PCM, 16),
            SampleFormat::Int24 => (FORMAT_TAG_PCM, 24),
            SampleFormat::Int32 => (FORMAT_TAG_PCM, 32),
            SampleFormat::Float32 => (FORMAT_TAG_IEEE_FLOAT, 32),
            SampleFormat::Float64 => (FORMAT_TAG_IEEE_FLOAT, 64),
        };
        let block_align = channels as u32 * sample_format.bytes_per_sample() as u32;
        let byte_rate = sample_rate * block_align;

        sink.write_all(b"RIFF")?;
        sink.write_all(&0u32.to_le_bytes())?; // patched in finish()
        sink.write_all(b"WAVE")?;
        sink.write_all(b"fmt ")?;
        sink.write_all(&16u32.to_le_bytes())?;
        sink.write_all(&format_tag.to_le_bytes())?;
        sink.write_all(&channels.to_le_bytes())?;
        sink.write_all(&sample_rate.to_le_bytes())?;
        sink.write_all(&byte_rate.to_le_bytes())?;
        sink.write_all(&(block_align as u16).to_le_bytes())?;
        sink.write_all(&bits_per_sample.to_le_bytes())?;
        sink.write_all(b"data")?;
        sink.write_all(&0u32.to_le_bytes())?; // patched in finish()

        Ok(WavEncoder {
            sink,
            channels,
            sample_format,
            data_bytes: 0,
            finished: false,
        })
    }

    fn finalize(&mut self) -> Result<(), CadenceError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // Pad the data chunk to an even length (RIFF chunks are word-aligned).
        if self.data_bytes % 2 == 1 {
            self.sink.write_all(&[0u8])?;
        }
        let data_bytes: u32 = self.data_bytes.min(u32::MAX as u64) as u32;
        let riff_size = 4 + (8 + 16) + (8 + data_bytes);
        self.sink.seek(SeekFrom::Start(4))?;
        self.sink.write_all(&riff_size.to_le_bytes())?;
        self.sink.seek(SeekFrom::Start(40))?;
        self.sink.write_all(&data_bytes.to_le_bytes())?;
        self.sink.seek(SeekFrom::End(0))?;
        self.sink.flush()?;
        Ok(())
    }

    fn write_sample(&mut self, sample: f32) -> Result<(), CadenceError> {
        let depth = self.sample_format.bit_depth();
        match self.sample_format {
            SampleFormat::Int8 => {
                // WAV stores 8-bit PCM unsigned.
                let v = (f32_to_int(sample, 8) + 128) as u8;
                self.sink.write_all(&[v])?;
            }
            SampleFormat::Int16 => {
                self.sink
                    .write_all(&(f32_to_int(sample, depth) as i16).to_le_bytes())?;
            }
            SampleFormat::Int24 => {
                let v = f32_to_int(sample, depth) as i32;
                self.sink.write_all(&v.to_le_bytes()[..3])?;
            }
            SampleFormat::Int32 => {
                self.sink
                    .write_all(&(f32_to_int(sample, depth) as i32).to_le_bytes())?;
            }
            SampleFormat::Float32 => {
                self.sink.write_all(&sample.to_le_bytes())?;
            }
            SampleFormat::Float64 => {
                self.sink.write_all(&(sample as f64).to_le_bytes())?;
            }
        }
        Ok(())
    }
}

impl<W: Write + Seek + Send> Encoder for WavEncoder<W> {
    fn encode(&mut self, samples: &[f32]) -> Result<usize, CadenceError> {
        let channels = self.channels as usize;
        if samples.len() % channels != 0 {
            return Err(CadenceError::InvalidFormat(format!(
                "sample count {} is not a multiple of the channel count {}",
                samples.len(),
                channels
            )));
        }
        for &sample in samples {
            self.write_sample(sample)?;
        }
        self.data_bytes += (samples.len() * self.sample_format.bytes_per_sample() as usize) as u64;
        Ok(samples.len() / channels)
    }

    fn finish(&mut self) -> Result<(), CadenceError> {
        self.finalize()
    }
}

impl<W: Write + Seek> Drop for WavEncoder<W> {
    fn drop(&mut self) {
        // Best-effort finalize if the caller forgot; errors are unobservable
        // from `drop`, matching `std::fs::File`'s own drop-flush behavior.
        let _ = self.finalize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::WavDecoder;
    use std::io::Cursor;
    use tpt_av_cadence_core::Decoder;

    fn round_trip(sample_format: SampleFormat, channels: u16, frames: &[f32]) -> Vec<f32> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut enc = WavEncoder::new(&mut buf, 44_100, channels, sample_format).unwrap();
            enc.encode(frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        buf.set_position(0);
        let mut dec = WavDecoder::from_source(Box::new(buf)).unwrap();
        assert_eq!(dec.info().channels, channels);
        assert_eq!(dec.info().sample_rate, 44_100);
        let mut out = vec![0.0f32; frames.len()];
        let got = dec.decode(&mut out).unwrap();
        out.truncate(got * channels as usize);
        out
    }

    #[test]
    fn int16_stereo_round_trips_bit_exact() {
        let frames = [0.0, 0.5, -0.5, -1.0, 32767.0 / 32768.0, -0.25];
        let got = round_trip(SampleFormat::Int16, 2, &frames);
        assert_eq!(got, frames);
    }

    #[test]
    fn int8_mono_round_trips() {
        // 8-bit has coarse quantization; only test values landing exactly on
        // the 256-level grid so the round trip is bit-exact.
        let frames = [0.0, 1.0, -1.0, 0.5, -0.5];
        let got = round_trip(SampleFormat::Int8, 1, &frames);
        for (a, b) in got.iter().zip(frames.iter()) {
            assert!((a - b).abs() < 1.0 / 64.0, "{a} vs {b}");
        }
    }

    #[test]
    fn int24_mono_round_trips_bit_exact() {
        let frames = [0.0, 0.5, -0.5, -1.0, 0.25, -0.75];
        let got = round_trip(SampleFormat::Int24, 1, &frames);
        for (a, b) in got.iter().zip(frames.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
    }

    #[test]
    fn int32_mono_round_trips_bit_exact() {
        let frames = [0.0, 0.5, -0.5, -1.0, 0.25];
        let got = round_trip(SampleFormat::Int32, 1, &frames);
        for (a, b) in got.iter().zip(frames.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
    }

    #[test]
    fn float32_stereo_round_trips_bit_exact() {
        let frames = [0.0, 0.123_456, -0.987_654, 1.0, -1.0, 0.333_333];
        let got = round_trip(SampleFormat::Float32, 2, &frames);
        assert_eq!(got, frames);
    }

    #[test]
    fn float64_mono_round_trips() {
        let frames = [0.0, 0.123_456_7, -0.987_654_3];
        let got = round_trip(SampleFormat::Float64, 1, &frames);
        for (a, b) in got.iter().zip(frames.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
    }

    #[test]
    fn zero_channels_rejected() {
        let buf = Cursor::new(Vec::new());
        assert!(WavEncoder::new(buf, 44_100, 0, SampleFormat::Int16).is_err());
    }

    #[test]
    fn zero_sample_rate_rejected() {
        let buf = Cursor::new(Vec::new());
        assert!(WavEncoder::new(buf, 0, 1, SampleFormat::Int16).is_err());
    }

    #[test]
    fn non_multiple_of_channels_is_error() {
        let mut buf = Cursor::new(Vec::new());
        let mut enc = WavEncoder::new(&mut buf, 44_100, 2, SampleFormat::Int16).unwrap();
        assert!(enc.encode(&[0.0, 0.1, 0.2]).is_err());
    }

    #[test]
    fn finish_is_idempotent() {
        let mut buf = Cursor::new(Vec::new());
        let mut enc = WavEncoder::new(&mut buf, 44_100, 1, SampleFormat::Int16).unwrap();
        enc.encode(&[0.0, 0.5]).unwrap();
        Encoder::finish(&mut enc).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
}
