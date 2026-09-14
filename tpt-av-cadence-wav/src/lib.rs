//! # tpt-av-cadence-wav
//!
//! RIFF/WAVE parser and PCM/IEEE-float decoder.
//!
//! Supports 8/16/24/32-bit integer PCM and 32/64-bit IEEE float, mono or any
//! channel count, plain and `WAVE_FORMAT_EXTENSIBLE` headers, and tolerates
//! unknown/odd-sized chunks anywhere in the file. RIFX (big-endian RIFF) is
//! rejected with [`CadenceError::UnsupportedFeature`].
//!
//! # Example
//!
//! ```no_run
//! use std::fs::File;
//! use tpt_av_cadence_core::{Decoder, FormatReader};
//! use tpt_av_cadence_wav::WavReader;
//!
//! # fn main() -> Result<(), tpt_av_cadence_core::CadenceError> {
//! let mut reader = WavReader::open(Box::new(File::open("clip.wav")?))?;
//! let channels = reader.info().channels as usize;
//!
//! let mut buf = vec![0.0f32; 4096 * channels];
//! loop {
//!     let frames = reader.decoder().decode(&mut buf)?;
//!     if frames == 0 { break; }
//! }
//! # Ok(())
//! # }
//! ```

pub mod decoder;
pub mod reader;

pub use decoder::{WavDecoder, WavReader};
