//! Top-level packet decode path for CELT-only Opus packets.
//!
//! This wires the packet/TOC parser ([`crate::packet`]) into
//! [`crate::celt::CeltDecoder`], reproducing the CELT-only slice of
//! `opus_decode_frame`/`opus_decode_native` from libopus's
//! `opus_decoder.c` (not present in this repo — implemented from
//! knowledge of the reference decoder and validated against the official
//! IETF test vectors; see `tests/conformance.rs`).
//!
//! There is no SILK decoder yet, so `Mode::Silk` and `Mode::Hybrid`
//! packets are rejected with [`CadenceError::UnsupportedFeature`]; this is
//! not a full [`Decoder`](tpt_av_cadence_core::Decoder) implementation,
//! just the CELT-only path.

use crate::celt::tables::WINDOW120;
use crate::celt::CeltDecoder;
use crate::packet::{Bandwidth, Mode, Packet};
use crate::range::RangeDecoder;
use crate::silk::decoder::{DecControl, LostFlag, SilkDecoder};
use crate::{CadenceError, Result};

/// Number of output channels this path always produces: the official test
/// vectors (and libopus's own `opus_decode_native`) always render to
/// stereo 48 kHz PCM, upmixing mono streams by duplicating the channel.
pub const OUTPUT_CHANNELS: usize = 2;

/// CELT `end_band` for a given Opus bandwidth (CELT-only mode always uses
/// `start_band = 0`; only hybrid mode starts CELT at band 17).
///
/// Cross-checked against `NB_EBANDS` (21) in `celt::rate`: every value
/// here is `<= NB_EBANDS`.
pub fn celt_end_band(bandwidth: Bandwidth) -> usize {
    match bandwidth {
        Bandwidth::Narrowband => 13,
        Bandwidth::Mediumband | Bandwidth::Wideband => 17,
        Bandwidth::Superwideband => 19,
        Bandwidth::Fullband => 21,
    }
}

/// Number of samples per channel in one CELT frame at 48 kHz for `packet`'s
/// TOC configuration (e.g. 20 ms -> 960).
pub fn celt_frame_size(packet: &Packet) -> usize {
    (packet.toc.frame_duration().micros() * 48 / 1000) as usize
}

/// Decodes one already-demuxed, CELT-only Opus packet end-to-end,
/// writing interleaved stereo `f32` PCM (±1.0 scale) at 48 kHz into `pcm`.
///
/// `celt` must be a decoder constructed for stereo 48 kHz output
/// (`CeltDecoder::new(2, 48000)`); this function reconfigures its
/// start/end band and stream channel count from the packet's TOC on every
/// call, but otherwise leaves its persistent state (energy history,
/// postfilter, PLC memory) untouched — CELT state carries across frames
/// within a packet and across packets in the same stream. Reset `celt`
/// explicitly only when starting a fresh stream (e.g. after a run of
/// packets this crate could not decode).
///
/// `payload` is the same byte slice that was passed to
/// [`crate::packet::parse_packet`] to produce `packet` — frame ranges
/// index into it.
///
/// `pcm` must hold at least `packet.frame_count() * celt_frame_size(packet)
/// * OUTPUT_CHANNELS` samples; frames are written back-to-back as
/// consecutive `frame_size`-sample chunks.
///
/// A zero-length frame range (DTX) is decoded as packet-loss concealment
/// (`celt.decode(None, ..)`), matching what `CeltDecoder::decode`
/// documents.
///
/// Returns the total number of samples per channel written. Returns
/// [`CadenceError::UnsupportedFeature`] if `packet.toc.mode()` is
/// `Mode::Silk` or `Mode::Hybrid` — callers should skip those packets
/// (just advancing their own output-offset bookkeeping) rather than treat
/// this as a hard error, since this crate has no SILK decoder yet.
pub fn decode_celt_only_packet(
    celt: &mut CeltDecoder,
    packet: &Packet,
    payload: &[u8],
    pcm: &mut [f32],
) -> Result<usize> {
    let toc = packet.toc;
    match toc.mode() {
        Mode::Celt => {}
        Mode::Silk => {
            return Err(CadenceError::UnsupportedFeature(
                "SILK mode packets are not supported (no SILK decoder yet)".to_string(),
            ))
        }
        Mode::Hybrid => {
            return Err(CadenceError::UnsupportedFeature(
                "Hybrid mode packets are not supported (no SILK decoder yet)".to_string(),
            ))
        }
    }

    celt.set_start_band(0);
    celt.set_end_band(celt_end_band(toc.bandwidth()));
    celt.set_stream_channels(if toc.stereo { 2 } else { 1 });

    let frame_size = celt_frame_size(packet);
    let frame_count = packet.frame_count();
    let needed = frame_count * frame_size * OUTPUT_CHANNELS;
    if pcm.len() < needed {
        return Err(CadenceError::BufferTooSmall {
            needed,
            provided: pcm.len(),
        });
    }

    for i in 0..frame_count {
        let (start, end) = packet.frame_range(i).ok_or_else(|| {
            CadenceError::CorruptData(format!(
                "packet reports {frame_count} frames but frame {i} has no byte range"
            ))
        })?;
        let chunk =
            &mut pcm[i * frame_size * OUTPUT_CHANNELS..(i + 1) * frame_size * OUTPUT_CHANNELS];
        if start == end {
            // DTX / empty frame: conceal.
            celt.decode(None, frame_size, chunk)?;
        } else {
            celt.decode(Some(&payload[start..end]), frame_size, chunk)?;
        }
    }

    Ok(frame_count * frame_size)
}

/// Decodes one already-demuxed, SILK-only Opus packet end-to-end,
/// writing interleaved stereo 16-bit PCM at 48 kHz into `pcm`.
///
/// This mirrors [`decode_celt_only_packet`] for the SILK modes: the
/// packet TOC's config selects the internal rate (configs 0–3 → 8 kHz,
/// 4–7 → 12 kHz, 8–11 → 16 kHz) and payload duration (10/20/40/60 ms),
/// the TOC's stereo flag the internal channel count. Every frame of the
/// packet is decoded against its own byte range with its own range
/// decoder and `new_packet_flag` set, exactly as libopus's
/// `opus_decode_frame`/`opus_decode_native` pair does; a zero-length
/// frame range (DTX) is decoded as packet-loss concealment, which for
/// SILK means the comfort-noise path.
///
/// `silk` must persist across the stream's SILK packets; libopus resets
/// it after CELT-only packets (`st->prev_mode == MODE_CELT_ONLY`), so
/// callers should [`SilkDecoder::reset`] at every SILK run start.
///
/// Returns the total samples per channel written. Hybrid packets are
/// still rejected: they additionally need the CELT low-band merge.
pub fn decode_silk_only_packet(
    silk: &mut SilkDecoder,
    packet: &Packet,
    payload: &[u8],
    pcm: &mut [i16],
) -> Result<usize> {
    let toc = packet.toc;
    match toc.mode() {
        Mode::Silk => {}
        Mode::Hybrid => {
            return Err(CadenceError::UnsupportedFeature(
                "hybrid packets need the SILK low band merged with CELT (not wired yet)"
                    .to_string(),
            ))
        }
        Mode::Celt => {
            return Err(CadenceError::UnsupportedFeature(
                "not a SILK-only packet; use decode_celt_only_packet".to_string(),
            ))
        }
    }

    let internal_sample_rate: i32 = match toc.config / 4 {
        0 => 8000,
        1 => 12000,
        2 => 16000,
        _ => {
            return Err(CadenceError::CorruptData(format!(
                "config {} is not a SILK-only configuration",
                toc.config
            )))
        }
    };
    let n_channels_internal = if toc.stereo { 2 } else { 1 };

    let frame_size = celt_frame_size(packet);
    let frame_count = packet.frame_count();
    let total = frame_count * frame_size;
    let needed = total * OUTPUT_CHANNELS;
    if pcm.len() < needed {
        return Err(CadenceError::BufferTooSmall {
            needed,
            provided: pcm.len(),
        });
    }

    // `payload_size_ms` mirrors libopus's IMAX(10, 1000 * audiosize / Fs):
    // the duration of one Opus frame at the API rate. A single call to
    // `SilkDecoder::decode` only ever produces one *internal* SILK frame
    // (always 20 ms, except the rare 10 ms case), so a 40/60 ms Opus frame
    // must drive multiple `decode` calls sharing the same range decoder —
    // mirroring libopus's `while (nSamplesOut < FrameSize)` loop in
    // `opus_decoder.c`'s SILK path.
    let payload_size_ms: i32 = (frame_size as i32).max(10 * 48) / 48;
    let (n_frames_per_payload, _) = SilkDecoder::frames_per_packet(payload_size_ms)?;
    let sub_frame_size = frame_size / n_frames_per_payload;

    for i in 0..frame_count {
        let (start, end) = packet.frame_range(i).ok_or_else(|| {
            CadenceError::CorruptData(format!(
                "packet reports {frame_count} frames but frame {i} has no byte range"
            ))
        })?;
        let frame_pcm =
            &mut pcm[i * frame_size * OUTPUT_CHANNELS..(i + 1) * frame_size * OUTPUT_CHANNELS];
        if start == end {
            // DTX / empty frame: conceal (the reference routes zero-byte
            // payloads through the PLC/CNG path with data == NULL).
            for f in 0..n_frames_per_payload {
                let mut ctrl = DecControl {
                    n_channels_api: OUTPUT_CHANNELS,
                    n_channels_internal,
                    api_sample_rate: 48_000,
                    internal_sample_rate,
                    payload_size_ms,
                    prev_pitch_lag: 0,
                };
                let chunk = &mut frame_pcm[f * sub_frame_size * OUTPUT_CHANNELS
                    ..(f + 1) * sub_frame_size * OUTPUT_CHANNELS];
                silk.decode(&mut ctrl, None, LostFlag::PacketLost, f == 0, chunk)?;
            }
        } else {
            let mut dec = RangeDecoder::new(&payload[start..end]);
            for f in 0..n_frames_per_payload {
                let mut ctrl = DecControl {
                    n_channels_api: OUTPUT_CHANNELS,
                    n_channels_internal,
                    api_sample_rate: 48_000,
                    internal_sample_rate,
                    payload_size_ms,
                    prev_pitch_lag: 0,
                };
                let chunk = &mut frame_pcm[f * sub_frame_size * OUTPUT_CHANNELS
                    ..(f + 1) * sub_frame_size * OUTPUT_CHANNELS];
                silk.decode(&mut ctrl, Some(&mut dec), LostFlag::Normal, f == 0, chunk)?;
            }
        }
    }

    Ok(total)
}

// ---------------------------------------------------------------------------
// Full top-level decoder — port of libopus's `opus_decoder.c` state machine
// ---------------------------------------------------------------------------

/// `st->Fs`: this crate always renders at 48 kHz (the official test
/// vectors' rate, and libopus's own internal rate).
const OPUS_FS: usize = 48_000;
/// 20/10/5/2.5 ms in samples per channel at 48 kHz (`F20`/`F10`/`F5`/`F2_5`).
const F20: usize = OPUS_FS / 50;
const F10: usize = F20 >> 1;
const F5: usize = F10 >> 1;
const F2_5: usize = F5 >> 1;

/// `st->mode`/`st->prev_mode` (`MODE_NONE`, `MODE_SILK_ONLY`, `MODE_HYBRID`,
/// `MODE_CELT_ONLY` in `opus_decoder.c`; ordering matters only for the
/// `> 0` / `== MODE_CELT_ONLY` comparisons, which these variants mirror).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OpusMode {
    None,
    Silk,
    Hybrid,
    Celt,
}

/// `opus_decoder.c`'s `smooth_fade`: element-wise crossfade
/// `out[i] = w·in2[i] + (1-w)·in1[i]` with `w = window[i]²` (the window is
/// applied squared, per `MULT16_16_Q15(window, window)` in the float
/// build). `in1`/`in2` may alias `out` element-for-element, exactly as the
/// C call sites do; each output depends only on the inputs at its own
/// index, so the small stack copies below make that aliasing safe.
fn smooth_fade(
    in1: &[f32],
    in2: &[f32],
    out: &mut [f32],
    overlap: usize,
    channels: usize,
    window: &[f32],
) {
    let n = overlap * channels;
    let mut a = [0f32; 2 * F2_5];
    let mut b = [0f32; 2 * F2_5];
    a[..n].copy_from_slice(&in1[..n]);
    b[..n].copy_from_slice(&in2[..n]);
    // `inc = 48000/Fs` is 1 at this crate's fixed 48 kHz output rate.
    for (i, &w0) in window.iter().take(overlap).enumerate() {
        let w = w0 * w0;
        for c in 0..channels {
            let idx = i * channels + c;
            out[idx] = w * b[idx] + (1.0 - w) * a[idx];
        }
    }
}

/// The complete Opus decoder state — port of libopus's `OpusDecoder`
/// (`src/opus_decoder.c`, float build): one SILK decoder, one CELT
/// decoder, and the cross-packet state (`mode`/`prev_mode`,
/// `prev_redundancy`, `rangeFinal`) that drives mode transitions, the
/// 5 ms CELT redundancy frames, hybrid mixing, and packet-loss
/// concealment across SILK-only, CELT-only, and hybrid packets.
///
/// PCM output is interleaved `f32` at ±1.0 scale, 48 kHz, `channels`
/// channels, mirroring `opus_decode_float`. `OpusDecoder::range_final()`
/// mirrors `st->rangeFinal` (`dec.rng ^ redundant_rng`), the value the
/// official test vectors record per packet.
pub struct OpusDecoder {
    silk: SilkDecoder,
    celt: CeltDecoder,
    /// `st->channels` — API output channel count (1 or 2).
    channels: usize,
    /// `st->mode` — the current packet's mode (set per packet).
    mode: OpusMode,
    /// `st->prev_mode` — the previous packet's (or frame's) mode.
    prev_mode: OpusMode,
    /// `st->bandwidth` — the current packet's bandwidth.
    bandwidth: Bandwidth,
    /// `st->frame_size` — the current packet's per-frame sample count.
    frame_size: usize,
    /// `st->stream_channels` — the current packet's bitstream channels.
    stream_channels: usize,
    /// `st->prev_redundancy`.
    prev_redundancy: bool,
    /// `st->rangeFinal`.
    range_final: u32,
    /// `st->DecControl.nChannelsInternal` — persistent across calls: the
    /// reference only refreshes it (and `internalSampleRate`) when a real
    /// payload is present, so the PLC path reuses the previous packet's
    /// values.
    silk_n_channels_internal: usize,
    /// `st->DecControl.internalSampleRate`, persistent like the above.
    silk_internal_rate: i32,
    /// Scratch `pcm_silk` (`IMAX(F10, 60 ms) * channels` i16 samples).
    pcm_silk: Vec<i16>,
    /// Scratch `pcm_transition` (`F5 * channels` f32 samples).
    pcm_transition: Vec<f32>,
    /// Scratch `redundant_audio` (`F5 * channels` f32 samples).
    redundant_audio: Vec<f32>,
}

impl OpusDecoder {
    /// `opus_decoder_init`: creates a decoder rendering `channels` (1 or 2)
    /// channels at 48 kHz.
    pub fn new(channels: usize) -> Result<Self> {
        // Resolve debug-trace env flags before any decode (see debug.rs).
        crate::debug::init();
        if !(1..=2).contains(&channels) {
            return Err(CadenceError::UnsupportedFeature(
                "Opus decoder needs 1 or 2 output channels".to_string(),
            ));
        }
        Ok(OpusDecoder {
            silk: SilkDecoder::new(channels)?,
            celt: CeltDecoder::new(channels, OPUS_FS as u32)?,
            channels,
            mode: OpusMode::None,
            prev_mode: OpusMode::None,
            bandwidth: Bandwidth::Fullband,
            frame_size: 0,
            stream_channels: channels,
            prev_redundancy: false,
            range_final: 0,
            silk_n_channels_internal: 1,
            silk_internal_rate: 16_000,
            pcm_silk: vec![0; 3 * F20 * 2],
            pcm_transition: vec![0.0; F5 * 2],
            redundant_audio: vec![0.0; F5 * 2],
        })
    }

    /// `st->rangeFinal` after the most recent [`decode_packet`]/
    /// [`decode_plc`] call: the final range-coder state XORed with the
    /// redundancy frame's, as recorded in the official test vectors.
    pub fn range_final(&self) -> u32 {
        self.range_final
    }

    /// `opus_decode_native`'s packet path: decodes every frame of an
    /// already-demuxed Opus packet (any mode — SILK-only, CELT-only, or
    /// hybrid) into interleaved `f32` PCM at 48 kHz, updating all
    /// cross-packet state. Zero-length frame ranges (DTX) conceal
    /// internally like the reference.
    ///
    /// `payload` is the same byte slice passed to [`crate::packet::parse_packet`].
    /// `pcm` must hold `packet.frame_count() * frame_size * channels`
    /// samples. Returns the samples per channel written.
    pub fn decode_packet(
        &mut self,
        packet: &Packet,
        payload: &[u8],
        pcm: &mut [f32],
    ) -> Result<usize> {
        // `opus_decode_native`: update the packet-level state first (after
        // parse validation), then decode each frame against it.
        self.mode = match packet.toc.mode() {
            Mode::Silk => OpusMode::Silk,
            Mode::Hybrid => OpusMode::Hybrid,
            Mode::Celt => OpusMode::Celt,
        };
        self.bandwidth = packet.toc.bandwidth();
        self.frame_size = celt_frame_size(packet);
        self.stream_channels = if packet.toc.stereo { 2 } else { 1 };

        let count = packet.frame_count();
        let needed = count * self.frame_size * self.channels;
        if pcm.len() < needed {
            return Err(CadenceError::BufferTooSmall {
                needed,
                provided: pcm.len(),
            });
        }

        let mut nb_samples = 0;
        for i in 0..count {
            let (start, end) = packet.frame_range(i).ok_or_else(|| {
                CadenceError::CorruptData(format!(
                    "packet reports {} frames but frame {i} has no byte range",
                    count
                ))
            })?;
            let ret = self.decode_frame(
                Some(&payload[start..end]),
                &mut pcm[nb_samples * self.channels..],
                count * self.frame_size - nb_samples,
            )?;
            nb_samples += ret;
        }
        Ok(nb_samples)
    }

    /// `opus_decode_native`'s PLC path (`data == NULL`): conceals
    /// `frame_size` samples per channel (a multiple of 2.5 ms, as the
    /// reference requires) into `pcm`.
    pub fn decode_plc(&mut self, frame_size: usize, pcm: &mut [f32]) -> Result<usize> {
        if frame_size % F2_5 != 0 {
            return Err(CadenceError::UnsupportedFeature(
                "PLC output size must be a multiple of 2.5 ms".to_string(),
            ));
        }
        let needed = frame_size * self.channels;
        if pcm.len() < needed {
            return Err(CadenceError::BufferTooSmall {
                needed,
                provided: pcm.len(),
            });
        }
        let mut pcm_count = 0;
        loop {
            let ret = self.decode_frame(
                None,
                &mut pcm[pcm_count * self.channels..],
                frame_size - pcm_count,
            )?;
            pcm_count += ret;
            if pcm_count >= frame_size {
                break;
            }
        }
        Ok(pcm_count)
    }

    /// `opus_decode_frame`: decodes one frame of a packet (or conceals one
    /// frame when `data` is `None` or the frame is a ≤1-byte DTX stub),
    /// including the SILK/CELT hybrid mixing, 5 ms redundancy frames,
    /// mode-transition crossfades, and PLC. `frame_size` is the remaining
    /// output capacity in samples per channel; returns the samples per
    /// channel produced.
    fn decode_frame(
        &mut self,
        data: Option<&[u8]>,
        pcm: &mut [f32],
        frame_size_in: usize,
    ) -> Result<usize> {
        if frame_size_in < F2_5 {
            return Err(CadenceError::BufferTooSmall {
                needed: F2_5 * self.channels,
                provided: frame_size_in * self.channels,
            });
        }
        let mut frame_size = frame_size_in.min(OPUS_FS / 25 * 3);
        // Payloads of 0/1 bytes trigger PLC/DTX, capped at the packet's
        // own frame size.
        let mut data = data;
        if let Some(d) = data {
            if d.len() <= 1 {
                data = None;
                frame_size = frame_size.min(self.frame_size);
            }
        }

        let mode;
        let bandwidth: Option<Bandwidth>;
        let mut audiosize;
        let mut dec: Option<RangeDecoder> = None;
        match data {
            Some(d) => {
                audiosize = self.frame_size;
                mode = self.mode;
                bandwidth = Some(self.bandwidth);
                dec = Some(RangeDecoder::new(d));
            }
            None => {
                audiosize = frame_size;
                // Run PLC in the last used mode (CELT if we ended with a
                // CELT redundancy frame).
                mode = if self.prev_redundancy {
                    OpusMode::Celt
                } else {
                    self.prev_mode
                };
                bandwidth = None;
                if mode == OpusMode::None {
                    // No packet yet: all we can do is return zeros.
                    pcm[..audiosize * self.channels].fill(0.0);
                    return Ok(audiosize);
                }
                if audiosize > F20 {
                    // Conceal long stretches in ≤20 ms chunks.
                    let total = audiosize;
                    let mut pcm_off = 0;
                    let mut remaining = audiosize;
                    while remaining > 0 {
                        let ret = self.decode_frame(
                            None,
                            &mut pcm[pcm_off * self.channels..],
                            remaining.min(F20),
                        )?;
                        pcm_off += ret;
                        remaining -= ret;
                    }
                    return Ok(total);
                } else if audiosize < F20 {
                    if audiosize > F10 {
                        audiosize = F10;
                    } else if mode != OpusMode::Silk && audiosize > F5 && audiosize < F10 {
                        audiosize = F5;
                    }
                }
            }
        }

        // Mode-transition detection (`opus_decoder.c` lines 346-357).
        let mut transition = data.is_some()
            && self.prev_mode != OpusMode::None
            && ((mode == OpusMode::Celt
                && self.prev_mode != OpusMode::Celt
                && !self.prev_redundancy)
                || (mode != OpusMode::Celt && self.prev_mode == OpusMode::Celt));

        // CELT→SILK/hybrid transition: conceal the first 5 ms in the
        // outgoing (CELT) mode so the new frame can crossfade from it.
        if transition && mode == OpusMode::Celt {
            let mut buf = std::mem::take(&mut self.pcm_transition);
            self.decode_frame(None, &mut buf, audiosize.min(F5))?;
            self.pcm_transition = buf;
        }
        if audiosize > frame_size {
            return Err(CadenceError::BufferTooSmall {
                needed: audiosize * self.channels,
                provided: frame_size * self.channels,
            });
        }
        frame_size = audiosize;

        // SILK processing (`opus_decoder.c` lines 377-450).
        let mut pcm_silk: Option<Vec<i16>> = None;
        if mode != OpusMode::Celt {
            if self.prev_mode == OpusMode::Celt {
                self.silk.reset();
            }
            let payload_size_ms = 10i32.max(1000 * audiosize as i32 / OPUS_FS as i32);
            if data.is_some() {
                self.silk_n_channels_internal = self.stream_channels;
                self.silk_internal_rate = if mode == OpusMode::Hybrid {
                    16_000
                } else {
                    match bandwidth {
                        Some(Bandwidth::Narrowband) => 8_000,
                        Some(Bandwidth::Mediumband) => 12_000,
                        _ => 16_000,
                    }
                };
            }
            let lost_flag = if data.is_none() {
                LostFlag::PacketLost
            } else {
                LostFlag::Normal
            };
            let mut scratch = std::mem::take(&mut self.pcm_silk);
            let mut decoded_samples = 0;
            loop {
                let mut ctrl = DecControl {
                    n_channels_api: self.channels,
                    n_channels_internal: self.silk_n_channels_internal,
                    api_sample_rate: OPUS_FS as i32,
                    internal_sample_rate: self.silk_internal_rate,
                    payload_size_ms,
                    prev_pitch_lag: 0,
                };
                let first_frame = decoded_samples == 0;
                let n = self.silk.decode(
                    &mut ctrl,
                    dec.as_mut(),
                    lost_flag,
                    first_frame,
                    &mut scratch[decoded_samples * self.channels..],
                )?;
                decoded_samples += n;
                if decoded_samples >= frame_size {
                    break;
                }
            }
            pcm_silk = Some(scratch);
        }

        // Redundancy header (`opus_decoder.c` lines 452-483).
        let mut len = data.map_or(0, |d| d.len());
        let mut redundancy = false;
        let mut redundancy_bytes = 0usize;
        let mut celt_to_silk = false;
        if let (Some(_), Some(dec)) = (data, dec.as_mut()) {
            if mode != OpusMode::Celt
                && dec.tell() + 17 + 20 * u32::from(mode == OpusMode::Hybrid) <= (8 * len) as u32
            {
                // Check if we have a redundant 0-8 kHz band.
                redundancy = if mode == OpusMode::Hybrid {
                    dec.decode_bit_logp(12)?
                } else {
                    true
                };
                if redundancy {
                    celt_to_silk = dec.decode_bit_logp(1)?;
                    redundancy_bytes = if mode == OpusMode::Hybrid {
                        dec.decode_uint(256)? as usize + 2
                    } else {
                        len - ((dec.tell() + 7) >> 3) as usize
                    };
                    len -= redundancy_bytes;
                    // Sanity check (never hits for valid packets).
                    if (len as u32) * 8 < dec.tell() {
                        len = 0;
                        redundancy_bytes = 0;
                        redundancy = false;
                    }
                    // Shrink the decoder because of the raw-coded
                    // redundancy bytes at the end of the frame.
                    dec.shrink_storage(redundancy_bytes);
                }
            }
        }
        let start_band: usize = if mode != OpusMode::Celt { 17 } else { 0 };

        // A redundancy frame replaces the transition handling entirely
        // (`opus_decoder.c` lines 485-489).
        if redundancy {
            transition = false;
        }

        // SILK/hybrid-side mode transition: conceal the first 5 ms in the
        // outgoing (CELT) mode (`opus_decoder.c` lines 491-497) so the
        // hybrid frame can be crossfaded from it below.
        if transition && mode != OpusMode::Celt {
            let mut buf = std::mem::take(&mut self.pcm_transition);
            self.decode_frame(None, &mut buf, audiosize.min(F5))?;
            self.pcm_transition = buf;
        }

        if let Some(bw) = bandwidth {
            self.celt.set_end_band(celt_end_band(bw));
        }
        self.celt.set_stream_channels(self.stream_channels);

        let mut redundant_rng: u32 = 0;
        // 5 ms redundant frame for CELT→SILK (`opus_decoder.c` lines
        // 531-543): decoded (for the final range) even when its audio
        // cannot be used because the CELT decoder was stale.
        if redundancy && celt_to_silk {
            self.celt.set_start_band(0);
            let red = &data.unwrap()[len..len + redundancy_bytes];
            self.celt
                .decode_with_ec(Some(red), F5, None, &mut self.redundant_audio)?;
            redundant_rng = self.celt.final_range();
        }

        // MUST be after the redundancy decode above.
        self.celt.set_start_band(start_band);

        if mode != OpusMode::Silk {
            let celt_frame_size = frame_size.min(F20);
            // Make sure to discard any previous CELT state on a mode
            // switch (unless that state IS the transition, carried by a
            // redundancy frame).
            if mode != self.prev_mode && self.prev_mode != OpusMode::None && !self.prev_redundancy {
                self.celt.reset();
            }
            // Hybrid mode continues on the SILK frame's range decoder, so
            // pass the shared decoder plus the possibly-shrunk byte range.
            let celt_data = data.map(|d| &d[..len]);
            self.celt
                .decode_with_ec(celt_data, celt_frame_size, dec.as_mut(), pcm)?;
        } else {
            pcm[..frame_size * self.channels].fill(0.0);
            // For hybrid → SILK transitions, let the CELT MDCT do a
            // fade-out by decoding a silence frame
            // (`opus_decoder.c` lines 561-574).
            if self.prev_mode == OpusMode::Hybrid
                && !(redundancy && celt_to_silk && self.prev_redundancy)
            {
                self.celt.set_start_band(0);
                let silence = [0xFFu8, 0xFF];
                self.celt.decode_with_ec(Some(&silence), F2_5, None, pcm)?;
            }
        }

        // Mix the 16-bit SILK output into the float CELT output
        // (`opus_decoder.c` lines 577-586).
        if mode != OpusMode::Celt {
            let scratch = pcm_silk.as_deref().unwrap_or(&[]);
            for i in 0..frame_size * self.channels {
                pcm[i] += (1.0f32 / 32768.0) * scratch[i] as f32;
            }
        }

        // 5 ms redundant frame for SILK→CELT (`opus_decoder.c` lines
        // 594-604): resets the CELT state, decodes the redundancy frame
        // (which becomes the CELT state the next CELT frame continues
        // from), and crossfades the main frame's last 2.5 ms into it.
        if redundancy && !celt_to_silk {
            self.celt.reset();
            self.celt.set_start_band(0);
            let red = &data.unwrap()[len..len + redundancy_bytes];
            self.celt
                .decode_with_ec(Some(red), F5, None, &mut self.redundant_audio)?;
            redundant_rng = self.celt.final_range();
            let fade_start = (frame_size - F2_5) * self.channels;
            let mut in1 = [0f32; F2_5 * 2];
            in1[..F2_5 * self.channels]
                .copy_from_slice(&pcm[fade_start..fade_start + F2_5 * self.channels]);
            smooth_fade(
                &in1,
                &self.redundant_audio[F2_5 * self.channels..],
                &mut pcm[fade_start..],
                F2_5,
                self.channels,
                &WINDOW120,
            );
        }
        // 5 ms redundant frame for CELT→SILK; ignored when the previous
        // frame did not use CELT (the first redundancy frame in a
        // transition from SILK may have been lost)
        // (`opus_decoder.c` lines 605-617).
        if redundancy && celt_to_silk && (self.prev_mode != OpusMode::Silk || self.prev_redundancy)
        {
            let n = F2_5 * self.channels;
            pcm[..n].copy_from_slice(&self.redundant_audio[..n]);
            let mut in2 = [0f32; F2_5 * 2];
            in2[..n].copy_from_slice(&pcm[n..2 * n]);
            smooth_fade(
                &self.redundant_audio[n..],
                &in2,
                &mut pcm[n..],
                F2_5,
                self.channels,
                &WINDOW120,
            );
        }
        if transition {
            transition = false; // (mirrors C's reuse of the local flag below)
            let _ = transition;
            if frame_size >= F5 {
                let n = F2_5 * self.channels;
                pcm[..n].copy_from_slice(&self.pcm_transition[..n]);
                let mut in2 = [0f32; F2_5 * 2];
                in2[..n].copy_from_slice(&pcm[n..2 * n]);
                smooth_fade(
                    &self.pcm_transition[n..],
                    &in2,
                    &mut pcm[n..],
                    F2_5,
                    self.channels,
                    &WINDOW120,
                );
            } else {
                let mut in2 = [0f32; F2_5 * 2];
                in2[..F2_5 * self.channels].copy_from_slice(&pcm[..F2_5 * self.channels]);
                smooth_fade(
                    &self.pcm_transition,
                    &in2,
                    pcm,
                    F2_5,
                    self.channels,
                    &WINDOW120,
                );
            }
        }

        // `opus_decoder.c` lines 651-654.
        self.range_final = if len <= 1 {
            0
        } else {
            dec.map(|d| d.rng()).unwrap_or(0) ^ redundant_rng
        };

        self.prev_mode = mode;
        self.prev_redundancy = redundancy && !celt_to_silk;

        // Return the pcm_silk scratch (and keep pcm_transition; it holds
        // the transition audio until the next transition overwrite).
        if let Some(scratch) = pcm_silk {
            self.pcm_silk = scratch;
        }

        Ok(frame_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::parse_packet;

    #[test]
    fn rejects_silk_and_hybrid_modes() {
        // Config 0 => SILK, narrowband, 10ms, code 0 (single frame).
        let payload = [0x00u8, 1, 2, 3];
        let packet = parse_packet(&payload).unwrap();
        let mut celt = CeltDecoder::new(2, 48_000).unwrap();
        let mut pcm = [0f32; 4096];
        let err = decode_celt_only_packet(&mut celt, &packet, &payload, &mut pcm).unwrap_err();
        assert!(matches!(err, CadenceError::UnsupportedFeature(_)));

        // Config 12 => Hybrid.
        let payload = [(12u8 << 3), 1, 2, 3];
        let packet = parse_packet(&payload).unwrap();
        let err = decode_celt_only_packet(&mut celt, &packet, &payload, &mut pcm).unwrap_err();
        assert!(matches!(err, CadenceError::UnsupportedFeature(_)));
    }

    #[test]
    fn end_band_matches_bandwidth_table() {
        assert_eq!(celt_end_band(Bandwidth::Narrowband), 13);
        assert_eq!(celt_end_band(Bandwidth::Mediumband), 17);
        assert_eq!(celt_end_band(Bandwidth::Wideband), 17);
        assert_eq!(celt_end_band(Bandwidth::Superwideband), 19);
        assert_eq!(celt_end_band(Bandwidth::Fullband), 21);
    }

    #[test]
    fn decodes_a_celt_only_silence_frame() {
        // Config 31 (>>3) => CELT, fullband, 20ms, mono, code 0.
        // An all-zero payload after the TOC decodes as a valid (if
        // low-energy) CELT frame rather than erroring.
        let payload = [(31u8 << 3), 0, 0, 0, 0, 0];
        let packet = parse_packet(&payload).unwrap();
        let mut celt = CeltDecoder::new(2, 48_000).unwrap();
        let frame_size = celt_frame_size(&packet);
        assert_eq!(frame_size, 960);
        let mut pcm = vec![0f32; frame_size * OUTPUT_CHANNELS];
        let produced = decode_celt_only_packet(&mut celt, &packet, &payload, &mut pcm).unwrap();
        assert_eq!(produced, frame_size);
    }
}
