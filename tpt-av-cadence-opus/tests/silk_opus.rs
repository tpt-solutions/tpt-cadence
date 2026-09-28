//! SILK-mode Opus packetization tests: multi-frame payload round trips
//! (bit-exact through the real [`SilkDecoder`], including the
//! conditionally coded frames of 40/60 ms packets), RFC 6716 Table 2 TOC
//! configuration checks, and end-to-end `.opus` files through
//! [`OggOpusReader`] — exact-length recovery, measured pre-skip
//! alignment, and determinism.

use std::io::Cursor;

use tpt_av_cadence_core::{Encoder, FormatReader};
use tpt_av_cadence_opus::packet::{parse_packet, Bandwidth, FrameDuration, Mode};
use tpt_av_cadence_opus::range::RangeDecoder;
use tpt_av_cadence_opus::silk::decoder::{DecControl, LostFlag, SilkDecoder};
use tpt_av_cadence_opus::silk::encoder::SilkEncoder;
use tpt_av_cadence_opus::{OggOpusEncoder, OggOpusReader, OpusHead};

/// A speech-like signal: harmonic stack with slow pitch and amplitude
/// drift, i16 domain, at the given rate.
fn speech_like(n: usize, fs: usize, f0: f32) -> Vec<i16> {
    (0..n)
        .map(|i| {
            let t = i as f32 / fs as f32;
            let glottal = (2.0 * core::f32::consts::PI * f0 * t).sin()
                + 0.5 * (2.0 * core::f32::consts::PI * 2.0 * f0 * t).sin()
                + 0.25 * (2.0 * core::f32::consts::PI * 3.0 * f0 * t).sin()
                + 0.125 * (2.0 * core::f32::consts::PI * 4.0 * f0 * t).sin();
            let env = 0.6 + 0.4 * (2.0 * core::f32::consts::PI * 0.7 * t).sin();
            (glottal * env * 8000.0) as i16
        })
        .collect()
}

/// Simple xorshift PRNG for deterministic noise.
struct Rng(u32);
impl Rng {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    fn noise(&mut self) -> i16 {
        (self.next() % 16000) as i16 - 8000
    }
}

// ---------------------------------------------------------------------------
// Bit-exact payload round trips at the internal rate
// ---------------------------------------------------------------------------

/// The decoder-side delay of the same-rate (copy) resampler
/// (`DELAY_MATRIX_DEC[fs][fs]`: 4 at 8 kHz, 9 at 12 kHz, 12 at 16 kHz).
fn decoder_stream_delay(internal_rate: i32) -> usize {
    match internal_rate {
        8_000 => 4,
        12_000 => 9,
        _ => 12,
    }
}

/// Builds the stream the decoder's per-frame resampler sees: per SILK
/// frame the input is `[previous frame's last sample, xq[0..n-1]]`, so
/// the whole stream is the reconstruction delayed by exactly one sample.
fn expected_decoder_stream(frames_xq: &[&[i16]], frame_len: usize) -> Vec<i16> {
    let mut stream = Vec::new();
    let mut prev_last = 0i16;
    for xq in frames_xq {
        assert_eq!(xq.len(), frame_len);
        stream.push(prev_last);
        stream.extend_from_slice(&xq[..frame_len - 1]);
        prev_last = xq[frame_len - 1];
    }
    stream
}

/// Encodes `packets` payloads at `api == internal_rate` (so the decoder's
/// resampler is a delay-line passthrough and the comparison is
/// bit-exact), decoding every SILK frame of every payload with a
/// `SilkDecoder`, and returns (decoded, expected) pairs per frame.
fn round_trip_exact(internal_rate: i32, packet_ms: i32, signal: &[i16], packets: usize) {
    let fs_khz = (internal_rate / 1000) as u32;
    let frame_len = 20 * fs_khz as usize; // multi-frame packets use 20 ms frames
    let frames_per_packet: usize = match packet_ms {
        10 => 1,
        20 => 1,
        40 => 2,
        60 => 3,
        _ => panic!("bad packet_ms"),
    };
    let frame_len_internal = if packet_ms == 10 {
        10 * fs_khz as usize
    } else {
        frame_len
    };
    let packet_len_api = packet_ms as usize * fs_khz as usize;

    let mut enc = SilkEncoder::new(internal_rate, internal_rate, packet_ms).unwrap();
    enc.set_bitrate(24_000);
    let mut dec = SilkDecoder::new(1).unwrap();

    let mut all_decoded: Vec<Vec<i16>> = Vec::new();
    let mut all_simulated: Vec<Vec<i16>> = Vec::new();
    for p in 0..packets {
        let input = &signal[p * packet_len_api..(p + 1) * packet_len_api];
        let payload = enc.encode_frame(input).unwrap();
        let simulated = enc.last_reconstructed_frame().to_vec();
        assert_eq!(simulated.len(), frame_len_internal * frames_per_packet);

        let mut decoded = vec![0i16; frame_len_internal * frames_per_packet];
        let mut rng = RangeDecoder::new(&payload);
        for f in 0..frames_per_packet {
            let mut ctrl = DecControl {
                n_channels_api: 1,
                n_channels_internal: 1,
                api_sample_rate: internal_rate,
                internal_sample_rate: internal_rate,
                payload_size_ms: packet_ms,
                prev_pitch_lag: 0,
            };
            let chunk = &mut decoded[f * frame_len_internal..(f + 1) * frame_len_internal];
            let n = dec
                .decode(&mut ctrl, Some(&mut rng), LostFlag::Normal, f == 0, chunk)
                .unwrap();
            assert_eq!(n, frame_len_internal, "packet {p} frame {f}");
        }
        for f in 0..frames_per_packet {
            all_decoded
                .push(decoded[f * frame_len_internal..(f + 1) * frame_len_internal].to_vec());
            all_simulated
                .push(simulated[f * frame_len_internal..(f + 1) * frame_len_internal].to_vec());
        }
    }

    /* Rebuild the expected decoder output: the headered reconstruction
     * stream delayed by the decoder resampler's copy delay. */
    let delay = decoder_stream_delay(internal_rate);
    let per_frame: Vec<&[i16]> = all_simulated.iter().map(|v| v.as_slice()).collect();
    let stream = expected_decoder_stream(&per_frame, frame_len_internal);
    for (f, decoded) in all_decoded.iter().enumerate() {
        let expected: Vec<i16> = (0..frame_len_internal)
            .map(|i| {
                let j = f * frame_len_internal + i;
                if j >= delay {
                    stream[j - delay]
                } else {
                    0
                }
            })
            .collect();
        assert_eq!(
            decoded, &expected,
            "frame {f}: decoder output diverged from the encoder simulation \
             ({internal_rate} Hz, {packet_ms} ms packet)"
        );
    }
}

#[test]
fn multi_frame_payloads_round_trip_bit_exactly() {
    for &(rate, ms) in &[
        (16_000i32, 20i32),
        (16_000, 40),
        (16_000, 60),
        (8_000, 40),
        (8_000, 60),
        (12_000, 60),
        (12_000, 10),
    ] {
        let fs = (rate / 1000) as usize;
        let packet_len = ms as usize * fs;
        // Mixed material long enough for six packets: speech-like, noise,
        // silence — so the conditional frames see voiced, unvoiced, and
        // inactive predecessors (pitch delta coding on and off).
        let mut rng = Rng(0x5EED);
        let mut signal = speech_like(packet_len * 2, fs, 120.0);
        signal.extend((0..packet_len * 2).map(|_| rng.noise()));
        signal.extend(std::iter::repeat(0i16).take(packet_len * 2));

        round_trip_exact(rate, ms, &signal, 6);
    }
}

// ---------------------------------------------------------------------------
// Ogg end-to-end (48 kHz API)
// ---------------------------------------------------------------------------

/// The packet payload bytes of the `page_index`-th Ogg page (the test
/// files have one packet per page; pages carry whole packets).
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

fn ogg_silk_stream(internal_rate: i32, packet_ms: i32, bitrate: u32, original: &[f32]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc =
        OggOpusEncoder::new_silk(&mut out, 48_000, 1, bitrate, internal_rate, packet_ms).unwrap();
    // Feed in irregular chunks to exercise the encoder's internal
    // frame-boundary buffering.
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
    drop(enc); // release the mutable borrow before the buffer is returned
    out
}

#[test]
fn ogg_silk_round_trip_recovers_exact_length_and_alignment() {
    let expected_pre_skip = |rate: i32| -> u16 {
        match rate {
            8_000 => 68,
            12_000 => 65,
            _ => 67,
        }
    };

    for internal in [8_000i32, 12_000, 16_000] {
        let packet_ms = 20i32;
        let n_packets = 12;
        let n_samples = 960 * n_packets + 137; // deliberately ragged tail
        let speech = speech_like(n_samples, 48_000, 120.0);
        let original: Vec<f32> = speech.iter().map(|&v| v as f32 / 32768.0).collect();

        let out = ogg_silk_stream(internal, packet_ms, 30_000, &original);

        // OpusHead: mono, measured pre-skip for this internal rate.
        let head_packet = &page_packets(&out, 0)[0];
        let head = OpusHead::parse(head_packet).unwrap();
        assert_eq!(head.channels, 1);
        assert_eq!(head.input_sample_rate, 48_000);
        assert_eq!(head.pre_skip, expected_pre_skip(internal));

        // Audio packets: TOC config matches (bandwidth, 20 ms, code 0).
        let expected_config = match internal {
            8_000 => 1u8,
            12_000 => 5,
            _ => 9,
        };
        let audio = page_packets(&out, 2);
        assert_eq!(audio.len(), 1, "one packet per page");
        let packet = parse_packet(&audio[0]).unwrap();
        assert_eq!(packet.toc.config, expected_config);
        assert_eq!(packet.toc.code, 0);
        assert_eq!(packet.toc.mode(), Mode::Silk);
        assert_eq!(packet.toc.frame_duration(), FrameDuration::Ms20);
        assert_eq!(
            packet.toc.bandwidth(),
            match internal {
                8_000 => Bandwidth::Narrowband,
                12_000 => Bandwidth::Mediumband,
                _ => Bandwidth::Wideband,
            }
        );

        // Final granule ends after the pre-skip delay.
        let final_page = out.windows(4).rposition(|w| w == b"OggS").unwrap();
        let final_granule =
            i64::from_le_bytes(out[final_page + 6..final_page + 14].try_into().unwrap());
        assert_eq!(final_granule, n_samples as i64 + i64::from(head.pre_skip));

        // End-to-end decode: exact length, high SNR at zero residual lag
        // (the decoder applied pre-skip for us).
        let mut reader = OggOpusReader::from_source(Box::new(Cursor::new(out))).unwrap();
        let dec = reader.decoder();
        let mut pcm = Vec::new();
        let mut buf = vec![0f32; 960];
        loop {
            let n = dec.decode(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            pcm.extend_from_slice(&buf[..n]);
        }
        assert_eq!(pcm.len(), n_samples, "exact sample-count recovery");

        // SNR over the aligned interior (skip the first packet where the
        // decoder's filters warm up and the ragged zero-padded tail).
        let start = 960;
        let end = 960 * (n_packets - 1);
        let mut num = 0f64;
        let mut den = 0f64;
        for i in start..end {
            let d = original[i] as f64 - pcm[i] as f64;
            num += d * d;
            den += original[i] as f64 * original[i] as f64;
        }
        let snr = 10.0 * (den / num.max(1e-12)).log10();
        assert!(
            snr > 15.0,
            "SILK Ogg round-trip SNR too low at {internal} Hz internal: {snr:.1} dB"
        );
    }
}

#[test]
fn ogg_silk_pre_skip_pins_the_measured_alignment() {
    // Decode the packets of a SILK Ogg stream manually (no pre-skip) and
    // check the SNR-optimal integer shift equals the signalled pre-skip.
    for internal in [8_000i32, 12_000, 16_000] {
        let packet_ms = 20i32;
        let n_packets = 20;
        let n_samples = 960 * n_packets;
        let speech = speech_like(n_samples, 48_000, 120.0);
        let original: Vec<f32> = speech.iter().map(|&v| v as f32 / 32768.0).collect();
        let original_i16: Vec<i16> = original
            .iter()
            .map(|&s| {
                let v = (s * 32768.0).round();
                v.clamp(-32768.0, 32767.0) as i16
            })
            .collect();

        let out = ogg_silk_stream(internal, packet_ms, 30_000, &original);
        let head = OpusHead::parse(&page_packets(&out, 0)[0]).unwrap();

        // Decode every audio packet at the 48 kHz API rate.
        let n_pages = out.windows(4).filter(|w| *w == b"OggS").count();
        let mut dec = SilkDecoder::new(1).unwrap();
        let mut decoded_all: Vec<i16> = Vec::new();
        for page in 2..n_pages {
            for packet in page_packets(&out, page) {
                let toc = parse_packet(&packet).unwrap().toc;
                assert_eq!(toc.mode(), Mode::Silk);
                let mut rng = RangeDecoder::new(&packet[1..]);
                let frame_api = 20 * 48;
                let mut chunk = vec![0i16; frame_api];
                let n = dec
                    .decode(
                        &mut DecControl {
                            n_channels_api: 1,
                            n_channels_internal: 1,
                            api_sample_rate: 48_000,
                            internal_sample_rate: internal,
                            payload_size_ms: packet_ms,
                            prev_pitch_lag: 0,
                        },
                        Some(&mut rng),
                        LostFlag::Normal,
                        true,
                        &mut chunk,
                    )
                    .unwrap();
                assert_eq!(n, frame_api);
                decoded_all.extend_from_slice(&chunk);
            }
        }

        // SNR(shift) with the input; the sharp optimum must sit at the
        // signalled pre-skip (±1 sample tolerance for the ragged content
        // boundary effects).
        let snr_at = |shift: usize| -> f64 {
            let mut num = 0f64;
            let mut den = 0f64;
            for (i, &a) in original_i16.iter().enumerate() {
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
        for shift in 10..220 {
            let s = snr_at(shift);
            if s > best_snr {
                best_snr = s;
                best = shift;
            }
        }
        let diff = (best as i64 - i64::from(head.pre_skip)).abs();
        assert!(
            diff <= 1,
            "pre-skip misalignment at {internal} Hz: signalled {}, SNR optimum {best} ({best_snr:.1} dB)",
            head.pre_skip
        );
        assert!(best_snr > 20.0, "alignment SNR {best_snr:.1} dB too low");
    }
}

#[test]
fn ogg_silk_40_and_60_ms_packets_round_trip() {
    for packet_ms in [40i32, 60i32] {
        let n_packets = 5;
        let n_samples = 960 * 20 * n_packets + 11;
        let speech = speech_like(n_samples, 48_000, 120.0);
        let original: Vec<f32> = speech.iter().map(|&v| v as f32 / 32768.0).collect();

        let out = ogg_silk_stream(16_000, packet_ms, 24_000, &original);

        // Audio packets carry the right multi-frame configuration
        // (config 10 for 40 ms, 11 for 60 ms at 16 kHz internal).
        let expected_config = if packet_ms == 40 { 10u8 } else { 11 };
        let audio = page_packets(&out, 2);
        let packet = parse_packet(&audio[0]).unwrap();
        assert_eq!(packet.toc.config, expected_config);
        assert_eq!(
            packet.toc.frame_duration(),
            match packet_ms {
                40 => FrameDuration::Ms40,
                _ => FrameDuration::Ms60,
            }
        );

        let mut reader = OggOpusReader::from_source(Box::new(Cursor::new(out))).unwrap();
        let dec = reader.decoder();
        let mut pcm = Vec::new();
        let mut buf = vec![0f32; 2048];
        loop {
            let n = dec.decode(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            pcm.extend_from_slice(&buf[..n]);
        }
        assert_eq!(pcm.len(), n_samples, "exact recovery at {packet_ms} ms");

        let start = 960 * 20;
        let end = 960 * 20 * (n_packets - 1);
        let mut num = 0f64;
        let mut den = 0f64;
        for i in start..end {
            let d = original[i] as f64 - pcm[i] as f64;
            num += d * d;
            den += original[i] as f64 * original[i] as f64;
        }
        let snr = 10.0 * (den / num.max(1e-12)).log10();
        assert!(snr > 15.0, "SNR {snr:.1} dB too low at {packet_ms} ms");
    }
}

#[test]
fn ogg_silk_is_deterministic() {
    let n_samples = 960 * 6;
    let speech = speech_like(n_samples, 48_000, 120.0);
    let original: Vec<f32> = speech.iter().map(|&v| v as f32 / 32768.0).collect();

    let a = ogg_silk_stream(16_000, 20, 30_000, &original);
    let b = ogg_silk_stream(16_000, 20, 30_000, &original);
    assert_eq!(a, b);
}

#[test]
fn ogg_silk_rejects_invalid_configuration() {
    let mut sink = Vec::new();
    // Non-48 kHz API rate (pre-skip constants are measured at 48 kHz).
    assert!(OggOpusEncoder::new_silk(&mut sink, 24_000, 1, 20_000, 16_000, 20).is_err());
    // Unsupported internal rates and packet durations.
    assert!(OggOpusEncoder::new_silk(&mut sink, 48_000, 1, 20_000, 24_000, 20).is_err());
    assert!(OggOpusEncoder::new_silk(&mut sink, 48_000, 1, 20_000, 16_000, 30).is_err());
    // Bitrate outside the supported target range.
    assert!(OggOpusEncoder::new_silk(&mut sink, 48_000, 1, 4_000, 16_000, 20).is_err());
    assert!(OggOpusEncoder::new_silk(&mut sink, 48_000, 1, 65_000, 16_000, 20).is_err());
}

#[test]
fn ogg_silk_packet_bitrates_steer_payload_size() {
    // The bitrate target must move the average audio packet size.
    let n_samples = 960 * 10;
    let speech = speech_like(n_samples, 48_000, 120.0);
    let original: Vec<f32> = speech.iter().map(|&v| v as f32 / 32768.0).collect();

    let avg_packet = |bitrate: u32| -> f64 {
        let out = ogg_silk_stream(16_000, 20, bitrate, &original);
        let n_pages = out.windows(4).filter(|w| *w == b"OggS").count();
        let sizes: Vec<usize> = (2..n_pages - 1)
            .map(|p| page_packets(&out, p)[0].len())
            .collect();
        sizes.iter().sum::<usize>() as f64 / sizes.len() as f64
    };

    // Budgets: 8 B vs 20 B per channel per packet. Both sit at/near the
    // quantizer's payload floor on this material, so compare 16k vs 64k.
    let low = avg_packet(16_000);
    let high = avg_packet(64_000);
    assert!(
        high > low * 1.25,
        "bitrate target must move payload size: 16 kbps → {low:.1} B/pkt, 64 kbps → {high:.1} B/pkt"
    );
}

// ---------------------------------------------------------------------------
// Stereo (adaptive mid/side)
// ---------------------------------------------------------------------------

/// A stereo pair: a harmonic mid signal and a decorrelated-but-related
/// side (delayed, inverted, and level-shifted speech) at 48 kHz.
fn stereo_speech(n: usize, f0: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(n * 2);
    let mut phase = 0.0f32;
    for i in 0..n {
        let t = i as f32 / 48_000.0;
        let glottal = (2.0 * core::f32::consts::PI * f0 * t).sin()
            + 0.5 * (2.0 * core::f32::consts::PI * 2.0 * f0 * t).sin()
            + 0.25 * (2.0 * core::f32::consts::PI * 3.0 * f0 * t).sin()
            + 0.125 * (2.0 * core::f32::consts::PI * 4.0 * f0 * t).sin();
        let env = 0.6 + 0.4 * (2.0 * core::f32::consts::PI * 0.7 * t).sin();
        let mid = glottal * env * 8000.0;
        let side = 0.35 * phase.sin() * env * 6000.0;
        phase += 2.0 * std::f32::consts::PI * (f0 * 0.77) / 48_000.0;
        let l = ((mid + side) / 32768.0).clamp(-1.0, 1.0);
        let r = ((mid - side) / 32768.0).clamp(-1.0, 1.0);
        out.push(l);
        out.push(r);
        let _ = i;
    }
    out
}

fn ogg_silk_stream_stereo(
    internal: i32,
    packet_ms: i32,
    bitrate: u32,
    original: &[f32],
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc =
        OggOpusEncoder::new_silk(&mut out, 48_000, 2, bitrate, internal, packet_ms).unwrap();
    let mut pos = 0usize;
    for chunk in [254usize, 1000, 512, 2048] {
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

fn decode_all_reader(out: Vec<u8>, channels: usize) -> Vec<f32> {
    let mut reader = OggOpusReader::from_source(Box::new(Cursor::new(out))).unwrap();
    let dec = reader.decoder();
    let mut pcm = Vec::new();
    let mut buf = vec![0f32; 960 * channels];
    loop {
        let n = dec.decode(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        pcm.extend_from_slice(&buf[..n * channels]);
    }
    pcm
}

#[test]
fn ogg_silk_stereo_round_trip_recovers_exact_length() {
    for internal in [16_000i32, 8_000] {
        let n_samples = 960 * 12 + 137;
        let original = stereo_speech(n_samples, 120.0);
        let out = ogg_silk_stream_stereo(internal, 20, 32_000, &original);

        // OpusHead: stereo, measured pre-skip.
        let head = OpusHead::parse(&page_packets(&out, 0)[0]).unwrap();
        assert_eq!(head.channels, 2);
        let expected_pre_skip = if internal == 8_000 { 68 } else { 67 };
        assert_eq!(head.pre_skip, expected_pre_skip);

        // Audio packet TOC: stereo bit set, SILK mode, right bandwidth.
        let packet = parse_packet(&page_packets(&out, 2)[0]).unwrap();
        assert!(packet.toc.stereo, "TOC stereo bit must be set");
        assert_eq!(packet.toc.mode(), Mode::Silk);
        assert_eq!(packet.toc.config, if internal == 8_000 { 1 } else { 9 });

        let pcm = decode_all_reader(out, 2);
        assert_eq!(pcm.len(), n_samples * 2, "exact sample-count recovery");

        // Per-channel SNR over the aligned interior.
        let start = 960 * 2;
        let end = 960 * 11;
        for ch in 0..2 {
            let mut num = 0f64;
            let mut den = 0f64;
            for i in start..end {
                let d = original[i * 2 + ch] as f64 - pcm[i * 2 + ch] as f64;
                num += d * d;
                den += original[i * 2 + ch] as f64 * original[i * 2 + ch] as f64;
            }
            let snr = 10.0 * (den / num.max(1e-12)).log10();
            let gate = if ch == 0 { 8.0 } else { 6.0 };
            assert!(
                snr > gate,
                "stereo SILK round-trip SNR too low at {internal} Hz (channel {ch}): {snr:.1} dB"
            );
        }
    }
}

/// Near-mono content (identical channels): the side residual collapses
/// and the encoder must skip the side channel (mid-only frames), while
/// the decode still reproduces both channels.
#[test]
fn ogg_silk_stereo_mid_only_skips_side_for_near_mono() {
    let n_samples = 960 * 10;
    let mono = speech_like(n_samples, 48_000, 120.0);
    // Identical left/right channels (properly interleaved): the side
    // residual collapses to near-zero, so every frame must go mid-only.
    let original: Vec<f32> = mono
        .iter()
        .flat_map(|&v| [v as f32 / 32768.0, v as f32 / 32768.0])
        .collect();

    let out = ogg_silk_stream_stereo(16_000, 20, 24_000, &original);

    // Mid-only frames produce visibly smaller packets than true stereo:
    // compare against the same content with a real side signal.
    let stereo_ref = stereo_speech(n_samples, 120.0);
    let out_stereo = ogg_silk_stream_stereo(16_000, 20, 24_000, &stereo_ref);
    let avg = |data: &[u8]| -> f64 {
        let n_pages = data.windows(4).filter(|w| *w == b"OggS").count();
        let sizes: Vec<usize> = (2..n_pages - 1)
            .map(|p| page_packets(data, p)[0].len())
            .collect();
        sizes.iter().sum::<usize>() as f64 / sizes.len() as f64
    };
    let near_mono_size = avg(&out);
    let true_stereo_size = avg(&out_stereo);
    assert!(
        near_mono_size * 1.5 < true_stereo_size,
        "near-mono stereo must skip side frames: {near_mono_size:.0} B vs true stereo {true_stereo_size:.0} B"
    );

    // The decode still reproduces both channels at full quality.
    let pcm = decode_all_reader(out, 2);
    assert_eq!(pcm.len(), n_samples * 2);
    let start = 960 * 2;
    let end = 960 * 9;
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
            snr > 20.0,
            "near-mono decode SNR too low (channel {ch}): {snr:.1} dB"
        );
    }
}

#[test]
fn ogg_silk_stereo_pre_skip_pins_the_measured_alignment() {
    // Stereo alignment uses the same measured constants as mono; verify
    // via manual packet decode (no pre-skip) and SNR argmax per channel.
    let n_samples = 960 * 16;
    let original = stereo_speech(n_samples, 120.0);
    let original_i16: Vec<Vec<i16>> = (0..2)
        .map(|ch| {
            original
                .iter()
                .skip(ch)
                .step_by(2)
                .map(|&s| {
                    let v = (s * 32768.0).round();
                    v.clamp(-32768.0, 32767.0) as i16
                })
                .collect()
        })
        .collect();

    let out = ogg_silk_stream_stereo(16_000, 20, 24_000, &original);
    let head = OpusHead::parse(&page_packets(&out, 0)[0]).unwrap();
    let n_pages = out.windows(4).filter(|w| *w == b"OggS").count();
    let mut silk = SilkDecoder::new(2).unwrap();
    let mut decoded: [Vec<i16>; 2] = [Vec::new(), Vec::new()];
    for page in 2..n_pages {
        for packet in page_packets(&out, page) {
            let mut rng = RangeDecoder::new(&packet[1..]);
            let mut chunk = vec![0i16; 2 * 20 * 48];
            let n = silk
                .decode(
                    &mut DecControl {
                        n_channels_api: 2,
                        n_channels_internal: 2,
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
            // The return is the per-channel count; the write is interleaved.
            assert_eq!(n, 20 * 48);
            for ch in 0..2 {
                decoded[ch].extend(chunk[ch..].iter().step_by(2).copied());
            }
        }
    }

    for ch in 0..2 {
        let snr_at = |shift: usize| -> f64 {
            let mut num = 0f64;
            let mut den = 0f64;
            for (i, &a) in original_i16[ch].iter().enumerate() {
                let j = i + shift;
                if j >= decoded[ch].len() {
                    break;
                }
                let e = decoded[ch][j] as f64 - a as f64;
                num += e * e;
                den += a as f64 * a as f64;
            }
            10.0 * (den / num.max(1e-9)).log10()
        };
        let (mut best, mut best_snr) = (0usize, f64::MIN);
        for shift in 10..200 {
            let v = snr_at(shift);
            if v > best_snr {
                best_snr = v;
                best = shift;
            }
        }
        // The mid channel pins the signalled pre-skip exactly; the side
        // channel's decorrelated content sits a few samples off through
        // the dispersive pair (the documented content-weighted
        // compromise).
        let tolerance = if ch == 0 { 1 } else { 3 };
        let diff = (best as i64 - i64::from(head.pre_skip)).abs();
        assert!(
            diff <= tolerance,
            "stereo pre-skip misalignment (channel {ch}): signalled {}, optimum {best} ({best_snr:.1} dB)",
            head.pre_skip
        );
        assert!(
            best_snr > 6.0,
            "stereo alignment SNR {best_snr:.1} dB too low"
        );
    }
}

// ---------------------------------------------------------------------------
// CBR payload sizing
// ---------------------------------------------------------------------------

#[test]
fn ogg_silk_cbr_constant_packet_size() {
    // Every audio packet is exactly the nominal byte count (+ TOC), and
    // padded payloads decode at full quality (trailing zero bytes are
    // never read by the SILK decoder).
    // Budgets sit above the quantizer's ~85 B/frame active-speech
    // payload floor at 16 kHz internal (mono 64 kbps → 160 B, stereo
    // 128 kbps → 160 B per channel).
    for (channels, bitrate) in [(1u16, 64_000u32), (2, 128_000)] {
        let n_samples = 960 * 10 + 137;
        let original = if channels == 2 {
            stereo_speech(n_samples, 120.0)
        } else {
            speech_like(n_samples, 48_000, 120.0)
                .iter()
                .map(|&v| v as f32 / 32768.0)
                .collect()
        };

        let mut out = Vec::new();
        let mut enc =
            OggOpusEncoder::new_silk_cbr(&mut out, 48_000, channels, bitrate, 16_000, 20).unwrap();
        enc.encode(&original).unwrap();
        enc.finish().unwrap();
        drop(enc);

        // All audio packets must have the identical size.
        let n_pages = out.windows(4).filter(|w| *w == b"OggS").count();
        let expected = bitrate as usize / 400 + 1;
        let mut sizes = Vec::new();
        for page in 2..n_pages {
            for packet in page_packets(&out, page) {
                assert_eq!(packet.len(), expected, "CBR packet size");
                sizes.push(packet.len());
            }
        }
        assert!(!sizes.is_empty());

        // Decode and gate the SNR (per channel for stereo).
        let pcm = decode_all_reader(out, channels as usize);
        assert_eq!(pcm.len(), n_samples * channels as usize);
        let start = 960 * 2;
        let end = 960 * 9;
        for ch in 0..channels as usize {
            let mut num = 0f64;
            let mut den = 0f64;
            for i in start..end {
                let d = original[i * channels as usize + ch] as f64
                    - pcm[i * channels as usize + ch] as f64;
                num += d * d;
                den += original[i * channels as usize + ch] as f64
                    * original[i * channels as usize + ch] as f64;
            }
            let snr = 10.0 * (den / num.max(1e-12)).log10();
            let gate = if channels == 2 { 5.0 } else { 8.0 };
            assert!(
                snr > gate,
                "CBR decode SNR too low ({channels} ch, {bitrate} bps, ch {ch}): {snr:.1} dB"
            );
        }
    }
}

#[test]
fn ogg_silk_cbr_deterministic() {
    let n_samples = 960 * 6;
    let original: Vec<f32> = speech_like(n_samples, 48_000, 120.0)
        .iter()
        .map(|&v| v as f32 / 32768.0)
        .collect();
    let build = || {
        let mut out = Vec::new();
        let mut enc =
            OggOpusEncoder::new_silk_cbr(&mut out, 48_000, 1, 64_000, 16_000, 20).unwrap();
        enc.encode(&original).unwrap();
        enc.finish().unwrap();
        drop(enc);
        out
    };
    assert_eq!(build(), build());
}

#[test]
fn ogg_silk_cbr_rejects_absurd_sizes() {
    // Below one silent frame's minimum size, CBR sizing cannot succeed.
    let mut out = Vec::new();
    let mut enc = OggOpusEncoder::new_silk_cbr(&mut out, 48_000, 1, 5_000, 16_000, 20);
    assert!(
        enc.is_err() || {
            // If construction succeeds (12-byte target), the encode must
            // fail cleanly rather than hang or emit a corrupt stream.
            let r = enc.as_mut().unwrap().encode(&[0f32; 960 * 4]);
            match r {
                Err(_) => true,
                Ok(_) => enc.unwrap().finish().is_err(),
            }
        }
    );
}

// ---------------------------------------------------------------------------
// Discontinuous transmission (DTX)
// ---------------------------------------------------------------------------

#[test]
fn ogg_silk_dtx_emits_one_byte_packets_during_silence() {
    // Speech (12 packets) -> silence (30 packets) -> speech (12 packets):
    // during silence the first 20 inactive frames are still coded; after
    // that packets collapse to 1 byte (TOC only) until speech resumes.
    let packets = |dtx: bool| {
        let n = 960 * 54 + 137;
        let mut original: Vec<f32> = Vec::new();
        let mut phase = 0.0f32;
        let push_speech = |out: &mut Vec<f32>, count: usize, phase: &mut f32| {
            for i in 0..count {
                let t = i as f32 / 48_000.0;
                let f0 = 120.0f32;
                let g = (2.0 * core::f32::consts::PI * f0 * t).sin()
                    + 0.5 * (2.0 * core::f32::consts::PI * 2.0 * f0 * t).sin();
                let v = g * 8000.0 / 32768.0;
                out.push(v);
                *phase += 1.0;
            }
        };
        push_speech(&mut original, 960 * 12, &mut phase);
        original.extend(std::iter::repeat(0f32).take(960 * 30));
        push_speech(&mut original, 960 * 12, &mut phase);
        original.resize(n, 0.0); // ragged zero tail to exactly n samples

        let mut out = Vec::new();
        let mut enc = if dtx {
            OggOpusEncoder::new_silk_dtx(&mut out, 48_000, 1, 32_000, 16_000, 20).unwrap()
        } else {
            OggOpusEncoder::new_silk(&mut out, 48_000, 1, 32_000, 16_000, 20).unwrap()
        };
        enc.encode(&original).unwrap();
        enc.finish().unwrap();
        drop(enc);
        out
    };

    let with_dtx = packets(true);
    let without_dtx = packets(false);

    // Packet sizes per audio page.
    let sizes = |data: &[u8]| -> Vec<usize> {
        let n_pages = data.windows(4).filter(|w| *w == b"OggS").count();
        (2..n_pages)
            .flat_map(|p| page_packets(data, p).into_iter().map(|pk| pk.len()))
            .collect()
    };

    let dtx_sizes = sizes(&with_dtx);
    let vbr_sizes = sizes(&without_dtx);
    assert_eq!(dtx_sizes.len(), vbr_sizes.len(), "same packet count");

    // DTX stream: the silence tail must contain 1-byte packets; the VBR
    // stream without DTX must have none.
    assert!(
        dtx_sizes.contains(&1),
        "DTX stream must emit 1-byte packets during silence"
    );
    assert!(
        vbr_sizes.iter().all(|&s| s > 1),
        "non-DTX stream must never emit 1-byte packets"
    );
    // The initial speech and the first 20 inactive frames stay coded.
    assert!(
        dtx_sizes[..30].iter().all(|&s| s > 1),
        "the first 20 inactive frames are still coded (reference schedule)"
    );

    // Decode: exact length, silence stays near-silent, final speech
    // recovers.
    let pcm = decode_all_reader(with_dtx, 1);
    assert_eq!(pcm.len(), 960 * 54 + 137);
    let silence_start = 960 * 24; // deep in the CNG region
    let silence_rms: f64 = (pcm[silence_start..960 * 40]
        .iter()
        .map(|&v| (v as f64) * v as f64)
        .sum::<f64>()
        / (960 * 16) as f64)
        .sqrt();
    assert!(
        silence_rms < 0.01,
        "CNG silence should be near-silent, RMS {silence_rms:.4}"
    );
    let tail_rms: f64 = (pcm[960 * 48..960 * 53]
        .iter()
        .map(|&v| (v as f64) * v as f64)
        .sum::<f64>()
        / (960 * 5) as f64)
        .sqrt();
    assert!(
        tail_rms > 0.05,
        "post-DTX speech must have real energy, RMS {tail_rms:.4}"
    );
}

#[test]
fn ogg_silk_dtx_is_deterministic() {
    let n = 960 * 40 + 7;
    let mut original: Vec<f32> = speech_like(960 * 8, 48_000, 120.0)
        .iter()
        .map(|&v| v as f32 / 32768.0)
        .collect();
    original.extend(std::iter::repeat(0f32).take(n - original.len()));
    let build = || {
        let mut out = Vec::new();
        let mut enc =
            OggOpusEncoder::new_silk_dtx(&mut out, 48_000, 1, 32_000, 16_000, 20).unwrap();
        enc.encode(&original).unwrap();
        enc.finish().unwrap();
        drop(enc);
        out
    };
    assert_eq!(build(), build());
}

// ---------------------------------------------------------------------------
// Low-bitrate redundancy (LBRR / FEC)
// ---------------------------------------------------------------------------

fn ogg_silk_stream_lbrr(lbrr: bool, original: &[f32]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = OggOpusEncoder::new_silk(&mut out, 48_000, 1, 32_000, 16_000, 20).unwrap();
    if lbrr {
        enc.set_packet_loss_perc(20).unwrap();
    }
    let mut pos = 0usize;
    for chunk in [250usize, 1000, 700, 1500] {
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
    drop(enc);
    out
}

/// The LBRR data rides in front of the regular frames; the regular
/// frames' serialization is unchanged, so both streams must decode to
/// IDENTICAL PCM, with the LBRR stream's packets visibly larger.
#[test]
fn ogg_silk_lbrr_stream_decodes_identically_and_grows() {
    let n_samples = 960 * 10 + 137;
    let original = speech_like(n_samples, 48_000, 120.0)
        .iter()
        .map(|&v| v as f32 / 32768.0)
        .collect::<Vec<f32>>();

    let plain = ogg_silk_stream_lbrr(false, &original);
    let with = ogg_silk_stream_lbrr(true, &original);

    let avg = |data: &[u8]| -> f64 {
        let n_pages = data.windows(4).filter(|w| *w == b"OggS").count();
        let sizes: Vec<usize> = (2..n_pages - 1)
            .map(|p| page_packets(data, p)[0].len())
            .collect();
        sizes.iter().sum::<usize>() as f64 / sizes.len() as f64
    };
    let plain_size = avg(&plain);
    let lbrr_size = avg(&with);
    assert!(
        lbrr_size > plain_size * 1.4,
        "LBRR packets must grow: plain {plain_size:.0} B vs LBRR {lbrr_size:.0} B"
    );

    let pcm_plain = decode_all_reader(plain, 1);
    let pcm_lbrr = decode_all_reader(with, 1);
    assert_eq!(pcm_plain.len(), n_samples);
    assert_eq!(pcm_lbrr.len(), n_samples);
    assert_eq!(
        pcm_plain, pcm_lbrr,
        "LBRR data must not alter the decoded regular frames"
    );
}

/// A stream with DTX and LBRR both enabled stays decodable: DTX-skipped
/// packets carry no LBRR, and coding after the gap re-syncs.
#[test]
fn ogg_silk_lbrr_with_dtx_round_trips() {
    let n = 960 * 30 + 100;
    let mut original: Vec<f32> = speech_like(960 * 8, 48_000, 120.0)
        .iter()
        .map(|&v| v as f32 / 32768.0)
        .collect();
    original.resize(n, 0.0);

    let mut out = Vec::new();
    let mut enc = OggOpusEncoder::new_silk_dtx(&mut out, 48_000, 1, 32_000, 16_000, 20).unwrap();
    enc.set_packet_loss_perc(20).unwrap();
    enc.encode(&original).unwrap();
    enc.finish().unwrap();
    drop(enc);

    let pcm = decode_all_reader(out, 1);
    assert_eq!(pcm.len(), n, "exact sample-count recovery with DTX + LBRR");
}
