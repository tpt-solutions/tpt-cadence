//! # tpt-av-cadence-flac
//!
//! FLAC (Free Lossless Audio Codec) decoder, written from RFC 9639.
//!
//! Supports the full frame/subframe model: Constant, Verbatim, Fixed
//! (orders 0–4), and LPC (orders 1–32) subframes, partitioned Rice and
//! Rice2 residuals with escaped partitions, wasted-bit removal, stereo
//! decorrelation (left/side, right/side, mid/side), fixed and variable
//! block sizes, CRC-8/CRC-16 integrity checking, and STREAMINFO metadata.
//!
//! Conformance is validated against the official IETF FLAC decoder
//! testbench vectors (CC0, bundled under `tests/data/`) by comparing the
//! decoded PCM's MD5 against the checksum embedded in each stream's
//! STREAMINFO block.
//!
//! # Example
//!
//! ```no_run
//! use std::fs::File;
//! use tpt_av_cadence_core::{Decoder, FormatReader};
//! use tpt_av_cadence_flac::FlacReader;
//!
//! # fn main() -> Result<(), tpt_av_cadence_core::CadenceError> {
//! let mut reader = FlacReader::open(Box::new(File::open("album.flac")?))?;
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
pub mod lpc;
pub mod rice;
pub mod stream;
pub mod subframe;

pub use decoder::{FlacDecoder, FlacReader};
pub use stream::StreamInfo as FlacStreamInfo;
