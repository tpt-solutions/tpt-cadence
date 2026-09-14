//! # tpt-av-cadence-aiff
//!
//! AIFF / AIFF-C parser and decoder (big-endian IFF chunk format).
//!
//! Critical for macOS/Logic Pro users. Supports the classic AIFF encodings
//! (signed 8/16/24/32-bit big-endian PCM) and the AIFF-C compression types
//! `NONE`, `twos` (big-endian PCM), `sowt` (little-endian PCM), `FL32`,
//! `in24`, and `ni24`. Compressed types such as `ima4`, `ULAW`, and `ALAW`
//! are rejected with [`tpt_av_cadence_core::CadenceError::UnsupportedFeature`].
//!
//! # Example
//!
//! ```no_run
//! use std::fs::File;
//! use tpt_av_cadence_core::{Decoder, FormatReader};
//! use tpt_av_cadence_aiff::AiffReader;
//!
//! # fn main() -> Result<(), tpt_av_cadence_core::CadenceError> {
//! let mut reader = AiffReader::open(Box::new(File::open("track.aif")?))?;
//! let channels = reader.info().channels as usize;
//! let mut buf = vec![0.0f32; 4096 * channels];
//! loop {
//!     let frames = reader.decoder().decode(&mut buf)?;
//!     if frames == 0 { break; }
//! }
//! # Ok(())
//! # }
//! ```

pub mod decoder;
pub mod ext_float;
pub mod reader;

pub use decoder::{AiffDecoder, AiffReader};
