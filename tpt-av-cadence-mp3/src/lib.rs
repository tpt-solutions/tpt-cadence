//! # tpt-av-cadence-mp3
//!
//! MPEG-1/2/2.5 audio Layer III (MP3) decoder, written from ISO/IEC 11172-3
//! and ISO/IEC 13818-3.
//!
//! Supports the full Layer III model: Huffman decoding with the 32 standard
//! codebooks (linbits escapes, count1 tables), scalefactors with scfsi
//! bit-reservoir sharing and the MPEG-2/2.5 low-sample-rate partition tables,
//! mid/side and intensity stereo, requantization, short-block reordering,
//! alias reduction, 36/12-point IMDCT with the four window sequences, and the
//! 32-band polyphase synthesis filterbank. Frame parsing covers the
//! bit-reservoir (`main_data_begin`) and CRC-16 protection; a leading ID3v2
//! tag is skipped.
//!
//! Conformance is validated against the bundled mpg123-derived streams
//! (`tests/data/`) by comparing decoded PCM against reference float output
//! and the original source WAVs.
//!
//! # Example
//!
//! ```no_run
//! use std::fs::File;
//! use tpt_av_cadence_core::{Decoder, FormatReader};
//! use tpt_av_cadence_mp3::Mp3Reader;
//!
//! # fn main() -> Result<(), tpt_av_cadence_core::CadenceError> {
//! let mut reader = Mp3Reader::open(Box::new(File::open("song.mp3")?))?;
//! let channels = reader.info().channels as usize;
//! let mut buf = vec![0.0f32; 4096 * channels];
//! loop {
//!     let frames = reader.decoder().decode(&mut buf)?;
//!     if frames == 0 { break; }
//! }
//! # Ok(())
//! # }
//! ```

pub mod bitreader;
pub mod decoder;
mod header;
pub mod huffman;
pub mod imdct;
pub mod processing;
pub mod scalefac;
pub mod sideinfo;
pub mod stereo;
pub mod synth;
mod tables;

pub use decoder::{Mp3Decoder, Mp3Reader};
