//! Top-level FLAC `Decoder` implementation.

use std::io::Read;

use tpt_av_cadence_core::{
    int_to_f32, BufferedSource, ByteSource, CadenceError, Decoder, Format, FormatReader,
    StreamInfo, Unseekable,
};

use crate::stream::{self, StereoMode};
use crate::subframe;

/// Minimum window that must hold one complete frame plus slack over the
/// declared maximum frame size.
const FRAMESIZE_SLACK: usize = 1024 * 1024;
const MIN_WINDOW: usize = 256 * 1024;
const MAX_WINDOW: usize = 8 * 1024 * 1024;
const DEFAULT_WINDOW: usize = 4 * 1024 * 1024;

/// Decoder for FLAC streams.
///
/// All allocation happens in [`FlacDecoder::from_source`]/[`FlacDecoder::open`];
/// [`Decoder::decode`] is allocation-free, lock-free, and panic-free.
pub struct FlacDecoder {
    source: BufferedSource,
    info: StreamInfo,
    /// Parsed STREAMINFO ground truth.
    streaminfo: stream::StreamInfo,
    /// Absolute byte offset where audio frames begin (for seek reset).
    audio_start: u64,

    /// Input window holding the frame currently being parsed.
    window: Box<[u8]>,
    window_valid: usize,
    window_pos: usize,
    source_eof: bool,

    /// One decode buffer per channel, each `max_blocksize` samples.
    blocks: Vec<Box<[i32]>>,
    /// Effective block-size cap (declared, or the format max when 0).
    max_blocksize: usize,
    /// Frames available in `blocks` for the current frame.
    staged_frames: usize,
    /// Frames already copied out of the current frame.
    staged_pos: usize,
    /// Bit depth of the staged frame (used for output scaling).
    staged_bps: u16,

    /// Absolute number of frames handed to the caller so far.
    frames_out: u64,
    /// Scratch for decode-and-discard seeking; allocated once.
    seek_scratch: Box<[f32]>,
}

impl FlacDecoder {
    /// Opens a FLAC stream over a byte source. Seekable sources
    /// (`Read + Seek + Send`) get working `seek()`.
    pub fn from_source(source: Box<dyn ByteSource>) -> Result<Self, CadenceError> {
        let mut source = BufferedSource::new(source, 8192);
        let (streaminfo, audio_start) = stream::read_metadata(&mut source)?;

        // Window sizing: the declared maximum frame size plus generous
        // slack. Streams that lie about their maximum (or omit it) get a
        // 4 MiB window, which covers every legal frame.
        let window_len = if streaminfo.max_framesize > 0 {
            (streaminfo.max_framesize as usize + FRAMESIZE_SLACK).clamp(MIN_WINDOW, MAX_WINDOW)
        } else {
            DEFAULT_WINDOW
        };

        let channels = streaminfo.channels as usize;
        // Unspecified (zero) block size falls back to the format maximum.
        let max_blocksize = if streaminfo.max_blocksize == 0 {
            65535
        } else {
            streaminfo.max_blocksize as usize
        };
        let mut blocks = Vec::with_capacity(channels);
        for _ in 0..channels {
            blocks.push(vec![0i32; max_blocksize].into_boxed_slice());
        }

        let mut info = StreamInfo::new(
            Format::Flac,
            streaminfo.sample_rate,
            streaminfo.channels,
            streaminfo.bits_per_sample,
        );
        info.total_frames = if streaminfo.total_samples > 0 {
            Some(streaminfo.total_samples)
        } else {
            None
        };
        info.validate()?;

        let seek_scratch_len = channels * streaminfo.max_blocksize as usize;
        let stream_bps = streaminfo.bits_per_sample;
        Ok(FlacDecoder {
            source,
            info,
            streaminfo,
            audio_start,
            window: vec![0u8; window_len].into_boxed_slice(),
            window_valid: 0,
            window_pos: 0,
            source_eof: false,
            blocks,
            max_blocksize,
            staged_frames: 0,
            staged_pos: 0,
            staged_bps: stream_bps,
            frames_out: 0,
            seek_scratch: vec![0.0f32; seek_scratch_len].into_boxed_slice(),
        })
    }

    /// Convenience constructor over a plain readable (unseekable) source.
    pub fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Self::from_source(Box::new(Unseekable(source)))
    }

    /// The parsed STREAMINFO block of this stream (bit depths, MD5 of the
    /// unencoded audio, frame size limits, …).
    pub fn streaminfo(&self) -> &stream::StreamInfo {
        &self.streaminfo
    }

    /// Compacts the window and pulls more bytes from the source.
    fn fill_window(&mut self) -> Result<(), CadenceError> {
        self.window
            .copy_within(self.window_pos..self.window_valid, 0);
        self.window_valid -= self.window_pos;
        self.window_pos = 0;
        while self.window_valid < self.window.len() && !self.source_eof {
            let n = self
                .source
                .read(&mut self.window[self.window_valid..])
                .map_err(CadenceError::from)?;
            if n == 0 {
                self.source_eof = true;
                break;
            }
            self.window_valid += n;
        }
        Ok(())
    }

    /// Finds the next frame sync in the window starting at `from`.
    /// Returns the index of the 0xFF byte, or None.
    fn find_sync(&self, from: usize) -> Option<usize> {
        let mut i = from;
        while i + 1 < self.window_valid {
            if self.window[i] == 0xFF && self.window[i + 1] & 0xFE == 0xF8 {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Decodes the next frame into the channel buffers.
    /// Returns Ok(false) at clean end of stream.
    fn decode_next_frame(&mut self) -> Result<bool, CadenceError> {
        // Known total: stop without even looking at trailing bytes.
        if let Some(total) = self.info.total_frames {
            if self.frames_out >= total {
                return Ok(false);
            }
        }

        loop {
            if self.window_pos + 2 > self.window_valid && !self.source_eof {
                self.fill_window()?;
            }
            if self.window_pos >= self.window_valid {
                return Ok(false); // clean EOF
            }

            let sync = match self.find_sync(self.window_pos) {
                Some(i) => i,
                None => {
                    if self.source_eof {
                        // Only trailing garbage remains: clean end.
                        return Ok(false);
                    }
                    // Keep the final byte (may be half a sync) and refill.
                    self.window_pos = self.window_valid.saturating_sub(1);
                    if !self.progress_or_eof()? {
                        return Err(CadenceError::CorruptData(
                            "no frame sync found and no input progress possible".to_string(),
                        ));
                    }
                    continue;
                }
            };
            self.window_pos = sync;

            // Parse the header (may need more bytes than currently buffered).
            let header = match stream::parse_frame_header(
                &self.window[sync..self.window_valid],
                &self.streaminfo,
            ) {
                Ok(h) => h,
                Err(CadenceError::EndOfStream) => {
                    if self.source_eof {
                        // Candidate sync with too few trailing bytes: the
                        // stream simply ended (trailing garbage / tiny final
                        // frame already consumed).
                        return Ok(false);
                    }
                    if !self.progress_or_eof()? {
                        return Err(CadenceError::CorruptData(
                            "frame header exceeds the input window".to_string(),
                        ));
                    }
                    continue;
                }
                Err(_) => {
                    // Bad sync/CRC: resume scanning after this candidate.
                    self.window_pos = sync + 1;
                    continue;
                }
            };

            if header.block_size as usize > self.max_blocksize {
                // Keep scanning rather than fail the whole stream; the next
                // sync may be a genuine frame.
                self.window_pos = sync + 1;
                continue;
            }

            // Decode subframes from the window.
            let body = &self.window[sync + header.header_len..self.window_valid];
            let mut br = stream::BitReader::new(body);
            let mut failed_channel = None;
            for c in 0..header.channels {
                // The side channel is coded one bit wider than the frame
                // depth. For left/side and mid/side it is channel 1; for
                // right/side the side channel comes first (channel 0) per
                // RFC 9639 Table 10.
                let channel_bps = match (header.stereo_mode, c) {
                    (Some(StereoMode::RightSide), 0)
                    | (Some(StereoMode::LeftSide) | Some(StereoMode::MidSide), 1) => {
                        header.bits_per_sample + 1
                    }
                    _ => header.bits_per_sample,
                };
                let block = &mut self.blocks[c];
                if let Err(e) = subframe::decode_subframe(
                    &mut br,
                    block,
                    header.block_size as usize,
                    channel_bps,
                ) {
                    failed_channel = Some((c, e));
                    break;
                }
            }
            if let Some((c, err)) = failed_channel {
                match err {
                    CadenceError::EndOfStream if !self.source_eof => {
                        // The frame body continued past the window; pull in
                        // more bytes (or give up if that is impossible).
                        if !self.progress_or_eof()? {
                            return Err(CadenceError::CorruptData(
                                "frame body exceeds the input window".to_string(),
                            ));
                        }
                    }
                    CadenceError::EndOfStream => {
                        return Err(CadenceError::CorruptData(format!(
                            "frame body truncated at end of stream (channel {c})"
                        )));
                    }
                    _ => {
                        // Structural corruption: resynchronize after this
                        // candidate sync.
                        self.window_pos = sync + 1;
                    }
                }
                continue;
            }

            // Frame footer: byte-aligned CRC-16 over the whole frame.
            br.align_to_byte();
            let frame_len = header.header_len + br.byte_pos();
            if sync + frame_len + 2 > self.window_valid {
                if self.source_eof {
                    return Err(CadenceError::CorruptData(
                        "truncated frame trailer at end of stream".to_string(),
                    ));
                }
                if !self.progress_or_eof()? {
                    return Err(CadenceError::CorruptData(
                        "frame trailer exceeds the input window".to_string(),
                    ));
                }
                continue;
            }
            let frame_bytes = &self.window[sync..sync + frame_len];
            let stored_crc = u16::from_be_bytes([
                self.window[sync + frame_len],
                self.window[sync + frame_len + 1],
            ]);
            if stream::crc16(frame_bytes) != stored_crc {
                // Corruption: resynchronize after this candidate.
                self.window_pos = sync + 1;
                continue;
            }

            // Success: commit.
            if let Some(mode) = header.stereo_mode {
                self.decorrelate(mode, header.block_size as usize);
            }
            self.staged_frames = header.block_size as usize;
            self.staged_pos = 0;
            self.staged_bps = header.bits_per_sample;
            self.window_pos = sync + frame_len + 2;
            return Ok(true);
        }
    }

    /// Refills the window. Returns Ok(false) when no further progress is
    /// possible: the window is already full and the source has not ended,
    /// so a retry with the same bytes would loop forever.
    fn progress_or_eof(&mut self) -> Result<bool, CadenceError> {
        let available_before = self.window_valid - self.window_pos;
        self.fill_window()?;
        let available_after = self.window_valid - self.window_pos;
        Ok(self.source_eof || available_after > available_before)
    }

    /// Undoes left/side, right/side, or mid/side stereo decorrelation in place.
    fn decorrelate(&mut self, mode: StereoMode, block_size: usize) {
        let (left, right) = match mode {
            StereoMode::LeftSide => {
                for i in 0..block_size {
                    let l = self.blocks[0][i];
                    let side = self.blocks[1][i];
                    self.blocks[1][i] = l.wrapping_sub(side); // right
                }
                return;
            }
            StereoMode::RightSide => {
                // Channel 0 = side, channel 1 = right: left = side + right.
                for i in 0..block_size {
                    let side = self.blocks[0][i];
                    let r = self.blocks[1][i];
                    self.blocks[0][i] = side.wrapping_add(r); // left
                }
                return;
            }
            StereoMode::MidSide => (0usize, 1usize),
        };
        for i in 0..block_size {
            let mid = self.blocks[left][i] as i64;
            let side = self.blocks[right][i] as i64;
            // mid was stored as floor((l + r) / 2): reconstruct exactly.
            let l = (((mid << 1) | (side & 1)).wrapping_add(side)) >> 1;
            self.blocks[left][i] = l as i32;
            self.blocks[right][i] = (l - side) as i32;
        }
    }
}

impl std::fmt::Debug for FlacDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlacDecoder")
            .field("info", &self.info)
            .field("streaminfo", &self.streaminfo)
            .field("frames_out", &self.frames_out)
            .finish()
    }
}

impl Decoder for FlacDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, frame: u64) -> Result<(), CadenceError> {
        let total = self.info.total_frames.unwrap_or(u64::MAX);
        if frame > total {
            return Err(CadenceError::SeekOutOfRange {
                requested: frame,
                total: self.info.total_frames.unwrap_or(0),
            });
        }

        // Reset to the first frame, then decode-and-discard up to `frame`.
        // (Seektable-assisted seeking is future work; this path is correct
        // but linear, and MAY block — it is not real-time safe.)
        self.source.seek_to(self.audio_start)?;
        self.window_valid = 0;
        self.window_pos = 0;
        self.source_eof = false;
        self.staged_frames = 0;
        self.staged_pos = 0;
        self.frames_out = 0;

        let channels = self.info.channels as usize;
        let scratch_frames = self.seek_scratch.len() / channels;
        while self.frames_out < frame {
            let want = ((frame - self.frames_out) as usize).min(scratch_frames) * channels;
            // Lend the scratch buffer out to avoid borrowing `self` twice.
            let mut scratch = std::mem::take(&mut self.seek_scratch);
            let result = self.decode(&mut scratch[..want]);
            self.seek_scratch = scratch;
            let got = result?;
            if got == 0 {
                return Err(CadenceError::CorruptData(
                    "stream ended while seeking".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn decode(&mut self, buffer: &mut [f32]) -> Result<usize, CadenceError> {
        let channels = self.info.channels as usize;
        if buffer.len() % channels != 0 {
            return Err(CadenceError::InvalidFormat(format!(
                "buffer length {} is not a multiple of the channel count {}",
                buffer.len(),
                channels
            )));
        }

        // Known total: never hand out more frames than declared.
        let mut want = buffer.len() / channels;
        if let Some(total) = self.info.total_frames {
            want = want.min((total - self.frames_out.min(total)) as usize);
        }

        let mut written = 0;
        while written < want {
            if self.staged_pos >= self.staged_frames {
                match self.decode_next_frame()? {
                    true => {}
                    false => break, // end of stream
                }
            }
            let available = self.staged_frames - self.staged_pos;
            let n = (want - written).min(available);
            let out = &mut buffer[written * channels..(written + n) * channels];
            for f in 0..n {
                let fi = self.staged_pos + f;
                for (c, slot) in out[f * channels..(f + 1) * channels].iter_mut().enumerate() {
                    *slot = int_to_f32(self.blocks[c][fi] as i64, self.staged_bps);
                }
            }
            self.staged_pos += n;
            self.frames_out += n as u64;
            written += n;
        }
        Ok(written)
    }
}

/// Reader wrapper implementing [`FormatReader`] for FLAC files.
pub struct FlacReader {
    decoder: FlacDecoder,
}

impl FormatReader for FlacReader {
    fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Ok(FlacReader {
            decoder: FlacDecoder::open(source)?,
        })
    }

    fn decoder(&mut self) -> &mut dyn Decoder {
        &mut self.decoder
    }

    fn info(&self) -> &StreamInfo {
        self.decoder.info()
    }
}
