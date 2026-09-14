//! Object-safe byte-source plumbing for file-backed decoders.
//!
//! Decoders are concrete (non-generic) types — required by
//! [`FormatReader::decoder`] returning `&mut dyn Decoder` — so they read from
//! a boxed [`ByteSource`]. A `ByteSource` is `Read + Send` plus an optional
//! seek capability, so the same decoder works over files, in-memory cursors,
//! and unseekable streams.

use crate::error::{CadenceError, Result};
use std::io::{self, Read, Seek, SeekFrom};

/// A byte source a format reader/decoder reads from: `Read + Send`, with an
/// optional absolute-seek capability.
pub trait ByteSource: Read + Send {
    /// Seeks to an absolute byte offset. Returns
    /// [`CadenceError::UnsupportedFeature`] if the underlying source cannot seek.
    fn try_seek(&mut self, pos: u64) -> Result<()>;
}

impl<T: Read + Seek + Send> ByteSource for T {
    fn try_seek(&mut self, pos: u64) -> Result<()> {
        Seek::seek(self, SeekFrom::Start(pos))
            .map(|_| ())
            .map_err(CadenceError::from)
    }
}

/// Adapter exposing a [`ByteSource`] over a plain `Box<dyn Read + Send>`.
///
/// Used by [`crate::FormatReader::open`], whose signature only promises `Read`.
/// The wrapped source cannot seek, so decoder `seek()` calls return
/// [`CadenceError::UnsupportedFeature`].
pub struct Unseekable(pub Box<dyn Read + Send>);

impl Read for Unseekable {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl ByteSource for Unseekable {
    fn try_seek(&mut self, _pos: u64) -> Result<()> {
        Err(CadenceError::UnsupportedFeature(
            "source is not seekable (opened via FormatReader::open with a plain Read)".to_string(),
        ))
    }
}

/// Fixed-capacity read buffer over a [`ByteSource`].
///
/// All allocation happens in [`BufferedSource::new`]; the read methods are
/// allocation-free so they may be used from `Decoder::decode`.
///
/// A `limit` bounds the readable region (used to clamp reads to a container's
/// `data` chunk). `consumed` tracks how many bytes have passed through the
/// buffer in total, which lets readers record the absolute offset at which a
/// chunk's payload starts (needed for seeking later).
pub struct BufferedSource {
    inner: Box<dyn ByteSource>,
    buf: Box<[u8]>,
    scratch: Box<[u8]>,
    len: usize,
    pos: usize,
    limit: Option<u64>,
    consumed: u64,
}

impl BufferedSource {
    /// Wraps `inner` with a `capacity`-byte read buffer. Allocates once.
    pub fn new(inner: Box<dyn ByteSource>, capacity: usize) -> Self {
        let capacity = capacity.max(512);
        BufferedSource {
            inner,
            buf: vec![0u8; capacity].into_boxed_slice(),
            scratch: vec![0u8; 4096].into_boxed_slice(),
            len: 0,
            pos: 0,
            limit: None,
            consumed: 0,
        }
    }

    /// Bounds subsequent reads to `Some(n)` bytes (or removes the bound).
    /// Callers set this when entering a bounded region such as a `data` chunk.
    pub fn set_limit(&mut self, limit: Option<u64>) {
        self.limit = limit;
    }

    /// Remaining readable bytes under the current limit, if bounded.
    pub fn remaining(&self) -> Option<u64> {
        self.limit
    }

    /// Total number of bytes served through this buffer since construction
    /// (including skipped bytes). Readers use this to record payload offsets.
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Seeks the underlying source to an absolute byte offset and drops any
    /// buffered data. Does not adjust the limit; callers re-establish it.
    pub fn seek_to(&mut self, pos: u64) -> Result<()> {
        self.inner.try_seek(pos)?;
        self.pos = 0;
        self.len = 0;
        Ok(())
    }

    /// Reads exactly `out.len()` bytes or fails with
    /// [`CadenceError::EndOfStream`]. Allocation-free.
    pub fn take_exact(&mut self, out: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < out.len() {
            match self.serve(&mut out[done..]) {
                Ok(0) => return Err(CadenceError::EndOfStream),
                Ok(n) => done += n,
                Err(e) => return Err(CadenceError::from(e)),
            }
        }
        Ok(())
    }

    /// Consumes and discards exactly `n` bytes. Allocation-free.
    pub fn skip(&mut self, mut n: u64) -> Result<()> {
        // The scratch buffer lives inside `self`, so lend it out while serving.
        let mut scratch = std::mem::take(&mut self.scratch);
        let result = loop {
            if n == 0 {
                break Ok(());
            }
            let want = n.min(scratch.len() as u64) as usize;
            match self.serve(&mut scratch[..want]) {
                Ok(0) => break Err(CadenceError::EndOfStream),
                Ok(got) => n -= got as u64,
                Err(e) => break Err(CadenceError::from(e)),
            }
        };
        self.scratch = scratch;
        result
    }

    /// Serves buffered bytes into `out`, refilling from the underlying source
    /// when empty. Returns `Ok(0)` at end of the readable region.
    fn serve(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.len && self.refill()? == 0 {
            return Ok(0);
        }
        // Buffered data may extend past a limit set after it was read
        // (e.g. the data chunk started inside the read-ahead window), so
        // cap the take by the remaining allowance as well.
        let mut take = (self.len - self.pos).min(out.len());
        if let Some(rem) = self.limit {
            take = take.min(rem as usize);
        }
        if take == 0 {
            return Ok(0);
        }
        out[..take].copy_from_slice(&self.buf[self.pos..self.pos + take]);
        self.pos += take;
        if let Some(rem) = self.limit.as_mut() {
            *rem -= take as u64;
        }
        self.consumed += take as u64;
        Ok(take)
    }

    fn refill(&mut self) -> io::Result<usize> {
        let cap = match self.limit {
            Some(rem) => rem.min(self.buf.len() as u64) as usize,
            None => self.buf.len(),
        };
        if cap == 0 {
            return Ok(0);
        }
        let n = self.inner.read(&mut self.buf[..cap])?;
        self.pos = 0;
        self.len = n;
        Ok(n)
    }
}

impl Read for BufferedSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.serve(buf)
    }
}
