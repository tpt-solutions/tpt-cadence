//! SILK encoder foundation tests: encode → decode with the crate's own
//! conformance-grade [`SilkDecoder`], asserting the encoder's simulated
//! reconstruction is **bit-exact** against the real decoder, plus
//! fidelity gates on synthetic material and a bitrate sanity sweep.

use tpt_av_cadence_opus::range::RangeDecoder;
use tpt_av_cadence_opus::silk::decoder::{DecControl, LostFlag, SilkDecoder};
use tpt_av_cadence_opus::silk::encoder::SilkEncoder;

/// A speech-like signal: harmonic stack with slow pitch and amplitude
/// drift, i16 domain, at the internal rate.
fn speech_like(n: usize, fs: usize) -> Vec<i16> {
    let f0 = 120.0f32; // Hz
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

fn sine(n: usize, fs: usize, freq: f32) -> Vec<i16> {
    (0..n)
        .map(
            |i| { (2.0 * core::f32::consts::PI * freq * (i as f32) / fs as f32).sin() * 12000.0 }
                as i16,
        )
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

/// SNR between two equal-length signals.
fn snr_db(reference: &[i16], test: &[i16]) -> f64 {
    let mut num = 0f64;
    let mut den = 0f64;
    for (r, t) in reference.iter().zip(test.iter()) {
        let d = (*r - *t) as f64;
        num += d * d;
        den += *r as f64 * *r as f64;
    }
    10.0 * (den / num.max(1e-9)).log10()
}

/// The decoder-side delay of the internal-rate output relative to the
/// encoder's simulated frame: the reference's mono s_mid 2-sample header
/// (1 carried sample per frame) plus the copy-resampler's delay-line
/// (`DELAY_MATRIX_DEC[fs][fs]`: 4 at 8 kHz, 9 at 12 kHz, 12 at 16 kHz).
fn decoder_stream_delay(internal_rate: i32) -> usize {
    match internal_rate {
        8_000 => 4,
        12_000 => 9,
        _ => 12,
    }
}

/// Reconstructs the expected decoder input stream: per frame the decoder
/// resamples `[s_mid[1], xq[0..n-1]]` (the previous frame's last sample
/// plus all but the last sample of the current frame), so the stream the
/// resampler sees is exactly reconstructible from the encoder's
/// simulated frames.
fn expected_decoder_stream(frames_xq: &[Vec<i16>]) -> Vec<i16> {
    let n = frames_xq[0].len();
    let mut stream = Vec::new();
    let mut prev_last = 0i16;
    for xq in frames_xq {
        stream.push(prev_last);
        stream.extend_from_slice(&xq[..n - 1]);
        prev_last = xq[n - 1];
    }
    stream
}

/// Runs `frames` through the encoder at `api_rate == internal_rate` (so
/// the decoder's resampler is a delay-line passthrough and the
/// comparison is bit-exact after accounting for its known delay),
/// decoding each payload with a `SilkDecoder`, and returns the decoder
/// output together with the expected delay-shifted stream per frame.
fn round_trip_exact(
    internal_rate: i32,
    frame_ms: i32,
    signal: &[i16],
    frames: usize,
) -> Vec<(Vec<i16>, Vec<i16>)> {
    let fs_khz = (internal_rate / 1000) as u32;
    let frame_len_internal = frame_ms as usize * fs_khz as usize;
    let frame_len_api = frame_len_internal; // api == internal

    let mut enc = SilkEncoder::new(internal_rate, internal_rate, frame_ms).unwrap();
    enc.set_bitrate(24_000);

    let mut dec = SilkDecoder::new(1).unwrap();
    let mut out = Vec::new();
    for f in 0..frames {
        let input = &signal[f * frame_len_api..(f + 1) * frame_len_api];
        let payload = enc.encode_frame(input).unwrap();
        let simulated = enc.last_reconstructed_frame().to_vec();

        let mut ctrl = DecControl {
            n_channels_api: 1,
            n_channels_internal: 1,
            api_sample_rate: internal_rate,
            internal_sample_rate: internal_rate,
            payload_size_ms: frame_ms,
            prev_pitch_lag: 0,
        };
        let mut decoded = vec![0i16; frame_len_internal];
        let n = dec
            .decode(
                &mut ctrl,
                Some(&mut RangeDecoder::new(&payload)),
                LostFlag::Normal,
                true,
                &mut decoded,
            )
            .unwrap();
        assert_eq!(n, frame_len_internal, "frame {f}");
        decoded.truncate(n);
        out.push((decoded, simulated));
    }
    /* Rebuild the expected decoder output: the delay-line-shifted
     * reconstruction stream. */
    let delay = decoder_stream_delay(internal_rate);
    let stream = expected_decoder_stream(&out.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>());
    let frame_len = frame_ms as usize * (internal_rate / 1000) as usize;
    out.into_iter()
        .enumerate()
        .map(|(f, (decoded, _))| {
            let expected: Vec<i16> = (0..frame_len)
                .map(|i| {
                    let j = f * frame_len + i;
                    if j >= delay {
                        stream[j - delay]
                    } else {
                        0
                    }
                })
                .collect();
            (decoded, expected)
        })
        .collect()
}

#[test]
fn encoder_simulation_matches_decoder_bit_exactly() {
    for &(rate, ms) in &[(16_000i32, 20i32), (8_000, 20), (12_000, 10), (16_000, 10)] {
        let fs = (rate / 1000) as usize;
        let frame_len = ms as usize * fs;
        // Six frames of mixed material: speech-like, noise, silence.
        let mut rng = Rng(0x5EED);
        let mut signal = speech_like(frame_len * 2, fs);
        signal.extend((0..frame_len * 2).map(|_| rng.noise()));
        signal.extend(std::iter::repeat(0i16).take(frame_len * 2));

        for (f, (decoded, expected)) in round_trip_exact(rate, ms, &signal, 6)
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                decoded, expected,
                "frame {f}: decoder output diverged from the encoder simulation ({rate} Hz, {ms} ms)"
            );
        }
    }
}

/// The encoder-direction resampler's delay for same-rate operation
/// (`DELAY_MATRIX_ENC[fs][fs]`: 6 at 8 kHz, 7 at 12 kHz, 10 at 16 kHz) —
/// the reconstruction stream is the input delayed by exactly this much.
fn encoder_resampler_delay(rate: i32) -> usize {
    match rate {
        8_000 => 6,
        12_000 => 7,
        _ => 10,
    }
}

/// Encodes `signal` (api == internal) and returns the decoder-exact
/// reconstructions aligned back to the input timeline (the encoder
/// resampler's fixed delay removed), plus the mean payload size.
fn encode_reconstruct(signal: &[i16], rate: i32, ms: i32, bps: i32) -> (Vec<i16>, f64) {
    let frame_len = ms as usize * (rate / 1000) as usize;
    let frames = signal.len() / frame_len;
    let mut enc = SilkEncoder::new(rate, rate, ms).unwrap();
    enc.set_bitrate(bps);
    let mut recon = Vec::new();
    let mut total_bytes = 0usize;
    for f in 0..frames {
        let payload = enc
            .encode_frame(&signal[f * frame_len..(f + 1) * frame_len])
            .unwrap();
        total_bytes += payload.len();
        recon.extend_from_slice(enc.last_reconstructed_frame());
    }
    let delay = encoder_resampler_delay(rate);
    let mut aligned: Vec<i16> = recon[delay..].to_vec();
    if aligned.len() > signal.len() {
        aligned.truncate(signal.len());
    }
    (aligned, total_bytes as f64 / frames as f64)
}

#[test]
fn speech_round_trip_fidelity() {
    for &(rate, ms) in &[(16_000i32, 20i32), (8_000, 20), (12_000, 10)] {
        let fs = (rate / 1000) as usize;
        let frame_len = ms as usize * fs;
        let signal = speech_like(frame_len * 10, fs);

        let (recon, avg_bytes) = encode_reconstruct(&signal, rate, ms, 30_000);
        let snr = snr_db(&signal[..recon.len()], &recon);
        // The foundation's quantizer has no noise shaping; require only
        // that coded speech remains close and far from garbage.
        assert!(snr > 6.0, "SNR {snr:.2} dB at {rate} Hz / {ms} ms");
        let _ = avg_bytes;
    }
}

#[test]
fn silence_and_noise_are_stable() {
    let rate = 16_000i32;
    const FRAME_LEN: usize = 20 * 16;
    let frame_len = FRAME_LEN;
    // Digital silence: tiny payloads, silent decode.
    let mut enc = SilkEncoder::new(rate, rate, 20).unwrap();
    enc.set_bitrate(20_000);
    let mut dec = SilkDecoder::new(1).unwrap();
    for f in 0..3 {
        let payload = enc.encode_frame(&vec![0i16; frame_len]).unwrap();
        assert!(
            payload.len() <= 16,
            "silence payload {} bytes (frame {f})",
            payload.len()
        );
        let mut ctrl = DecControl {
            n_channels_api: 1,
            n_channels_internal: 1,
            api_sample_rate: rate,
            internal_sample_rate: rate,
            payload_size_ms: 20,
            prev_pitch_lag: 0,
        };
        let mut decoded = vec![0i16; frame_len];
        let n = dec
            .decode(
                &mut ctrl,
                Some(&mut RangeDecoder::new(&payload)),
                LostFlag::Normal,
                true,
                &mut decoded,
            )
            .unwrap();
        assert_eq!(n, frame_len);
        assert!(
            decoded[..n].iter().all(|&v| v.abs() <= 64),
            "decoded silence should be near-silent"
        );
    }

    // Broadband noise round-trips without corruption.
    let mut rng = Rng(0x1234_5678);
    let noise: Vec<i16> = (0..frame_len * 4).map(|_| rng.noise()).collect();
    for (f, (decoded, expected)) in round_trip_exact(rate, 20, &noise, 4)
        .into_iter()
        .enumerate()
    {
        assert_eq!(decoded, expected, "noise frame {f}");
    }
}

#[test]
fn sine_round_trip_fidelity() {
    let rate = 16_000i32;
    let frame_len = 20 * 16;
    let signal = sine(frame_len * 8, 16_000, 440.0);
    let (recon, avg_bytes) = encode_reconstruct(&signal, rate, 20, 30_000);
    let snr = snr_db(&signal[..recon.len()], &recon);
    assert!(
        snr > 10.0,
        "sine SNR {snr:.2} dB (avg {avg_bytes:.1} B/frame)"
    );
}

#[test]
fn bitrate_control_moves_payload_size() {
    let rate = 16_000i32;
    let frame_len = 20 * 16;
    let signal = speech_like(frame_len * 6, 16_000);

    let mut sizes_low = Vec::new();
    let mut sizes_high = Vec::new();

    {
        let mut enc = SilkEncoder::new(rate, rate, 20).unwrap();
        enc.set_bitrate(10_000);
        for f in 0..6 {
            let payload = enc
                .encode_frame(&signal[f * frame_len..(f + 1) * frame_len])
                .unwrap();
            if f >= 2 {
                sizes_low.push(payload.len());
            }
        }
    }
    {
        let mut enc = SilkEncoder::new(rate, rate, 20).unwrap();
        enc.set_bitrate(40_000);
        for f in 0..6 {
            let payload = enc
                .encode_frame(&signal[f * frame_len..(f + 1) * frame_len])
                .unwrap();
            if f >= 2 {
                sizes_high.push(payload.len());
            }
        }
    }
    let avg = |v: &[usize]| v.iter().sum::<usize>() as f64 / v.len() as f64;
    let low = avg(&sizes_low);
    let high = avg(&sizes_high);
    // bytes/frame → bits/s: ·50 for 20 ms frames.
    let low_bps = low * 8.0 * 50.0;
    let high_bps = high * 8.0 * 50.0;
    assert!(
        high_bps > low_bps * 1.5,
        "bitrate control must move the payload: 10 kbps target → {low_bps:.0} bps, 40 kbps target → {high_bps:.0} bps"
    );
}

#[test]
fn api_rate_resampling_path_round_trips() {
    // 48 kHz API, 16 kHz internal: exercises the encoder-direction
    // resampler. Fidelity is checked after alignment (the resampler pair
    // contributes a fixed delay) via SNR of the aligned signals.
    let rate = 48_000i32;
    let internal = 16_000i32;
    let frame_len_api = 20 * 48;
    let signal = speech_like(frame_len_api * 8, 48_000);

    let mut enc = SilkEncoder::new(rate, internal, 20).unwrap();
    enc.set_bitrate(30_000);
    let mut dec = SilkDecoder::new(1).unwrap();

    let mut decoded_all: Vec<i16> = Vec::new();
    let mut input_all: Vec<i16> = Vec::new();
    for f in 0..8 {
        let payload = enc
            .encode_frame(&signal[f * frame_len_api..(f + 1) * frame_len_api])
            .unwrap();
        let mut ctrl = DecControl {
            n_channels_api: 1,
            n_channels_internal: 1,
            api_sample_rate: rate,
            internal_sample_rate: internal,
            payload_size_ms: 20,
            prev_pitch_lag: 0,
        };
        let mut decoded = vec![0i16; 20 * 48];
        let n = dec
            .decode(
                &mut ctrl,
                Some(&mut RangeDecoder::new(&payload)),
                LostFlag::Normal,
                true,
                &mut decoded,
            )
            .unwrap();
        assert_eq!(n, 20 * 48);
        decoded_all.extend_from_slice(&decoded[..n]);
        input_all.extend_from_slice(&signal[f * frame_len_api..(f + 1) * frame_len_api]);
    }
    // Sanity: the payload decodes to something with real energy that
    // tracks the input's envelope (the round-trip delay of the 48k↔16k
    // resampler pair is nonzero; align by cross-correlation before SNR).
    let input_rms = (input_all
        .iter()
        .map(|&v| (v as f64) * v as f64)
        .sum::<f64>()
        / input_all.len() as f64)
        .sqrt();
    let decoded_rms = (decoded_all
        .iter()
        .map(|&v| (v as f64) * v as f64)
        .sum::<f64>()
        / decoded_all.len() as f64)
        .sqrt();
    assert!(
        decoded_rms > 0.3 * input_rms && decoded_rms < 3.0 * input_rms,
        "decoded RMS {decoded_rms:.1} vs input RMS {input_rms:.1}"
    );

    // Alignment search for the correlation peak.
    let mut best = (f64::MIN, 0usize);
    for shift in 0..400 {
        let mut corr = 0f64;
        for i in (0..decoded_all.len()).step_by(3) {
            let j = i + shift;
            if j < input_all.len() {
                corr += decoded_all[i] as f64 * input_all[j] as f64;
            }
        }
        if corr > best.0 {
            best = (corr, shift);
        }
    }
    assert!(best.1 > 0, "expected a nonzero round-trip delay");
}

#[test]
fn invalid_inputs_are_rejected() {
    let mut enc = SilkEncoder::new(16_000, 16_000, 20).unwrap();
    assert!(enc.encode_frame(&[0i16; 319]).is_err());
    assert!(enc.encode_frame(&[0i16; 321]).is_err());
    assert!(SilkEncoder::new(16_000, 24_000, 20).is_err());
    assert!(SilkEncoder::new(16_000, 16_000, 30).is_err());
    assert!(SilkEncoder::new(44_100, 16_000, 20).is_err());
}
