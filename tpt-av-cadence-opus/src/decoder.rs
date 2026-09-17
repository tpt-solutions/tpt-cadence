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
