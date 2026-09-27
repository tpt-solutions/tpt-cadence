//! Ogg Opus (RFC 7845) encoder: wraps the crate's CELT and SILK frame
//! encoders and writes a complete, spec-legal `.opus` stream (`OpusHead`
//! and `OpusTags` header pages, then one audio packet per page) via
//! [`OggOpusEncoder`], implementing the shared
//! [`Encoder`](tpt_av_cadence_core::Encoder) trait.
//!
//! Two coding modes are available:
//!
//! - **CELT mode** ([`OggOpusEncoder::new`]/[`OggOpusEncoder::new_vbr`]):
//!   mono/stereo, fullband, 20 ms frames, CBR or loudness-adaptive
//!   constrained VBR (see [`crate::celt::encoder::CeltEncoder`] for the
//!   full feature accounting: transient/TF handling, joint mid/side
//!   stereo, analysis-driven intensity stereo).
//! - **SILK mode** ([`OggOpusEncoder::new_silk`]): mono, 8/12/16 kHz
//!   internal rate, 10/20/40/60 ms packets, VBR payloads — the SILK
//!   foundation ([`crate::silk::encoder::SilkEncoder`]) wrapped in
//!   single-frame code-0 Opus packets with TOC configs 0–11. The payload
//!   bitrate is a *target* (SILK's rate control has no hard CBR sizing
//!   yet), so audio packet sizes vary.
//!
//! In both modes **48 kHz input only** (RFC 7845 granule positions are
//! always counted at 48 kHz regardless) and **RFC 7845 pre-skip**: audio
//! granules include the codec's algorithmic delay, and the final granule
//! is `input_samples + pre_skip`, so pre-skip removal recovers exactly
//! the original sample count for both this crate's decoder and external
//! Opus players. The CELT overlap delay is exactly 120 samples; the SILK
//! constants are measured round-trip alignments (see
//! [`silk_pre_skip`]).
//!
//! **One packet per Ogg page** (`OggPageWriter`'s own scope) — not
//! bit-optimal (a fixed ~27+ byte page overhead per packet, plus
//! segment-table bytes) but always spec-legal.

use std::io::Write;

use tpt_av_cadence_core::{CadenceError, Encoder, Result};
use tpt_av_cadence_ogg::OggPageWriter;

use crate::celt::encoder::{finalize_sized, CeltEncoder};
use crate::ogg_opus::OpusHead;
use crate::range::RangeEncoder;
use crate::silk::encoder::SilkEncoder;

/// Output/input sample rate this encoder supports (RFC 7845 granule
/// positions are always counted at 48 kHz regardless).
const SAMPLE_RATE: u32 = 48_000;
/// Frame duration CELT mode uses: `lm = 3` (20 ms), matching
/// `CeltEncoder`'s documented `N2 = SHORT_MDCT_SIZE << lm` convention.
const CELT_LM: usize = 3;
const CELT_FRAME_LEN: usize = 120 << CELT_LM; // 960 samples/channel
/// CELT MDCT-overlap algorithmic delay at the 48 kHz Opus output rate.
/// Audio granules include this offset and RFC 7845 pre-skip removes it.
const CELT_PRE_SKIP: u16 = 120;

/// Measured SILK-mode pre-skip, indexed by internal rate
/// (8000/12000/16000 Hz): the integer sample shift (at 48 kHz) that
/// best aligns the decoder's output with the encoder's input.
///
/// The SILK round trip's delay is small (~1.4 ms) because the reference
/// resampler's delay matrices over-compensate the FIR kernels' inherent
/// delays; what remains is a *dispersive* residual — the minimum-phase
/// kernels delay low- and mid-frequency content by different amounts, so
/// no single shift is perfect for every partial. These constants
/// maximize the waveform SNR of the full encode→decode round trip
/// (encoder resampler → zero-delay closed-loop NSQ → decoder resampler),
/// measured with aperiodic impulse-train excitation to avoid the
/// phase ambiguity a periodic signal introduces (a periodic signal's SNR
/// curve peaks at every delay shifted by its period; the minimal,
/// causal representative is the codec's true delay).
pub(crate) fn silk_pre_skip(internal_sample_rate: i32) -> u16 {
    match internal_sample_rate {
        8_000 => 68,
        12_000 => 65,
        16_000 => 67,
        _ => unreachable!("internal rate validated by new_silk"),
    }
}

/// The RFC 6716 Table 2 TOC configuration for a SILK-only mono
/// single-frame (code 0) packet: configs 0–3 are narrowband
/// (8 kHz internal), 4–7 mediumband (12 kHz), 8–11 wideband (16 kHz);
/// the duration bits add 10/20/40/60 ms.
pub(crate) fn silk_toc_config(internal_sample_rate: i32, packet_ms: i32) -> u8 {
    let base = match internal_sample_rate {
        8_000 => 0u8,
        12_000 => 4,
        16_000 => 8,
        _ => unreachable!("internal rate validated by new_silk"),
    };
    let duration = match packet_ms {
        10 => 0u8,
        20 => 1,
        40 => 2,
        60 => 3,
        _ => unreachable!("packet duration validated by new_silk"),
    };
    base + duration
}

/// Measured hybrid-mode pre-skip: the integer sample shift (at 48 kHz)
/// that best aligns the hybrid decoder's output with the encoder's
/// input, maximizing whole-waveform SNR on aperiodic impulse-train
/// excitation (same methodology as [`silk_pre_skip`], whose dispersion
/// caveat applies doubly here: the SILK low band and the CELT high band
/// have intrinsically different delays — ~67 vs 120 samples — so the
/// composite alignment is a content-weighted compromise dominated by the
/// low band's energy; the constant lands on the SILK-only WB value).
pub(crate) fn hybrid_pre_skip() -> u16 {
    67
}

/// The coding mode backing an [`OggOpusEncoder`].
enum CodingMode {
    Celt {
        /// Boxed: the CELT state is ~5 KB and the SILK variant would
        /// otherwise inflate every match arm's stack footprint.
        celt: Box<CeltEncoder>,
        /// CBR byte budget per frame, or the *average* budget in VBR
        /// mode (see [`CeltEncoder::encode_frame_vbr`]).
        bytes_per_frame: usize,
        vbr: bool,
    },
    Silk {
        /// Boxed, like the CELT state: the encoder carries several KB of
        /// resampler/analysis buffers.
        silk: Box<SilkEncoder>,
        /// The packet's TOC configuration byte (mono, code 0).
        toc_byte: u8,
        /// Samples per channel in one packet at the 48 kHz API rate.
        frame_samples: usize,
        /// Measured round-trip alignment signalled as pre-skip.
        pre_skip: u16,
    },
    Hybrid {
        silk: Box<SilkEncoder>,
        celt: Box<CeltEncoder>,
        /// The packet's TOC configuration byte (mono, code 0).
        toc_byte: u8,
        /// Fixed whole-frame byte budget split: the SILK share and the
        /// CELT share. The *sum* is what both layers' budget arithmetic
        /// uses (the decoder derives everything from the final packet
        /// length); the split only decides how much room each layer's
        /// allocation gets.
        silk_bytes: usize,
        celt_bytes: usize,
        /// CELT's last coded band: 19 for superwideband, 21 for fullband
        /// (bands 17.. = the ~6.8 kHz+ region the SILK layer leaves
        /// uncoded — the decoder's own hybrid start band).
        celt_end_band: usize,
        /// Samples per channel in one packet at the 48 kHz API rate.
        frame_samples: usize,
        /// Measured round-trip alignment signalled as pre-skip.
        pre_skip: u16,
    },
}

/// A fixed, non-randomized Ogg logical-stream serial number. Fine for this
/// encoder's single-stream-per-file scope (no multiplexing); a real
/// multi-stream muxer would need a distinct, ideally random, serial per
/// logical stream.
const DEFAULT_SERIAL: u32 = 0x4F70_7553; // "OpuS" packed as bytes, arbitrary but distinctive

/// Ogg Opus encoder: [`Encoder::encode`] buffers interleaved `f32` PCM into
/// frames and writes each coded packet as its own Ogg page;
/// [`Encoder::finish`] flushes any final partial frame (zero-padded) and
/// closes the stream with a correctly end-trimmed EOS page.
pub struct OggOpusEncoder<W: Write> {
    sink: W,
    channels: u16,
    mode: CodingMode,
    page_writer: OggPageWriter,
    /// Interleaved PCM awaiting a full frame.
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
    /// CELT mode: `sample_rate` must be 48000 (see the module doc
    /// comment); `channels` must be 1 or 2; `bitrate_bps` is the target
    /// CELT CBR bitrate (converted to a fixed per-20ms-frame byte budget —
    /// very low bitrates that would round down to an unusably small frame
    /// budget are rejected).
    pub fn new(sink: W, sample_rate: u32, channels: u16, bitrate_bps: u32) -> Result<Self> {
        Self::new_impl(sink, sample_rate, channels, bitrate_bps, false)
    }

    /// VBR counterpart to [`OggOpusEncoder::new`]: `bitrate_bps` becomes
    /// the *average* bitrate and each 20 ms frame's actual budget adapts to
    /// its loudness relative to the preceding ~200 ms (constrained to
    /// [max(20, base/3), 3×base] bytes per frame, so peaks stay bounded).
    /// RFC 7845 has no VBR flag — the container output is identical in
    /// shape; only the audio packet sizes vary. Over steady content the
    /// derived budgets converge to the base, so the long-term average
    /// tracks the requested bitrate.
    pub fn new_vbr(sink: W, sample_rate: u32, channels: u16, bitrate_bps: u32) -> Result<Self> {
        Self::new_impl(sink, sample_rate, channels, bitrate_bps, true)
    }

    fn new_impl(
        mut sink: W,
        sample_rate: u32,
        channels: u16,
        bitrate_bps: u32,
        vbr: bool,
    ) -> Result<Self> {
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
            pre_skip: CELT_PRE_SKIP,
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
            mode: CodingMode::Celt {
                celt: Box::new(CeltEncoder::new(channels as usize, CELT_LM)),
                bytes_per_frame,
                vbr,
            },
            page_writer,
            pending: Vec::with_capacity(CELT_FRAME_LEN * channels as usize),
            total_samples: 0,
            emitted_samples: 0,
            buffered_packet: None,
            buffered_granule: 0,
            finished: false,
        })
    }

    /// Opens a new Ogg Opus stream in SILK mode: mono speech-oriented
    /// coding at `internal_sample_rate` Hz (8000/12000/16000 — the
    /// RFC 6716 narrow/medium/wideband SILK layers) with `packet_ms`
    /// (10/20/40/60) per packet. `bitrate_bps` (5000–64000) is the
    /// *target* payload bitrate: SILK's rate control steers the
    /// quantizer SNR toward it, but payloads are VBR — no hard CBR
    /// sizing yet. Useful pairings: 8 kHz NB for ~6–12 kbps narrowband
    /// speech, 12 kHz MB for ~14–20 kbps, 16 kHz WB for ~20–40 kbps.
    ///
    /// The pre-skip signalled in `OpusHead` is the measured SILK
    /// round-trip alignment for the chosen internal rate (see
    /// [`silk_pre_skip`]).
    pub fn new_silk(
        sink: W,
        sample_rate: u32,
        channels: u16,
        bitrate_bps: u32,
        internal_sample_rate: i32,
        packet_ms: i32,
    ) -> Result<Self> {
        Self::new_silk_impl(
            sink,
            sample_rate,
            channels,
            bitrate_bps,
            internal_sample_rate,
            packet_ms,
            false,
        )
    }

    /// CBR counterpart to [`OggOpusEncoder::new_silk`]: every audio
    /// packet is exactly `bitrate_bps·packet_ms/8000` bytes (plus the
    /// TOC byte). The SILK layer starts at the nominal rate and, when a
    /// payload would overshoot, re-encodes the frame at a progressively
    /// reduced rate; undershooting payloads are padded with zero bytes,
    /// which the SILK decoder never reads (its range coding has no
    /// end-relative raw bits), so padding is lossless. The floor is one
    /// SILK frame's minimum size — CBR sizes below ~16 bytes are
    /// rejected, since even a silent frame does not fit.
    pub fn new_silk_cbr(
        sink: W,
        sample_rate: u32,
        channels: u16,
        bitrate_bps: u32,
        internal_sample_rate: i32,
        packet_ms: i32,
    ) -> Result<Self> {
        Self::new_silk_impl(
            sink,
            sample_rate,
            channels,
            bitrate_bps,
            internal_sample_rate,
            packet_ms,
            true,
        )
    }

    fn new_silk_impl(
        mut sink: W,
        sample_rate: u32,
        channels: u16,
        bitrate_bps: u32,
        internal_sample_rate: i32,
        packet_ms: i32,
        cbr: bool,
    ) -> Result<Self> {
        if sample_rate != SAMPLE_RATE {
            return Err(CadenceError::InvalidFormat(format!(
                "Ogg Opus encoder supports only {SAMPLE_RATE} Hz input, got {sample_rate}"
            )));
        }
        if !(1..=2).contains(&channels) {
            return Err(CadenceError::InvalidFormat(format!(
                "Ogg Opus SILK mode supports 1 or 2 channels, got {channels}"
            )));
        }
        if !matches!(internal_sample_rate, 8_000 | 12_000 | 16_000) {
            return Err(CadenceError::InvalidFormat(format!(
                "SILK internal sample rate must be 8000, 12000, or 16000 Hz, got {internal_sample_rate}"
            )));
        }
        if !matches!(packet_ms, 10 | 20 | 40 | 60) {
            return Err(CadenceError::InvalidFormat(format!(
                "SILK packet duration must be 10, 20, 40, or 60 ms, got {packet_ms}"
            )));
        }
        if !(5_000..=64_000).contains(&bitrate_bps) {
            return Err(CadenceError::InvalidFormat(format!(
                "SILK target bitrate {bitrate_bps} bps is outside the supported 5000–64000 bps range"
            )));
        }

        let mut silk = if channels == 2 {
            SilkEncoder::new_stereo(sample_rate as i32, internal_sample_rate, packet_ms)?
        } else {
            SilkEncoder::new(sample_rate as i32, internal_sample_rate, packet_ms)?
        };
        // The rate control target is per internal (mono) channel: split a
        // stereo stream's total target evenly.
        silk.set_bitrate((bitrate_bps / u32::from(channels)) as i32);
        if cbr {
            let bytes = (u64::from(bitrate_bps) * packet_ms as u64 / 8000) as usize;
            silk.set_cbr_bytes(bytes)?;
        }

        let mut page_writer = OggPageWriter::new(DEFAULT_SERIAL);
        let head = OpusHead {
            version: 1,
            channels,
            pre_skip: silk_pre_skip(internal_sample_rate),
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

        let pre_skip = silk_pre_skip(internal_sample_rate);
        Ok(OggOpusEncoder {
            sink,
            channels,
            mode: CodingMode::Silk {
                silk: Box::new(silk),
                toc_byte: (silk_toc_config(internal_sample_rate, packet_ms) << 3)
                    | (u8::from(channels == 2)) << 2,
                frame_samples: packet_ms as usize * 48,
                pre_skip,
            },
            page_writer,
            pending: Vec::with_capacity(packet_ms as usize * 48),
            total_samples: 0,
            emitted_samples: 0,
            buffered_packet: None,
            buffered_granule: 0,
            finished: false,
        })
    }

    /// Opens a new Ogg Opus stream in hybrid mode: mono, superwideband or
    /// fullband coding where the SILK layer (always 16 kHz internal — the
    /// wideband low band) codes everything below ~8 kHz and a CELT layer
    /// codes the `17..end` band window (≈6.8 kHz up to 12 kHz for SWB /
    /// 20 kHz for FB) on the *same range coder*, exactly as a hybrid
    /// decoder reads it. `packet_ms` is 10 or 20 (hybrid has no longer
    /// packet sizes); `fullband` picks FB (TOC configs 14/15) over SWB
    /// (12/13).
    ///
    /// The frame's byte budget is split explicitly: `silk_bitrate_bps`
    /// steers the SILK layer's quantizer SNR and its share
    /// (`silk_bitrate_bps·packet_ms/8000` bytes) of the fixed-size frame,
    /// `celt_bitrate_bps` sizes the CELT share the same way. The total
    /// frame length is fixed (`silk_share + celt_share`), which is what
    /// lets both layers' budget arithmetic match the decoder exactly —
    /// the decoder derives all of it from the final packet length.
    pub fn new_hybrid(
        mut sink: W,
        sample_rate: u32,
        channels: u16,
        silk_bitrate_bps: u32,
        celt_bitrate_bps: u32,
        packet_ms: i32,
        fullband: bool,
    ) -> Result<Self> {
        if sample_rate != SAMPLE_RATE {
            return Err(CadenceError::InvalidFormat(format!(
                "Ogg Opus encoder supports only {SAMPLE_RATE} Hz input, got {sample_rate}"
            )));
        }
        if !(1..=2).contains(&channels) {
            return Err(CadenceError::InvalidFormat(format!(
                "Ogg Opus hybrid mode supports 1 or 2 channels, got {channels}"
            )));
        }
        if !matches!(packet_ms, 10 | 20) {
            return Err(CadenceError::InvalidFormat(format!(
                "hybrid packet duration must be 10 or 20 ms, got {packet_ms}"
            )));
        }
        // The rate targets are per internal (mono) channel; a stereo
        // stream splits its totals evenly across the two channels.
        let silk_per_channel = silk_bitrate_bps / u32::from(channels);
        let celt_per_channel = celt_bitrate_bps / u32::from(channels);
        if !(5_000..=80_000).contains(&silk_per_channel) {
            return Err(CadenceError::InvalidFormat(format!(
                "hybrid SILK target bitrate {silk_bitrate_bps} bps ({} per channel) is outside the supported 5000–80000 bps per-channel range",
                silk_per_channel
            )));
        }
        if !(8_000..=128_000).contains(&celt_per_channel) {
            return Err(CadenceError::InvalidFormat(format!(
                "hybrid CELT target bitrate {celt_bitrate_bps} bps ({} per channel) is outside the supported 8000–128000 bps per-channel range",
                celt_per_channel
            )));
        }

        let silk_bytes = ((u64::from(silk_bitrate_bps) * packet_ms as u64 / 8000) as usize).max(4);
        let celt_bytes = ((u64::from(celt_bitrate_bps) * packet_ms as u64 / 8000) as usize).max(12);

        let stereo = channels == 2;
        let mut silk = if stereo {
            SilkEncoder::new_stereo(sample_rate as i32, 16_000, packet_ms)?
        } else {
            SilkEncoder::new(sample_rate as i32, 16_000, packet_ms)?
        };
        // The rate control target is per internal (mono) channel, and
        // the SILK share of the fixed-size frame is enforced by CBR
        // sizing: an oversized payload is re-encoded at a reduced rate
        // rather than spilling into the CELT layer's budget (which would
        // trip the decoder's redundancy-lookahead guard below).
        silk.set_bitrate(silk_per_channel as i32);
        silk.set_max_payload_bytes(silk_bytes);
        let lm = if packet_ms == 10 { 2 } else { 3 };
        let celt = CeltEncoder::new(channels as usize, lm);
        let celt_end_band = if fullband { 21 } else { 19 };

        // TOC config: 12/13 = hybrid SWB 10/20 ms, 14/15 = hybrid FB;
        // the stereo bit sits two below the code bits.
        let config = if fullband { 14 } else { 12 } + if packet_ms == 20 { 1 } else { 0 };
        let pre_skip = hybrid_pre_skip();

        let mut page_writer = OggPageWriter::new(DEFAULT_SERIAL);
        let head = OpusHead {
            version: 1,
            channels,
            pre_skip,
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
            mode: CodingMode::Hybrid {
                silk: Box::new(silk),
                celt: Box::new(celt),
                toc_byte: (config << 3) | (u8::from(stereo)) << 2,
                silk_bytes,
                celt_bytes,
                celt_end_band,
                frame_samples: packet_ms as usize * 48,
                pre_skip,
            },
            page_writer,
            pending: Vec::with_capacity(packet_ms as usize * 48),
            total_samples: 0,
            emitted_samples: 0,
            buffered_packet: None,
            buffered_granule: 0,
            finished: false,
        })
    }

    /// Samples per channel of one coding frame (CELT: 960; SILK: the
    /// packet duration at 48 kHz).
    fn frame_samples(&self) -> usize {
        match &self.mode {
            CodingMode::Celt { .. } => CELT_FRAME_LEN,
            CodingMode::Silk { frame_samples, .. } => *frame_samples,
            CodingMode::Hybrid { frame_samples, .. } => *frame_samples,
        }
    }

    /// The codec's algorithmic delay signalled as RFC 7845 pre-skip.
    fn pre_skip(&self) -> u16 {
        match &self.mode {
            CodingMode::Celt { .. } => CELT_PRE_SKIP,
            CodingMode::Silk { pre_skip, .. } => *pre_skip,
            CodingMode::Hybrid { pre_skip, .. } => *pre_skip,
        }
    }

    /// Encodes one full frame from the front of `pending`, buffering it
    /// as a page (writing out whatever was previously buffered first, as
    /// a non-final page).
    fn emit_frame(&mut self) -> Result<()> {
        let n = self.frame_samples() * self.channels as usize;
        let packet = match &mut self.mode {
            CodingMode::Celt {
                celt,
                bytes_per_frame,
                vbr,
            } => {
                let pcm = &self.pending[..n];
                if *vbr {
                    celt.try_encode_frame_vbr(pcm, *bytes_per_frame)?
                } else {
                    celt.try_encode_frame(pcm, *bytes_per_frame)?
                }
            }
            CodingMode::Silk { silk, toc_byte, .. } => {
                let frame_api = self.pending[..n]
                    .iter()
                    .map(|&s| {
                        let v = (s * 32768.0).round();
                        v.clamp(-32768.0, 32767.0) as i16
                    })
                    .collect::<Vec<i16>>();
                let payload = silk.encode_frame(&frame_api)?;
                let mut packet = Vec::with_capacity(1 + payload.len());
                packet.push(*toc_byte);
                packet.extend_from_slice(&payload);
                packet
            }
            CodingMode::Hybrid {
                silk,
                celt,
                toc_byte,
                silk_bytes,
                celt_bytes,
                celt_end_band,
                ..
            } => {
                let i16_pcm: Vec<i16> = self.pending[..n]
                    .iter()
                    .map(|&s| {
                        let v = (s * 32768.0).round();
                        v.clamp(-32768.0, 32767.0) as i16
                    })
                    .collect();
                let total_frame_bytes = *silk_bytes + *celt_bytes;
                let mut enc = RangeEncoder::new();
                // SILK layer first (the decoder reads its symbols from
                // the same coder before touching the CELT data).
                silk.encode_frame_into(&i16_pcm, &mut enc)?;
                // The decoder reads the hybrid redundancy bit only when
                // its 17+20-bit lookahead gate holds after the SILK
                // payload (`dec.tell() + 17 + 20 <= 8*len`); if it
                // didn't, the whole CELT symbol stream would desync.
                // With any sane budget split the gate holds by a wide
                // margin — fail loudly rather than emit a packet the
                // decoder would misread.
                if i64::from(enc.tell()) + 37 > (8 * total_frame_bytes) as i64 {
                    return Err(CadenceError::InvalidFormat(
                        "hybrid frame budget: the SILK payload leaves no room for the CELT layer"
                            .to_string(),
                    ));
                }
                // Hybrid redundancy: none.
                enc.encode_bit_logp(false, 12);
                celt.encode_hybrid_frame(
                    &self.pending[..n],
                    &mut enc,
                    17,
                    *celt_end_band,
                    total_frame_bytes,
                );
                let frame = finalize_sized(enc, total_frame_bytes);
                let mut packet = Vec::with_capacity(1 + frame.len());
                packet.push(*toc_byte);
                packet.extend_from_slice(&frame);
                packet
            }
        };
        self.pending.drain(..n);
        self.emitted_samples += (n / self.channels as usize) as i64;
        if let Some(prev) = self.buffered_packet.take() {
            let page = self
                .page_writer
                .write_page(&prev, self.buffered_granule, false, false);
            self.sink.write_all(&page)?;
        }
        self.buffered_packet = Some(packet);
        self.buffered_granule = self.emitted_samples + i64::from(self.pre_skip());
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
        let n = self.frame_samples() * channels;
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
        let n = self.frame_samples() * channels;
        // A leftover partial frame needs zero-padding to reach one full
        // coding frame; a stream with nothing ever encoded still needs one
        // (silent) audio page, since the container requires EOS to land on
        // a real audio-carrying page, not an empty trailing one.
        if !self.pending.is_empty() || self.buffered_packet.is_none() {
            self.pending.resize(n, 0.0);
            self.emit_frame()?;
        }
        // The codec's pre-skip samples of algorithmic delay require real
        // decoded frames after the input endpoint. Flush zero-padded
        // frames until enough untrimmed output exists for the final
        // granule (`total_samples + pre_skip`) to survive pre-skip
        // removal intact — or until emitting stalls (an error, or a call
        // that produces no audio), in which case the stream simply ends
        // a few samples short rather than spinning forever.
        let required_decoded = self.total_samples + i64::from(self.pre_skip());
        while self.emitted_samples < required_decoded {
            let before = self.emitted_samples;
            self.pending.resize(n, 0.0);
            self.emit_frame()?;
            if self.emitted_samples <= before {
                break;
            }
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
            let final_granule = self.total_samples + i64::from(self.pre_skip());
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
            let n = self.frame_samples() * channels;
            if !self.pending.is_empty() || self.buffered_packet.is_none() {
                self.pending.resize(n, 0.0);
                let _ = self.emit_frame();
            }
            let required_decoded = self.total_samples + i64::from(self.pre_skip());
            while self.emitted_samples < required_decoded {
                let before = self.emitted_samples;
                self.pending.resize(n, 0.0);
                if self.emit_frame().is_err() || self.emitted_samples <= before {
                    break;
                }
            }
            if let Some(last) = self.buffered_packet.take() {
                let final_granule = self.total_samples + i64::from(self.pre_skip());
                let page = self
                    .page_writer
                    .write_page(&last, final_granule, false, true);
                let _ = self.sink.write_all(&page);
            }
            let _ = self.sink.flush();
        }
    }
}
