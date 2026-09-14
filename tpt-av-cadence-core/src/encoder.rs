//! The unified `Encoder` trait (future work).
//!
//! tpt-cadence currently ships decoders only. This module reserves the
//! encoder API surface; the trait below is a draft and may change without a
//! major version bump while the project is pre-1.0.

use crate::error::Result;

/// Draft encoder trait. **Unstable — do not implement yet.**
///
/// Mirrors [`crate::Decoder`]: allocation and blocking work happen in
/// construction/finish; per-call `encode` is intended to follow the same
/// real-time discipline as `decode` once designs settle.
pub trait Encoder: Send {
    /// Encodes interleaved `f32` input samples, returning how many frames
    /// were consumed from `samples`.
    fn encode(&mut self, samples: &[f32]) -> Result<usize>;

    /// Flushes internal state and finalizes the encoded stream.
    fn finish(&mut self) -> Result<()>;
}
