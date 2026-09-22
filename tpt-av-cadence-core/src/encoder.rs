//! The unified `Encoder` trait.
//!
//! Mirrors [`crate::Decoder`] on the write side: construction does all
//! allocation and header setup, `encode` is the steady-state hot loop, and
//! `finish` performs any blocking/allocating finalization (e.g. seeking back
//! to patch a RIFF/IFF header with the final size once it's known).

use crate::error::Result;

/// The write-side counterpart to [`crate::Decoder`], implemented by every
/// format writer in the suite (WAV, AIFF, headerless PCM, and future
/// compressed encoders).
///
/// # Real-Time Safety Contract
///
/// After construction, [`Encoder::encode`] SHOULD be allocation-free,
/// lock-free, and panic-free (return `Result` instead of panicking on
/// out-of-range input) wherever the underlying format allows it — the same
/// discipline [`crate::Decoder::decode`] requires. [`Encoder::finish`] MAY
/// allocate and MAY block (e.g. seeking a file to patch a header), just like
/// [`crate::Decoder::seek`].
pub trait Encoder: Send {
    /// Encodes interleaved `f32` input samples in `[-1.0, 1.0]`, returning
    /// how many frames (not samples) were consumed from `samples`.
    ///
    /// `samples.len()` must be a multiple of the stream's channel count.
    fn encode(&mut self, samples: &[f32]) -> Result<usize>;

    /// Flushes internal state and finalizes the encoded stream (e.g.
    /// patching chunk-size fields that could only be known once every
    /// sample was written). Idempotent: calling it more than once is a no-op.
    fn finish(&mut self) -> Result<()>;
}
