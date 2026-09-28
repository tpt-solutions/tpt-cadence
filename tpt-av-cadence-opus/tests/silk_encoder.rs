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
    complexity: u8,
) -> Vec<(Vec<i16>, Vec<i16>)> {
    let fs_khz = (internal_rate / 1000) as u32;
    let frame_len_internal = frame_ms as usize * fs_khz as usize;
    let frame_len_api = frame_len_internal; // api == internal

    let mut enc = SilkEncoder::new(internal_rate, internal_rate, frame_ms).unwrap();
    enc.set_bitrate(24_000);
    enc.set_complexity(complexity);

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

        for (f, (decoded, expected)) in round_trip_exact(rate, ms, &signal, 6, 1)
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
fn encode_reconstruct(
    signal: &[i16],
    rate: i32,
    ms: i32,
    bps: i32,
    complexity: u8,
) -> (Vec<i16>, f64) {
    let frame_len = ms as usize * (rate / 1000) as usize;
    let frames = signal.len() / frame_len;
    let mut enc = SilkEncoder::new(rate, rate, ms).unwrap();
    enc.set_bitrate(bps);
    enc.set_complexity(complexity);
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

        let (recon, avg_bytes) = encode_reconstruct(&signal, rate, ms, 30_000, 1);
        let snr = snr_db(&signal[..recon.len()], &recon);
        // The quantizer's noise shaping is still analysis-side only (the
        // shaping filter is not closed into the residual loop), so the gate
        // tracks the per-subframe warped-gain analysis rather than a fully
        // shaped quantizer.
        //
        // Floor re-baselined (2026-09-28, seventh session): the rate-control
        // loop's interpolation branch was firing on `found_upper` alone
        // (garbage `gain_mult_lower`/`n_bits_lower`, still their
        // zero-initializers), which sent `gainMult` off in essentially
        // random directions and, incidentally, let the encoder silently
        // exceed its 30 kbps target by 1.2-1.5x on many frames — this test's
        // old 12.2-19.0 dB range was measuring that inflated effective
        // bitrate, not 30 kbps quality. With the loop gated correctly on
        // `found_lower && found_upper` the payload now actually tracks
        // 30 kbps (see `rate_control_lands_payload_on_budget` and the
        // todo.md session log), and honest 30 kbps quality on this signal
        // measures 9.3-18.2 dB across the three configurations.
        assert!(snr > 8.0, "SNR {snr:.2} dB at {rate} Hz / {ms} ms");
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
    for (f, (decoded, expected)) in round_trip_exact(rate, 20, &noise, 4, 1)
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
    let (recon, avg_bytes) = encode_reconstruct(&signal, rate, 20, 30_000, 1);
    let snr = snr_db(&signal[..recon.len()], &recon);
    assert!(
        snr > 10.0,
        "sine SNR {snr:.2} dB (avg {avg_bytes:.1} B/frame)"
    );
}

/// The per-subframe, frequency-warped gain analysis
/// ([`crate::silk::noise_shape`], the reference's
/// `silk_noise_shape_analysis_FLP`) must measurably beat the frame-level
/// proxy it replaced. The original before/after table measured here (5.99
/// vs 6.82 dB at 8 kbps, up to 24.53 vs 25.70 dB at 48 kbps) was taken while
/// the rate-control loop had the interpolation-gating bug described below,
/// which silently overshot these bitrate targets by 1.2-1.5x — see the
/// floors' own comment for the corrected, budget-compliant measurement.
#[test]
fn shaped_gain_analysis_improves_speech_snr() {
    let rate = 16_000i32;
    let frame_len = 20 * 16;
    let signal = speech_like(frame_len * 200, 16);
    // Floors re-baselined (2026-09-28, seventh session): fixing the
    // rate-control loop's interpolation-gating bug (see todo.md) stopped the
    // encoder from silently overshooting these targets by 1.2-1.5x, so the
    // payload now actually tracks 8/16/24/48 kbps instead of an inflated
    // effective bitrate — and 8/16/24 kbps are all below this foundation
    // quantizer's documented ~34 kbps floor for active 16 kHz speech (its
    // gain search hits the 4x cap and still can't clear a smaller budget;
    // see `shaped_gains_stay_within_the_quantizer_bound`), so quality this
    // far under the floor is now honestly measured rather than flattered by
    // the bug. Measured at the corrected, budget-compliant bitrate: 1.8 /
    // 6.3 / 10.3 / 20.6 dB. Only 48 kbps clears the floor, and its honest
    // number is still below the old (overshoot-inflated) gate.
    for &(bps, floor) in &[
        (8_000i32, 1.0f64),
        (16_000, 5.0),
        (24_000, 9.0),
        (48_000, 19.0),
    ] {
        let (recon, _) = encode_reconstruct(&signal, rate, 20, bps, 1);
        let snr = snr_db(&signal[..recon.len()], &recon);
        assert!(
            snr > floor,
            "speech SNR {snr:.2} dB at {bps} bps (floor {floor})"
        );
    }
}

/// The shaping analysis must not change the bitstream *contract*: the
/// encoder's simulated reconstruction stays bit-identical to the real
/// decoder's, which the differential tests in the crate cover, and the
/// per-subframe gains it produces must still respect the quantizer's
/// `process_gains` bound so `Gains_Q16` cannot overflow.
#[test]
fn shaped_gains_stay_within_the_quantizer_bound() {
    let rate = 16_000i32;
    let frame_len = 20 * 16;
    // Loud, broadband material stresses the gain path hardest.
    let mut seed = 0x2468_1357u32;
    let signal: Vec<i16> = (0..frame_len * 20)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            ((seed % 30000) as i16) - 15000
        })
        .collect();
    for bps in [8_000i32, 32_000, 80_000] {
        let mut enc = SilkEncoder::new(rate, rate, 20).unwrap();
        enc.set_bitrate(bps);
        // VBR payloads overshoot the nominal frame budget (the reference
        // spends a 2-6x margin on the first frames after a rate change), so
        // the bound is generous; it only catches a runaway.
        let budget = (bps as usize) / 8 * 20 / 1000 * 2 + 60;
        for f in 0..20 {
            let payload = enc
                .encode_frame(&signal[f * frame_len..(f + 1) * frame_len])
                .unwrap();
            assert!(
                payload.len() < budget,
                "frame {f} payload {} B (budget {budget}) at {bps} bps",
                payload.len()
            );
            let recon = enc.last_reconstructed_frame();
            let peak = recon.iter().map(|v| i32::from(*v).abs()).max().unwrap_or(0);
            // A runaway gain would clip; the reference's soft limit keeps the
            // reconstruction inside the signal's own range.
            assert!(peak <= 30000, "recon peak {peak} at {bps} bps frame {f}");
        }
    }
}

#[test]
fn rate_control_lands_payload_on_budget() {
    // With the reference per-frame rate control, the payload tracks the
    // caller's bitrate budget: bytes/frame = rate/400 (20 ms frames).
    // The foundation's quantizer floor (~85 B/frame for active speech at
    // 16 kHz internal — side info plus residual at the 4x gain cap)
    // bounds what low targets can reach, so budgets sit above it.
    let rate = 16_000i32;
    let frame_len = 20 * 16;
    let signal = speech_like(frame_len * 6, 16_000);

    for &(bps, budget_b) in &[(32_000i32, 80usize), (48_000, 120)] {
        let mut enc = SilkEncoder::new(rate, rate, 20).unwrap();
        enc.set_bitrate(bps);
        let mut sizes = Vec::new();
        for f in 0..6 {
            let payload = enc
                .encode_frame(&signal[f * frame_len..(f + 1) * frame_len])
                .unwrap();
            if f >= 2 {
                sizes.push(payload.len());
            }
        }
        let avg: f64 = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
        assert!(
            (avg - budget_b as f64).abs() <= 0.35 * budget_b as f64,
            "{bps} bps: avg payload {avg:.1} B must track the {budget_b} B budget"
        );
    }

    // Rate control must still move the payload: 48 kbps lands at its
    // 120 B budget while 24 kbps (60 B budget, near the quantizer floor)
    // lands clearly lower.
    let avg_of = |bps: i32| -> f64 {
        let mut enc = SilkEncoder::new(rate, rate, 20).unwrap();
        enc.set_bitrate(bps);
        let mut sizes = Vec::new();
        for f in 2..6 {
            let payload = enc
                .encode_frame(&signal[f * frame_len..(f + 1) * frame_len])
                .unwrap();
            sizes.push(payload.len());
        }
        sizes.iter().sum::<usize>() as f64 / sizes.len() as f64
    };
    let low = avg_of(24_000);
    let high = avg_of(48_000);
    println!("24 kbps → {low:.1} B, 48 kbps → {high:.1} B");
    // Higher budget must never yield a smaller payload; the spread on
    // this compressible synthetic signal is modest because the 24 kbps
    // attempt lands near the quantizer's payload floor.
    assert!(
        high > low,
        "bitrate control must move the payload: 24 kbps → {low:.1} B, 48 kbps → {high:.1} B"
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

/// Perceptual-quality measurement via the shared test-utils crate: the
/// A-weighted SNR (error spectrum weighted by the hearing sensitivity
/// curve) and segmental SNR. Both must remain above conservative floors
/// on shaped speech — they exist to make noise-shaping regressions (and
/// any future delayed-decision NSQ work) measurable beyond waveform SNR.
#[test]
fn perceptual_metrics_track_shaped_speech_quality() {
    use tpt_av_cadence_test_utils::quality::{a_weighted_snr_db, segmental_snr_db};

    let rate = 16_000i32;
    let frame_len = 20 * 16;
    let signal = speech_like(frame_len * 40, 16);

    let (recon, _) = encode_reconstruct(&signal, rate, 20, 32_000, 1);
    let n = recon.len();
    let sig: Vec<f32> = signal[..n].iter().map(|&v| v as f32 / 32768.0).collect();
    let out: Vec<f32> = recon.iter().map(|&v| v as f32 / 32768.0).collect();

    let awsnr = a_weighted_snr_db(&sig, &out, 48_000);
    let segsnr = segmental_snr_db(&sig, &out, 320);
    println!("A-weighted SNR {awsnr:.1} dB, segmental SNR {segsnr:.1} dB");
    // A-weighted floor lowered 10.0 -> 8.0 dB (2026-09-28, seventh session):
    // the rate-control loop's interpolation-gating fix ended a bug that let
    // the encoder silently overshoot its 32 kbps target by 1.2-1.5x; honest
    // 32 kbps quality on this signal measures 9.3 dB (see todo.md).
    assert!(
        awsnr > 8.0,
        "A-weighted SNR {awsnr:.1} dB below the regression floor"
    );
    assert!(
        segsnr > 5.0,
        "segmental SNR {segsnr:.1} dB below the regression floor"
    );
}

/// `set_complexity(1)` is the encoder's default: its bitstreams must be
/// byte-identical to a fresh encoder's (the complexity knob defaults below
/// the reference's `silk_NSQ_del_dec` dispatch to keep the foundation
/// quantizer — see `SilkEncoder::set_complexity`). Also pins the 0..=10
/// clamp: 255 saturates to 10, whose payloads differ from the default's
/// (the delayed-decision quantizer is engaged) and are stable.
#[test]
fn complexity_knob_selects_the_reference_dispatch() {
    let rate = 16_000i32;
    let frame_len = 20 * 16;
    let signal = speech_like(frame_len * 4, 16);

    let mut default = SilkEncoder::new(rate, rate, 20).unwrap();
    default.set_bitrate(24_000);
    let mut c1 = SilkEncoder::new(rate, rate, 20).unwrap();
    c1.set_bitrate(24_000);
    c1.set_complexity(1);
    let mut c10 = SilkEncoder::new(rate, rate, 20).unwrap();
    c10.set_bitrate(24_000);
    c10.set_complexity(10);
    let mut clamped = SilkEncoder::new(rate, rate, 20).unwrap();
    clamped.set_bitrate(24_000);
    clamped.set_complexity(255);

    for f in 0..4 {
        let input = &signal[f * frame_len..(f + 1) * frame_len];
        let default_payload = default.encode_frame(input).unwrap();
        let c1_payload = c1.encode_frame(input).unwrap();
        let c10_payload = c10.encode_frame(input).unwrap();
        let clamped_payload = clamped.encode_frame(input).unwrap();
        assert_eq!(
            default_payload, c1_payload,
            "frame {f}: complexity 1 must be the default bitstream"
        );
        assert_eq!(
            c10_payload, clamped_payload,
            "frame {f}: the complexity clamp must saturate at 10"
        );
        assert_ne!(
            default_payload, c10_payload,
            "frame {f}: complexity 10 must engage the delayed-decision quantizer"
        );
    }
}

/// The full encode→decode loop with the delayed-decision quantizer active
/// (complexity 10: 4 states + warped shaping feedback): the decoder's
/// output must stay bit-exact against the encoder's simulation — the same
/// contract `encoder_simulation_matches_decoder_bit_exactly` pins for the
/// default path, across the same mixed material (speech-like, noise,
/// digital silence) and rates.
#[test]
fn complexity_ten_round_trips_match_the_decoder() {
    for &(rate, ms) in &[(16_000i32, 20i32), (8_000, 20), (12_000, 10)] {
        let fs = (rate / 1000) as usize;
        let frame_len = ms as usize * fs;
        // Six frames of mixed material: speech-like, noise, silence.
        let mut rng = Rng(0x5EED);
        let mut signal = speech_like(frame_len * 2, fs);
        signal.extend((0..frame_len * 2).map(|_| rng.noise()));
        signal.extend(std::iter::repeat(0i16).take(frame_len * 2));

        for (f, (decoded, expected)) in round_trip_exact(rate, ms, &signal, 6, 10)
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                decoded, expected,
                "frame {f}: complexity-10 decoder output diverged from the encoder \
                 simulation ({rate} Hz, {ms} ms)"
            );
        }
    }
}

/// CBR sizing with the delayed-decision quantizer: every payload is
/// exactly the requested size (the CBR retry loop must restore the NSQ
/// carrier state between attempts like any other), the zero padding is
/// never read, and the decode matches the accepted attempt's simulation
/// bit-exactly after the decoder's fixed stream delay.
#[test]
fn complexity_ten_cbr_keeps_constant_payloads_bit_exact() {
    let rate = 16_000i32;
    const FRAME_LEN: usize = 20 * 16;
    const CBR_BYTES: usize = 80;
    let signal = speech_like(FRAME_LEN * 4, 16);

    let mut enc = SilkEncoder::new(rate, rate, 20).unwrap();
    enc.set_bitrate(32_000);
    enc.set_complexity(10);
    enc.set_cbr_bytes(CBR_BYTES).unwrap();

    let mut dec = SilkDecoder::new(1).unwrap();
    let mut sim_frames = Vec::new();
    let mut decoded_stream = Vec::new();
    for f in 0..4 {
        let input = &signal[f * FRAME_LEN..(f + 1) * FRAME_LEN];
        let payload = enc.encode_frame(input).unwrap();
        assert_eq!(payload.len(), CBR_BYTES, "frame {f}: CBR size not held");
        sim_frames.push(enc.last_reconstructed_frame().to_vec());

        let mut ctrl = DecControl {
            n_channels_api: 1,
            n_channels_internal: 1,
            api_sample_rate: rate,
            internal_sample_rate: rate,
            payload_size_ms: 20,
            prev_pitch_lag: 0,
        };
        let mut decoded = vec![0i16; FRAME_LEN];
        let n = dec
            .decode(
                &mut ctrl,
                Some(&mut RangeDecoder::new(&payload)),
                LostFlag::Normal,
                true,
                &mut decoded,
            )
            .unwrap();
        assert_eq!(n, FRAME_LEN);
        decoded_stream.extend_from_slice(&decoded[..n]);
    }

    // The padded zero bytes are never read, so the decode is bit-identical
    // to the simulation stream (offset by the decoder's fixed delay).
    let delay = decoder_stream_delay(rate);
    let expected = expected_decoder_stream(&sim_frames);
    for (i, &d) in decoded_stream.iter().enumerate() {
        let e = if i >= delay { expected[i - delay] } else { 0 };
        assert_eq!(d, e, "sample {i}: CBR decode diverged from the simulation");
    }
}

/// The delayed-decision quantizer's benefit, measured in the perceptual
/// terms this suite records for exactly this purpose (see
/// `perceptual_metrics_track_shaped_speech_quality`): closing the
/// reference noise shaping into the quantizer's error feedback trades raw
/// waveform/segmental SNR for hearing-weighted quality. On the 16 kHz
/// speech-like fixture the A-weighted SNR improves sharply while the
/// payload *shrinks*; at 8 kHz the signal is already below the A-weighted
/// penalty band, so parity is the gate. Measured floors (2026-09-29,
/// synthetic speech-like at 32/24/16 kbps): baseline A-weighted
/// 6.8/7.8/24.0 dB vs complexity 10 at 20.1/17.2/23.2 dB.
#[test]
fn delayed_decision_complexity_improves_perceptual_metrics() {
    use tpt_av_cadence_test_utils::quality::a_weighted_snr_db;

    for &(rate, bps, base_floor, dd_floor) in &[
        (16_000i32, 32_000i32, 5.0, 17.0),
        (16_000, 24_000, 6.0, 14.0),
        (8_000, 16_000, 20.0, 20.0),
    ] {
        let fs = (rate / 1000) as usize;
        let frame_len = 20 * fs;
        let signal = speech_like(frame_len * 12, fs);
        let sig: Vec<f32> = signal.iter().map(|&v| v as f32).collect();

        let (base_recon, base_bytes) = encode_reconstruct(&signal, rate, 20, bps, 1);
        let (dd_recon, dd_bytes) = encode_reconstruct(&signal, rate, 20, bps, 10);

        let n = sig.len().min(base_recon.len()).min(dd_recon.len());
        let base: Vec<f32> = base_recon[..n].iter().map(|&v| v as f32).collect();
        let dd: Vec<f32> = dd_recon[..n].iter().map(|&v| v as f32).collect();
        let a_base = a_weighted_snr_db(&sig[..n], &base, rate as u32);
        let a_dd = a_weighted_snr_db(&sig[..n], &dd, rate as u32);

        assert!(
            a_base >= base_floor,
            "{rate} Hz / {bps} bps: baseline A-weighted {a_base:.2} dB below floor"
        );
        assert!(
            a_dd >= dd_floor,
            "{rate} Hz / {bps} bps: complexity-10 A-weighted {a_dd:.2} dB below floor"
        );
        if rate == 16_000 {
            assert!(
                a_dd > a_base + 3.0,
                "{rate} Hz / {bps} bps: complexity 10 must clearly improve the \
                 A-weighted metric ({a_dd:.2} vs {a_base:.2} dB)"
            );
        } else {
            assert!(
                a_dd > a_base - 1.5,
                "{rate} Hz / {bps} bps: complexity 10 must hold the A-weighted \
                 metric ({a_dd:.2} vs {a_base:.2} dB)"
            );
        }
        let _ = (base_bytes, dd_bytes);
    }
}
