//! # tpt-av-cadence-core
//!
//! Core traits and types shared by every crate in the `tpt-cadence` audio codec
//! suite: the unified [`Decoder`] trait, [`StreamInfo`] metadata, format enums,
//! and error handling.
//!
//! ## The `Decoder` trait
//!
//! Every format decoder implements [`Decoder`]. This is the single most
//! important interface in the suite — the `tpt-audio` engine calls it during
//! playback.
//!
//! ### Real-Time Safety Contract
//!
//! After `init()` completes, the [`Decoder::decode`] method MUST be:
//!
//! - **Allocation-free** (no heap allocations)
//! - **Lock-free** (no mutexes, no atomics with contention)
//! - **Panic-free** (returns [`Result`], never unwraps)
//!
//! All memory required for decoding (Huffman tables, MDCT windows, LPC
//! coefficients, …) MUST be allocated during initialization.
//!
//! ## The `FormatReader` trait
//!
//! For formats that read directly from files (WAV, AIFF, FLAC), [`FormatReader`]
//! handles file I/O and header parsing on background threads. It hands out a
//! `&mut dyn Decoder` for real-time PCM extraction.

mod decoder;
mod encoder;
mod error;
mod format;
mod sample;
mod source;
mod stream_info;

pub use decoder::{Decoder, FormatReader};
pub use encoder::Encoder;
pub use error::{CadenceError, Result};
pub use format::{ChannelLayout, Format, SampleFormat};
pub use sample::{f32_to_int, int_to_f32};
pub use source::{BufferedSource, ByteSource, Unseekable};
pub use stream_info::StreamInfo;
