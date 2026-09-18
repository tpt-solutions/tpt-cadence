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
