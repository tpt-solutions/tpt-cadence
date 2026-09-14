//! The `CadenceError` enum.

use std::io;

/// Result alias used across the `tpt-cadence` crates.
pub type Result<T, E = CadenceError> = std::result::Result<T, E>;

/// Errors produced by format readers and decoders.
#[derive(Debug, thiserror::Error)]
pub enum CadenceError {
    /// The file or stream is not a valid instance of this format.
    #[error("invalid format: {0}")]
    InvalidFormat(String),

    /// The format is recognized but uses unsupported features.
    #[error("unsupported feature: {0}")]
    UnsupportedFeature(String),

    /// Corrupt or truncated data encountered during decoding.
    #[error("corrupt data: {0}")]
    CorruptData(String),

    /// An I/O error occurred (only from `FormatReader`, never from
    /// `Decoder::decode` unless the decoder's internal buffered source hit a
    /// real I/O failure mid-stream).
    #[error("I/O error: {0}")]
    IoError(#[from] io::Error),

    /// The seek position is out of range.
    #[error("seek position {requested} out of range (stream has {total} frames)")]
    SeekOutOfRange { requested: u64, total: u64 },

    /// The caller-provided decode buffer could not hold the smallest unit of
    /// audio this decoder produces (e.g. one block for block-based formats).
    #[error("buffer too small: need at least {needed} samples, got {provided}")]
    BufferTooSmall { needed: usize, provided: usize },

    /// End of stream.
    #[error("end of stream")]
    EndOfStream,
}
