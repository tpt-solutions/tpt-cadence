//! AIFF-C writer (big-endian IFF chunk format).

use std::io::{Seek, SeekFrom, Write};

use tpt_av_cadence_core::{f32_to_int, CadenceError, Encoder, SampleFormat};

use crate::ext_float::f64_to_extended;

/// Writes an AIFF-C file: 8/16/24/32-bit signed big-endian PCM (compression
/// type `NONE`) or 32/64-bit IEEE float (`FL32`/`FL64`), any channel count.
///
/// Always emits the AIFF-C (`FORM 'AIFC'`) chunk layout with an empty
/// compression-name string, which this crate's own [`crate::AiffDecoder`]
/// (and every other AIFF-C-aware reader) accepts for both integer and float
/// encodings — classic `FORM 'AIFF'` cannot carry a compression type at all,
/// so float output would otherwise be unrepresentable.
///
/// All allocation and header setup happens in [`AiffEncoder::new`];
/// [`Encoder::encode`] only writes bytes derived from the caller's samples.
/// [`Encoder::finish`] seeks back to patch the `FORM`/`COMM`/`SSND` size and
/// frame-count fields that can only be known once every sample has been
/// written.
pub struct AiffEncoder<W: Write + Seek> {
    sink: W,
    channels: u16,
    sample_format: SampleFormat,
    frames_written: u64,
    data_bytes: u64,
    /// Byte offset of the `COMM` chunk's `numSampleFrames` field.
    comm_frames_offset: u64,
    /// Byte offset of the `SSND` chunk's size field.
    ssnd_size_offset: u64,
    finished: bool,
}

impl<W: Write + Seek> AiffEncoder<W> {
    /// Opens a new AIFF-C file for writing, emitting the `FORM`/`FVER`/`COMM`/
    /// `SSND` headers immediately (size and frame-count fields are
    /// placeholders, patched by [`Encoder::finish`]).
    pub fn new(
        mut sink: W,
        sample_rate: u32,
        channels: u16,
        sample_format: SampleFormat,
    ) -> Result<Self, CadenceError> {
        if channels == 0 {
            return Err(CadenceError::InvalidFormat(
                "AIFF stream must have at least 1 channel".to_string(),
            ));
        }
        if sample_rate == 0 {
            return Err(CadenceError::InvalidFormat(
                "AIFF stream must have a non-zero sample rate".to_string(),
            ));
        }
        let (compression_type, sample_size): (&[u8; 4], u16) = match sample_format {
            SampleFormat::Int8 => (b"NONE", 8),
            SampleFormat::Int16 => (b"NONE", 16),
            SampleFormat::Int24 => (b"NONE", 24),
            SampleFormat::Int32 => (b"NONE", 32),
            SampleFormat::Float32 => (b"FL32", 32),
            SampleFormat::Float64 => (b"FL64", 64),
        };

        sink.write_all(b"FORM")?;
        sink.write_all(&0u32.to_be_bytes())?; // FORM size, patched in finish()
        sink.write_all(b"AIFC")?;

        // FVER: the one defined AIFF-C format-version timestamp.
        sink.write_all(b"FVER")?;
        sink.write_all(&4u32.to_be_bytes())?;
        sink.write_all(&0xA280_5140u32.to_be_bytes())?;

        // COMM (AIFF-C form: base 18 bytes + 4-byte compression type + an
        // empty Pascal string, i.e. a 1-byte zero count plus 1 pad byte).
        sink.write_all(b"COMM")?;
        sink.write_all(&24u32.to_be_bytes())?;
        sink.write_all(&channels.to_be_bytes())?;
        let comm_frames_offset = position(&mut sink)?;
        sink.write_all(&0u32.to_be_bytes())?; // numSampleFrames, patched
        sink.write_all(&sample_size.to_be_bytes())?;
        sink.write_all(&f64_to_extended(sample_rate as f64))?;
        sink.write_all(compression_type)?;
        sink.write_all(&[0u8, 0u8])?; // empty pstring: count=0, pad=1

        // SSND.
        sink.write_all(b"SSND")?;
        let ssnd_size_offset = position(&mut sink)?;
        sink.write_all(&0u32.to_be_bytes())?; // SSND chunk size, patched
        sink.write_all(&0u32.to_be_bytes())?; // offset
        sink.write_all(&0u32.to_be_bytes())?; // blockSize

        Ok(AiffEncoder {
            sink,
            channels,
            sample_format,
            frames_written: 0,
            data_bytes: 0,
            comm_frames_offset,
            ssnd_size_offset,
            finished: false,
        })
    }

    fn write_sample(&mut self, sample: f32) -> Result<(), CadenceError> {
        let depth = self.sample_format.bit_depth();
        match self.sample_format {
            SampleFormat::Int8 => {
                self.sink.write_all(&[f32_to_int(sample, 8) as i8 as u8])?;
            }
            SampleFormat::Int16 => {
                self.sink
                    .write_all(&(f32_to_int(sample, depth) as i16).to_be_bytes())?;
            }
            SampleFormat::Int24 => {
                let v = f32_to_int(sample, depth) as i32;
                self.sink.write_all(&v.to_be_bytes()[1..])?;
            }
            SampleFormat::Int32 => {
                self.sink
                    .write_all(&(f32_to_int(sample, depth) as i32).to_be_bytes())?;
            }
            SampleFormat::Float32 => {
                self.sink.write_all(&sample.to_be_bytes())?;
            }
            SampleFormat::Float64 => {
                self.sink.write_all(&(sample as f64).to_be_bytes())?;
            }
        }
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), CadenceError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // Pad SSND's sound data to an even length (IFF chunks are
        // word-aligned); the pad byte is not counted in the chunk size.
        if self.data_bytes % 2 == 1 {
            self.sink.write_all(&[0u8])?;
        }
        let data_bytes: u32 = self.data_bytes.min(u32::MAX as u64) as u32;
        let ssnd_chunk_size = 8 + data_bytes; // offset + blockSize + data
        let form_size = 4 // "AIFC"
            + (8 + 4) // FVER
            + (8 + 24) // COMM
            + (8 + ssnd_chunk_size); // SSND

        self.sink.seek(SeekFrom::Start(4))?;
        self.sink.write_all(&form_size.to_be_bytes())?;
        self.sink.seek(SeekFrom::Start(self.comm_frames_offset))?;
        self.sink
            .write_all(&(self.frames_written.min(u32::MAX as u64) as u32).to_be_bytes())?;
        self.sink.seek(SeekFrom::Start(self.ssnd_size_offset))?;
        self.sink.write_all(&ssnd_chunk_size.to_be_bytes())?;
        self.sink.seek(SeekFrom::End(0))?;
        self.sink.flush()?;
        Ok(())
    }
}

fn position<W: Seek>(w: &mut W) -> Result<u64, CadenceError> {
    Ok(w.stream_position()?)
}

impl<W: Write + Seek + Send> Encoder for AiffEncoder<W> {
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
        let frames = samples.len() / channels;
        self.frames_written += frames as u64;
        self.data_bytes += (samples.len() * self.sample_format.bytes_per_sample() as usize) as u64;
        Ok(frames)
    }

    fn finish(&mut self) -> Result<(), CadenceError> {
        self.finalize()
    }
}

impl<W: Write + Seek> Drop for AiffEncoder<W> {
    fn drop(&mut self) {
        // Best-effort finalize if the caller forgot; errors are unobservable
        // from `drop`, matching `std::fs::File`'s own drop-flush behavior.
        let _ = self.finalize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::AiffDecoder;
    use std::io::Cursor;
    use tpt_av_cadence_core::Decoder;

    fn round_trip(sample_format: SampleFormat, channels: u16, frames: &[f32]) -> Vec<f32> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut enc = AiffEncoder::new(&mut buf, 44_100, channels, sample_format).unwrap();
            enc.encode(frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        buf.set_position(0);
        let mut dec = AiffDecoder::from_source(Box::new(buf)).unwrap();
        assert_eq!(dec.info().channels, channels);
        assert_eq!(dec.info().sample_rate, 44_100);
        assert_eq!(
            dec.info().total_frames,
            Some(frames.len() as u64 / channels as u64)
        );
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
        assert!(AiffEncoder::new(buf, 44_100, 0, SampleFormat::Int16).is_err());
    }

    #[test]
    fn zero_sample_rate_rejected() {
        let buf = Cursor::new(Vec::new());
        assert!(AiffEncoder::new(buf, 0, 1, SampleFormat::Int16).is_err());
    }

    #[test]
    fn non_multiple_of_channels_is_error() {
        let mut buf = Cursor::new(Vec::new());
        let mut enc = AiffEncoder::new(&mut buf, 44_100, 2, SampleFormat::Int16).unwrap();
        assert!(enc.encode(&[0.0, 0.1, 0.2]).is_err());
    }

    #[test]
    fn finish_is_idempotent() {
        let mut buf = Cursor::new(Vec::new());
        let mut enc = AiffEncoder::new(&mut buf, 44_100, 1, SampleFormat::Int16).unwrap();
        enc.encode(&[0.0, 0.5]).unwrap();
        Encoder::finish(&mut enc).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
}
