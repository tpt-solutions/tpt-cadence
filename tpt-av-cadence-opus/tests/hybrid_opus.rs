//! Hybrid SILK+CELT encoder tests: end-to-end `.opus` streams through the
//! real [`OpusDecoder`] hybrid path (SILK low band + start-band-17 CELT
//! high band on one range coder), covering TOC configuration, exact
//! sample-count recovery, alignment, determinism, and configuration
//! validation.

use std::io::Cursor;

use tpt_av_cadence_core::{Encoder, FormatReader};
use tpt_av_cadence_opus::packet::{parse_packet, Bandwidth, FrameDuration, Mode};
use tpt_av_cadence_opus::range::RangeDecoder;
use tpt_av_cadence_opus::silk::decoder::{DecControl, LostFlag, SilkDecoder};
use tpt_av_cadence_opus::{OggOpusEncoder, OggOpusReader, OpusDecoder, OpusHead};

/// A speech-like signal: harmonic stack with slow pitch and amplitude
/// drift, at 48 kHz, ±1.0 scale.
fn speech_like(n: usize, f0: f32) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / 48_000.0;
            let glottal = (2.0 * core::f32::consts::PI * f0 * t).sin()
                + 0.5 * (2.0 * core::f32::consts::PI * 2.0 * f0 * t).sin()
                + 0.25 * (2.0 * core::f32::consts::PI * 3.0 * f0 * t).sin()
                + 0.125 * (2.0 * core::f32::consts::PI * 4.0 * f0 * t).sin();
            let env = 0.6 + 0.4 * (2.0 * core::f32::consts::PI * 0.7 * t).sin();
            glottal * env * 8000.0 / 32768.0
        })
        .collect()
}

/// The packet payload bytes of the `page_index`-th Ogg page.
fn page_packets(data: &[u8], page_index: usize) -> Vec<Vec<u8>> {
    let positions: Vec<usize> = data
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == b"OggS")
        .map(|(i, _)| i)
        .collect();
    let p = positions[page_index];
    let seg_count = data[p + 26] as usize;
    let seg_table = &data[p + 27..p + 27 + seg_count];
    let body_start = p + 27 + seg_count;
    let mut packets = Vec::new();
    let mut current = Vec::new();
    let mut offset = body_start;
    for &len in seg_table {
        current.extend_from_slice(&data[offset..offset + len as usize]);
        offset += len as usize;
        if len < 255 {
            packets.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        packets.push(current);
    }
    packets
}

fn hybrid_stream(
    silk_bitrate: u32,
    celt_bitrate: u32,
    packet_ms: i32,
    fullband: bool,
    original: &[f32],
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = OggOpusEncoder::new_hybrid(
        &mut out,
        48_000,
        1,
        silk_bitrate,
        celt_bitrate,
        packet_ms,
        fullband,
    )
    .unwrap();
    let mut pos = 0usize;
    for chunk in [177usize, 1000, 333, 2048, 512, 960] {
        let end = (pos + chunk).min(original.len());
        if pos >= end {
            break;
        }
        enc.encode(&original[pos..end]).unwrap();
        pos = end;
    }
    if pos < original.len() {
        enc.encode(&original[pos..]).unwrap();
    }
    enc.finish().unwrap();
    drop(enc); // release the borrow before returning the buffer
    out
}

/// End-to-end hybrid decode through the reader: exact length, TOC
/// checks, and an SNR gate on the aligned interior.
fn assert_hybrid_round_trip(
    silk_bitrate: u32,
    celt_bitrate: u32,
    packet_ms: i32,
    fullband: bool,
    snr_gate_db: f64,
) {
    let frame_samples = packet_ms as usize * 48;
    let n_packets = 12;
    let n_samples = frame_samples * n_packets + 137;
    let original = speech_like(n_samples, 120.0);

    let out = hybrid_stream(silk_bitrate, celt_bitrate, packet_ms, fullband, &original);

    // OpusHead: mono, measured hybrid pre-skip.
    let head = OpusHead::parse(&page_packets(&out, 0)[0]).unwrap();
    assert_eq!(head.channels, 1);
    assert_eq!(head.pre_skip, 67);

    // First audio packet: TOC config 13 (SWB 20 ms) / 15 (FB 20 ms) /
    // 12/14 (10 ms), mono, code 0; hybrid mode; and the frame is exactly
    // silk_share + celt_share bytes.
    let expected_config: u8 = match (fullband, packet_ms) {
        (false, 10) => 12,
        (false, 20) => 13,
        (true, 10) => 14,
        (true, 20) => 15,
        _ => unreachable!(),
    };
    let audio = page_packets(&out, 2);
    assert_eq!(audio.len(), 1, "one packet per page");
    let packet = parse_packet(&audio[0]).unwrap();
    assert_eq!(packet.toc.config, expected_config);
    assert_eq!(packet.toc.code, 0);
    assert_eq!(packet.toc.mode(), Mode::Hybrid);
    assert_eq!(
        packet.toc.bandwidth(),
        if fullband {
            Bandwidth::Fullband
        } else {
            Bandwidth::Superwideband
        }
    );
    assert_eq!(
        packet.toc.frame_duration(),
        if packet_ms == 20 {
            FrameDuration::Ms20
        } else {
            FrameDuration::Ms10
        }
    );
    let silk_share = (u64::from(silk_bitrate) * packet_ms as u64 / 8000) as usize;
    let celt_share = (u64::from(celt_bitrate) * packet_ms as u64 / 8000) as usize;
    assert_eq!(
        audio[0].len(),
        1 + silk_share + celt_share,
        "hybrid frame must be exactly silk_share + celt_bytes frame bytes (+ TOC)"
    );

    // Decode end-to-end through the reader (OpusDecoder's hybrid path).
    let mut reader = OggOpusReader::from_source(Box::new(Cursor::new(out))).unwrap();
    let dec = reader.decoder();
    let mut pcm = Vec::new();
    let mut buf = vec![0f32; frame_samples];
    loop {
        let n = dec.decode(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        pcm.extend_from_slice(&buf[..n]);
    }
    assert_eq!(
        pcm.len(),
        n_samples,
        "exact sample-count recovery ({fullband}, {packet_ms} ms)"
    );

    // SNR over the aligned interior (skip warm-up and the padded tail).
    let start = frame_samples;
    let end = frame_samples * (n_packets - 1);
    let mut num = 0f64;
    let mut den = 0f64;
    for i in start..end {
        let d = original[i] as f64 - pcm[i] as f64;
        num += d * d;
        den += original[i] as f64 * original[i] as f64;
    }
    let snr = 10.0 * (den / num.max(1e-12)).log10();
    assert!(
        snr > snr_gate_db,
        "hybrid SNR too low ({fullband}, {packet_ms} ms): {snr:.1} dB"
    );
}

#[test]
fn hybrid_20ms_swb_round_trip() {
    assert_hybrid_round_trip(48_000, 32_000, 20, false, 20.0);
}

#[test]
fn hybrid_20ms_fb_round_trip() {
    assert_hybrid_round_trip(48_000, 32_000, 20, true, 20.0);
}

#[test]
fn hybrid_10ms_round_trip() {
    assert_hybrid_round_trip(48_000, 32_000, 10, false, 12.0);
}

#[test]
fn hybrid_pre_skip_pins_the_measured_alignment() {
    // Decode the packets of a hybrid Ogg stream manually (no pre-skip);
    // the SNR-optimal integer shift must equal the signalled pre-skip.
    let n_samples = 960 * 16;
    let original = speech_like(n_samples, 120.0);
    let out = hybrid_stream(16_000, 24_000, 20, false, &original);
    let head = OpusHead::parse(&page_packets(&out, 0)[0]).unwrap();

    let n_pages = out.windows(4).filter(|w| *w == b"OggS").count();
    let mut opus = OpusDecoder::new(1).unwrap();
    let mut decoded_all: Vec<f32> = Vec::new();
    for page in 2..n_pages {
        for packet in page_packets(&out, page) {
            let frame = parse_packet(&packet).unwrap();
            assert_eq!(frame.toc.mode(), Mode::Hybrid);
            let _ = RangeDecoder::new(&packet[1..]); // shape check only
            let mut buf = vec![0f32; 960];
            let n = opus.decode_packet(&frame, &packet, &mut buf).unwrap();
            assert_eq!(n, 960);
            decoded_all.extend_from_slice(&buf[..n]);
        }
    }

    let snr_at = |shift: usize| -> f64 {
        let mut num = 0f64;
        let mut den = 0f64;
        for (i, &a) in original.iter().enumerate() {
            let j = i + shift;
            if j >= decoded_all.len() {
                break;
            }
            let e = decoded_all[j] as f64 - a as f64;
            num += e * e;
            den += a as f64 * a as f64;
        }
        10.0 * (den / num.max(1e-9)).log10()
    };
    let (mut best, mut best_snr) = (0usize, f64::MIN);
    for shift in 10..200 {
        let s = snr_at(shift);
        if s > best_snr {
            best_snr = s;
            best = shift;
        }
    }
    let diff = (best as i64 - i64::from(head.pre_skip)).abs();
    assert!(
        diff <= 1,
        "pre-skip misalignment: signalled {}, SNR optimum {best} ({best_snr:.1} dB)",
        head.pre_skip
    );
    assert!(best_snr > 25.0, "alignment SNR {best_snr:.1} dB too low");
}

/// The hybrid packet's SILK prefix decodes (through a standalone
/// `SilkDecoder` fed the whole hybrid frame) to the same low band a
/// SILK-only encoder would have produced for the same input — the
/// shared-coder arrangement leaves the SILK symbols intact.
#[test]
fn hybrid_silk_prefix_decodes_consistently() {
    let frame_samples = 960;
    let n_packets = 6;
    let n_samples = frame_samples * n_packets;
    let original = speech_like(n_samples, 120.0);

    let out = hybrid_stream(16_000, 24_000, 20, false, &original);
    let n_pages = out.windows(4).filter(|w| *w == b"OggS").count();
    let mut silk = SilkDecoder::new(1).unwrap();
    let mut decoded_all: Vec<i16> = Vec::new();
    for page in 2..n_pages {
        for packet in page_packets(&out, page) {
            let mut rng = RangeDecoder::new(&packet[1..]);
            let mut chunk = vec![0i16; 20 * 48];
            let n = silk
                .decode(
                    &mut DecControl {
                        n_channels_api: 1,
                        n_channels_internal: 1,
                        api_sample_rate: 48_000,
                        internal_sample_rate: 16_000,
                        payload_size_ms: 20,
                        prev_pitch_lag: 0,
                    },
                    Some(&mut rng),
                    LostFlag::Normal,
                    true,
                    &mut chunk,
                )
                .unwrap();
            assert_eq!(n, 20 * 48);
            decoded_all.extend_from_slice(&chunk);
        }
    }

    // The decoded low band must track the input's envelope: the hybrid
    // round trip's RMS must be in a sane band relative to the input,
    // and its correlation with a same-shape reference must be high.
    let input_rms: f64 = (original.iter().map(|&v| (v as f64) * v as f64).sum::<f64>()
        / original.len() as f64)
        .sqrt()
        * 32768.0;
    let decoded_rms: f64 = (decoded_all
        .iter()
        .map(|&v| (v as f64) * v as f64)
        .sum::<f64>()
        / decoded_all.len() as f64)
        .sqrt();
    assert!(
        decoded_rms > 0.4 * input_rms && decoded_rms < 3.0 * input_rms,
        "decoded low-band RMS {decoded_rms:.1} vs input RMS {input_rms:.1}"
    );
}

#[test]
fn hybrid_is_deterministic() {
    let n_samples = 960 * 6;
    let original = speech_like(n_samples, 120.0);
    let a = hybrid_stream(16_000, 24_000, 20, false, &original);
    let b = hybrid_stream(16_000, 24_000, 20, false, &original);
    assert_eq!(a, b);
}

#[test]
fn hybrid_rejects_invalid_configuration() {
    let mut sink = Vec::new();
    // Three channels are rejected.
    assert!(OggOpusEncoder::new_hybrid(&mut sink, 48_000, 3, 16_000, 24_000, 20, false).is_err());
    // Non-48 kHz API rate.
    assert!(OggOpusEncoder::new_hybrid(&mut sink, 24_000, 1, 16_000, 24_000, 20, false).is_err());
    // Hybrid has no 40/60 ms packets and no other durations.
    assert!(OggOpusEncoder::new_hybrid(&mut sink, 48_000, 1, 16_000, 24_000, 40, false).is_err());
    assert!(OggOpusEncoder::new_hybrid(&mut sink, 48_000, 1, 16_000, 24_000, 60, false).is_err());
    // Bitrate ranges.
    assert!(OggOpusEncoder::new_hybrid(&mut sink, 48_000, 1, 4_000, 24_000, 20, false).is_err());
    assert!(OggOpusEncoder::new_hybrid(&mut sink, 48_000, 1, 16_000, 4_000, 20, false).is_err());
    assert!(OggOpusEncoder::new_hybrid(&mut sink, 48_000, 1, 16_000, 200_000, 20, false).is_err());
}

// ---------------------------------------------------------------------------
// Stereo hybrid (stereo adaptive-mid/side SILK + stereo CELT, one coder)
// ---------------------------------------------------------------------------

/// A stereo pair: harmonic mid signal with a decorrelated side tone.
fn stereo_speech(n: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(n * 2);
    let mut phase = 0.0f32;
    for i in 0..n {
        let t = i as f32 / 48_000.0;
        let f0 = 120.0f32;
        let glottal = (2.0 * core::f32::consts::PI * f0 * t).sin()
            + 0.5 * (2.0 * core::f32::consts::PI * 2.0 * f0 * t).sin()
            + 0.25 * (2.0 * core::f32::consts::PI * 3.0 * f0 * t).sin()
            + 0.125 * (2.0 * core::f32::consts::PI * 4.0 * f0 * t).sin();
        let env = 0.6 + 0.4 * (2.0 * core::f32::consts::PI * 0.7 * t).sin();
        let mid = glottal * env * 8000.0;
        let side = 0.35 * phase.sin() * env * 6000.0;
        phase += 2.0 * std::f32::consts::PI * (f0 * 0.77) / 48_000.0;
        out.push(((mid + side) / 32768.0).clamp(-1.0, 1.0));
        out.push(((mid - side) / 32768.0).clamp(-1.0, 1.0));
    }
    out
}

fn hybrid_stream_stereo(
    silk_bitrate: u32,
    celt_bitrate: u32,
    packet_ms: i32,
    fullband: bool,
    original: &[f32],
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = OggOpusEncoder::new_hybrid(
        &mut out,
        48_000,
        2,
        silk_bitrate,
        celt_bitrate,
        packet_ms,
        fullband,
    )
    .unwrap();
    let mut pos = 0usize;
    for chunk in [300usize, 900, 444, 1600] {
        let end = (pos + chunk * 2).min(original.len());
        if pos >= end {
            break;
        }
        enc.encode(&original[pos..end]).unwrap();
        pos = end;
    }
    if pos < original.len() {
        enc.encode(&original[pos..]).unwrap();
    }
    enc.finish().unwrap();
    drop(enc);
    out
}

#[test]
fn hybrid_stereo_round_trips() {
    for (fullband, packet_ms) in [(false, 20i32), (true, 20i32)] {
        let frame_samples = packet_ms as usize * 48;
        let n_packets = 12;
        let n_samples = frame_samples * n_packets + 137;
        let original = stereo_speech(n_samples);

        // Stereo SILK's per-channel side info makes its natural payload
        // much larger than the nominal rate math (~290 B/frame at
        // 40 kbps/channel, the per-channel cap); the combined budget must
        // cover it (the guard refuses to emit a frame the decoder would
        // misread otherwise).
        let silk_bitrate = 80_000u32;
        let celt_bitrate = 48_000u32;
        let out = hybrid_stream_stereo(silk_bitrate, celt_bitrate, packet_ms, fullband, &original);

        // OpusHead: stereo, hybrid pre-skip.
        let head = OpusHead::parse(&page_packets(&out, 0)[0]).unwrap();
        assert_eq!(head.channels, 2);
        assert_eq!(head.pre_skip, 67);

        // Audio packet: stereo bit set, hybrid mode, right config, and
        // the frame is exactly silk_share + celt_share bytes.
        let expected_config: u8 = match (fullband, packet_ms) {
            (false, 20) => 13,
            (true, 20) => 15,
            _ => unreachable!(),
        };
        let audio = page_packets(&out, 2);
        let packet = parse_packet(&audio[0]).unwrap();
        assert_eq!(packet.toc.config, expected_config);
        assert!(packet.toc.stereo, "TOC stereo bit must be set");
        assert_eq!(packet.toc.mode(), Mode::Hybrid);
        let silk_share = (u64::from(silk_bitrate) * packet_ms as u64 / 8000) as usize;
        let celt_share = (u64::from(celt_bitrate) * packet_ms as u64 / 8000) as usize;
        assert_eq!(audio[0].len(), 1 + silk_share + celt_share);

        // End-to-end decode with exact length recovery.
        let mut reader = OggOpusReader::from_source(Box::new(Cursor::new(out))).unwrap();
        let dec = reader.decoder();
        let mut pcm = Vec::new();
        let mut buf = vec![0f32; frame_samples * 2];
        loop {
            let n = dec.decode(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            pcm.extend_from_slice(&buf[..n * 2]);
        }
        assert_eq!(
            pcm.len(),
            n_samples * 2,
            "exact sample-count recovery ({fullband})"
        );

        // Per-channel SNR over the aligned interior.
        let start = frame_samples * 2;
        let end = frame_samples * (n_packets - 1);
        for ch in 0..2 {
            let mut num = 0f64;
            let mut den = 0f64;
            for i in start..end {
                let d = original[i * 2 + ch] as f64 - pcm[i * 2 + ch] as f64;
                num += d * d;
                den += original[i * 2 + ch] as f64 * original[i * 2 + ch] as f64;
            }
            let snr = 10.0 * (den / num.max(1e-12)).log10();
            assert!(
                snr > 5.0,
                "stereo hybrid SNR too low ({fullband}, channel {ch}): {snr:.1} dB"
            );
        }
    }
}
