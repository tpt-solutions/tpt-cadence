//! Ogg Opus (RFC 7845) encoder: wraps [`crate::celt::encoder::CeltEncoder`]
//! and writes a complete, spec-legal `.opus` stream (`OpusHead` + `OpusTags`
//! header pages, then CELT-only CBR audio packets, one per page) via
//! [`OggOpusEncoder`], implementing the shared
//! [`Encoder`](tpt_av_cadence_core::Encoder) trait.
//!
//! Scope of this first cut, mirroring the same "real, complete, deliberately
//! reduced feature set" convention the FLAC/MP3 encoders use:
//!
//! - **48 kHz input only** (CELT's native rate; no resampling), **mono or
//!   stereo**, **CELT-only fullband, CBR, 20 ms frames** — exactly the
//!   scope [`CeltEncoder`] itself implements today (see its module doc
//!   comment for the full accounting: transient/TF handling and joint
//!   mid/side stereo coding are supported; SILK/hybrid encoding, VBR, and
//!   intensity stereo are not).
//! - **RFC 7845 pre-skip = 120 samples**: this encoder's CELT analysis/
//!   synthesis path has one 120-sample MDCT overlap of algorithmic delay.
//!   Audio-page granules include that delay, and the final granule is
//!   `input_samples + PRE_SKIP`, so pre-skip removal recovers exactly the
//!   original sample count for both this crate's decoder and external Opus
//!   players.
//! - **One packet per Ogg page** (`OggPageWriter`'s own scope) — not
//!   bit-optimal (a fixed ~27+ byte page overhead per 20 ms packet, plus
//!   segment-table bytes) but always spec-legal.

use std::io::Write;

use tpt_av_cadence_core::{CadenceError, Encoder, Result};
use tpt_av_cadence_ogg::OggPageWriter;

use crate::celt::encoder::CeltEncoder;
use crate::ogg_opus::OpusHead;

/// Output/input sample rate this encoder supports (CELT's native rate;
/// RFC 7845 granule positions are always counted at 48 kHz regardless).
const SAMPLE_RATE: u32 = 48_000;
/// Frame duration this encoder uses: `lm = 3` (20 ms), matching
/// `CeltEncoder`'s documented `N2 = SHORT_MDCT_SIZE << lm` convention.
const LM: usize = 3;
const FRAME_LEN: usize = 120 << LM; // 960 samples/channel
/// CELT MDCT-overlap algorithmic delay at the 48 kHz Opus output rate.
/// Audio granules include this offset and RFC 7845 pre-skip removes it.
const PRE_SKIP: u16 = 120;

/// A fixed, non-randomized Ogg logical-stream serial number. Fine for this
/// encoder's single-stream-per-file scope (no multiplexing); a real
/// multi-stream muxer would need a distinct, ideally random, serial per
/// logical stream.
const DEFAULT_SERIAL: u32 = 0x4F70_7553; // "OpuS" packed as bytes, arbitrary but distinctive

/// Ogg Opus encoder: [`Encoder::encode`] buffers interleaved `f32` PCM into
/// 20 ms CELT frames and writes each as its own Ogg page; [`Encoder::finish`]
/// flushes any final partial frame (zero-padded) and closes the stream with
/// a correctly end-trimmed EOS page.
pub struct OggOpusEncoder<W: Write> {
    sink: W,
    channels: u16,
    bytes_per_frame: usize,
    celt: CeltEncoder,
    page_writer: OggPageWriter,
    /// Interleaved PCM awaiting a full `FRAME_LEN`-sample frame.
    pending: Vec<f32>,
    /// Exact sample count fed via `encode()` (pre-padding) — becomes the
    /// final page's granule position, trimming any zero-pad tail.
    total_samples: i64,
    /// Cumulative frame-aligned sample count already turned into packets.
    emitted_samples: i64,
    /// One packet is always held back so `finish()` can retroactively mark
    /// it (and only it) as the EOS page — the container format requires
    /// the *final audio-carrying* page to carry EOS (see the module doc
    /// comment on `OggOpusDecoder`'s end-trim handling), which isn't known
    /// until `finish()` is actually called.
    buffered_packet: Option<Vec<u8>>,
    buffered_granule: i64,
    finished: bool,
}

impl<W: Write> OggOpusEncoder<W> {
    /// Opens a new Ogg Opus stream for writing: immediately writes the
    /// `OpusHead` and `OpusTags` header pages.
    ///
    /// `sample_rate` must be 48000 (see the module doc comment); `channels`
    /// must be 1 or 2; `bitrate_bps` is the target CELT CBR bitrate
    /// (converted to a fixed per-20ms-frame byte budget — very low
    /// bitrates that would round down to an unusably small frame budget
    /// are rejected).
    pub fn new(mut sink: W, sample_rate: u32, channels: u16, bitrate_bps: u32) -> Result<Self> {
        if sample_rate != SAMPLE_RATE {
            return Err(CadenceError::InvalidFormat(format!(
                "Ogg Opus encoder supports only {SAMPLE_RATE} Hz input, got {sample_rate}"
            )));
        }
        if !(1..=2).contains(&channels) {
            return Err(CadenceError::InvalidFormat(format!(
                "Ogg Opus encoder supports 1 or 2 channels, got {channels}"
            )));
        }
        // 20 ms frames -> 50 frames/s -> bytes/frame = bits_per_sec / 8 / 50.
        let bytes_per_frame = (bitrate_bps as usize) / 400;
        if bytes_per_frame < 20 {
            return Err(CadenceError::InvalidFormat(format!(
                "{bitrate_bps} bps is too low for a 20 ms Opus CBR frame (need >= 8000 bps)"
            )));
        }

        let mut page_writer = OggPageWriter::new(DEFAULT_SERIAL);
        let head = OpusHead {
            version: 1,
            channels,
            pre_skip: PRE_SKIP,
            input_sample_rate: sample_rate,
            output_gain_q8: 0,
            mapping_family: 0,
        };
        let page = page_writer.write_page(&head.write(), 0, true, false);
        sink.write_all(&page)?;

        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        let vendor = b"tpt-cadence";
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes()); // zero user comments
        let page = page_writer.write_page(&tags, 0, false, false);
        sink.write_all(&page)?;

        Ok(OggOpusEncoder {
            sink,
            channels,
            bytes_per_frame,
            celt: CeltEncoder::new(channels as usize, LM),
            page_writer,
            pending: Vec::with_capacity(FRAME_LEN * channels as usize),
            total_samples: 0,
            emitted_samples: 0,
            buffered_packet: None,
            buffered_granule: 0,
            finished: false,
        })
    }

    /// Encodes one full `FRAME_LEN`-sample frame from the front of
    /// `pending`, buffering it as a page (writing out whatever was
    /// previously buffered first, as a non-final page).
    fn emit_frame(&mut self) -> Result<()> {
        let n = FRAME_LEN * self.channels as usize;
        let packet = self
            .celt
            .try_encode_frame(&self.pending[..n], self.bytes_per_frame)?;
        self.pending.drain(..n);
        self.emitted_samples += FRAME_LEN as i64;
        if let Some(prev) = self.buffered_packet.take() {
            let page = self
                .page_writer
                .write_page(&prev, self.buffered_granule, false, false);
            self.sink.write_all(&page)?;
        }
        self.buffered_packet = Some(packet);
        self.buffered_granule = self.emitted_samples + i64::from(PRE_SKIP);
        Ok(())
    }
}

impl<W: Write + Send> Encoder for OggOpusEncoder<W> {
    fn encode(&mut self, samples: &[f32]) -> Result<usize> {
        if self.finished {
            return Err(CadenceError::InvalidFormat(
                "cannot encode samples after finish()".to_string(),
            ));
        }
        let channels = self.channels as usize;
        if samples.len() % channels != 0 {
            return Err(CadenceError::InvalidFormat(format!(
                "sample count {} is not a multiple of the channel count {}",
                samples.len(),
                channels
            )));
        }
        if let Some((index, _sample)) = samples
            .iter()
            .copied()
            .enumerate()
            .find(|(_, sample)| !sample.is_finite() || !(-1.0..=1.0).contains(sample))
        {
            return Err(CadenceError::InvalidFormat(format!(
                "sample {index} is outside the finite [-1, 1] PCM range"
            )));
        }
        self.pending.extend_from_slice(samples);
        self.total_samples += (samples.len() / channels) as i64;
        let n = FRAME_LEN * channels;
        while self.pending.len() >= n {
            self.emit_frame()?;
        }
        Ok(samples.len() / channels)
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let channels = self.channels as usize;
        let n = FRAME_LEN * channels;
        // A leftover partial frame needs zero-padding to reach one full
        // CELT frame; a stream with nothing ever encoded still needs one
        // (silent) audio page, since the container requires EOS to land on
        // a real audio-carrying page, not an empty trailing one.
        if !self.pending.is_empty() || self.buffered_packet.is_none() {
            self.pending.resize(n, 0.0);
            self.emit_frame()?;
        }
        // The codec's PRE_SKIP samples of algorithmic delay require real
        // decoded frames after the input endpoint. Flush zero-padded CELT
        // frames until enough untrimmed output exists for the final granule
        // (`total_samples + PRE_SKIP`) to survive pre-skip removal intact.
        let required_decoded = self.total_samples + i64::from(PRE_SKIP);
        while self.emitted_samples < required_decoded {
            self.pending.resize(n, 0.0);
            self.emit_frame()?;
        }
        if let Some(last) = self.buffered_packet.take() {
            // The final page's granule is the exact input endpoint plus the
            // codec delay; decoder pre-skip removal then recovers exactly the
            // original number of samples.
            // `bos = false`: only the very first page (the OpusHead page
            // written in `new()`) may ever carry BOS — `PageReader`
            // interprets a *second* BOS page on the same serial as the
            // start of a new chained link and immediately ends the
            // current one without yielding this page's packet at all.
            let final_granule = self.total_samples + i64::from(PRE_SKIP);
            let page = self
                .page_writer
                .write_page(&last, final_granule, false, true);
            self.sink.write_all(&page)?;
        }
        self.sink.flush()?;
        Ok(())
    }
}

impl<W: Write> Drop for OggOpusEncoder<W> {
    fn drop(&mut self) {
        // Best-effort flush, matching the FLAC/WAV/AIFF/MP3 encoders'
        // Drop convention.
        if !self.finished {
            let channels = self.channels as usize;
            let n = FRAME_LEN * channels;
            if !self.pending.is_empty() || self.buffered_packet.is_none() {
                self.pending.resize(n, 0.0);
                let _ = self.emit_frame();
            }
            let required_decoded = self.total_samples + i64::from(PRE_SKIP);
            while self.emitted_samples < required_decoded {
                self.pending.resize(n, 0.0);
                let _ = self.emit_frame();
            }
            if let Some(last) = self.buffered_packet.take() {
                let final_granule = self.total_samples + i64::from(PRE_SKIP);
                let page = self
                    .page_writer
                    .write_page(&last, final_granule, false, true);
                let _ = self.sink.write_all(&page);
            }
            let _ = self.sink.flush();
        }
    }
}
