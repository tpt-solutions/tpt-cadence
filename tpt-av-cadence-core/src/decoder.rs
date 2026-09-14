//! The unified `Decoder` and `FormatReader` traits.

use crate::error::Result;
use crate::stream_info::StreamInfo;
use std::io::Read;

/// The core trait every audio codec decoder implements.
///
/// This is the single most important interface in the entire crate. The
/// `tpt-audio` engine calls it during playback.
///
/// # Real-Time Safety Contract
///
/// After `init()` completes, the [`Decoder::decode`] method MUST be:
/// - Allocation-free (no heap allocations)
/// - Lock-free (no mutexes, no atomics with contention)
/// - Panic-free (returns `Result`, never unwraps)
///
/// All memory required for decoding (Huffman tables, MDCT windows,
/// LPC coefficients, etc.) MUST be allocated during initialization.
///
/// # File-backed decoders
///
/// Decoders for file formats (WAV, AIFF, FLAC) pull bytes from an internal
/// buffered source. When that buffer is exhausted mid-decode they may perform
/// a read from the underlying medium, which can block; drive such decoders
/// from a background thread (see the two-thread pipeline in `DESIGN.md` §6).
/// Once data is buffered, the decode loop itself is allocation-free,
/// lock-free, and panic-free.
pub trait Decoder: Send {
    /// Returns immutable stream metadata. Allocation-free.
    fn info(&self) -> &StreamInfo;

    /// Seeks to an exact sample frame position.
    ///
    /// This method MAY allocate and MAY block (e.g., reading from disk).
    /// It is NOT real-time safe. Call it only from background threads.
    ///
    /// Decoders backed by unseekable sources return
    /// [`CadenceError::UnsupportedFeature`].
    fn seek(&mut self, frame: u64) -> Result<()>;

    /// Decodes the next chunk of audio into the provided buffer.
    ///
    /// Samples are written as interleaved `f32` values in the range
    /// `[-1.0, 1.0]`. Returns the number of **frames** written (not samples).
    ///
    /// # Real-Time Safety
    ///
    /// This method is guaranteed to be allocation-free, lock-free, and
    /// panic-free after initialization. It is safe to call from the
    /// audio callback thread (for packet-fed decoders) or a background
    /// decoding thread.
    ///
    /// # Arguments
    ///
    /// * `buffer` - A caller-provided slice to write interleaved f32 samples
    ///   into. The slice length must be a multiple of `info().channels`.
    ///
    /// # Returns
    ///
    /// * `Ok(frames_written)` - Number of frames decoded.
    /// * `Ok(0)` - End of stream reached.
    /// * `Err(CadenceError)` - Decoding error (corrupt data, etc.).
    fn decode(&mut self, buffer: &mut [f32]) -> Result<usize>;
}

/// Reads audio data from a byte source (file, memory, network).
///
/// Unlike [`Decoder`], this trait handles I/O and header parsing and MAY
/// allocate. It is intended for use on background threads, not the audio
/// thread.
pub trait FormatReader: Send {
    /// Opens and parses the audio source.
    fn open(source: Box<dyn Read + Send>) -> Result<Self>
    where
        Self: Sized;

    /// Returns the underlying [`Decoder`] for real-time PCM extraction.
    fn decoder(&mut self) -> &mut dyn Decoder;

    /// Returns stream metadata.
    fn info(&self) -> &StreamInfo;
}
