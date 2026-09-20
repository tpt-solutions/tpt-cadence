//! # tpt-av-cadence-aac
//!
//! AAC-LC (Advanced Audio Coding, Low Complexity) decoder, implemented
//! from ISO/IEC 14496-3 for the `tpt-cadence` suite.
//!
//! Supports ADTS-framed streams (auto-detected) and raw streams with an
//! explicit [`audio_specific::AudioSpecificConfig`] (e.g. from an MP4
//! `esds`), mono through 7.1 channel configurations, and the full LC tool
//! set: Huffman spectral coding with escapes, scalefactor prediction
//! deltas, Temporal Noise Shaping, Perceptual Noise Substitution, and
//! M/S + intensity stereo.
//!
//! Constant tables (Huffman codebooks, scalefactor-band offsets, TNS
//! limits) are normative data of the ISO standard; see `tables.rs`.
//!
//! # Example
//!
//! ```no_run
//! use std::fs::File;
//! use tpt_av_cadence_core::{Decoder};
//! use tpt_av_cadence_aac::AacDecoder;
//!
//! # fn main() -> Result<(), tpt_av_cadence_core::CadenceError> {
//! let file = File::open("song.aac")?;
//! let mut decoder = AacDecoder::from_source(Box::new(file))?;
//! let channels = decoder.info().channels as usize;
//! let mut buf = vec![0.0f32; 1024 * channels];
//! loop {
//!     let frames = decoder.decode(&mut buf)?;
//!     if frames == 0 { break; }
//! }
//! # Ok(())
//! # }
//! ```

pub mod adts;
pub mod audio_specific;
pub mod bitreader;
pub mod decoder;
pub mod huffman;
pub mod imdct;
pub mod pns;
pub mod sbr;
pub mod stereo;
pub mod tables;
pub mod tns;

pub use adts::SAMPLING_FREQUENCIES;
pub use audio_specific::AudioSpecificConfig;
pub use decoder::{AacDecoder, AacReader};

/// Re-exported error type used by the module APIs.
pub use tpt_av_cadence_core::{CadenceError, Result};
