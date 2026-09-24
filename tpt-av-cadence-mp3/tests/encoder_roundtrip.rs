//! Round-trip tests for the MP3 encoder: encode synthetic signals and
//! decode them back through this crate's own decoder.
//!
//! The tests that pass unconditionally check what is actually solid:
//! structural bitstream validity (sync/frame geometry, CBR byte budget
//! respected) and clean decodability (no decode errors, finite non-empty
//! output) across sample rates, bitrates, mono/stereo, and edge cases
//! (silence, a sub-one-frame stream).
//!
//! `mono_sine_tone_decodes_with_concentrated_energy` and
//! `stereo_white_noise_round_trips_recognizably` are active end-to-end
//! fidelity gates. The analysis, MDCT, Huffman, side-info, and synthesis
//! stages are covered, and both targets now pass with the current reduced
//! encoder scope.

use std::io::Cursor;

use tpt_av_cadence_core::{Decoder, Encoder};
use tpt_av_cadence_mp3::{Mp3Decoder, Mp3Encoder};

fn encode(sample_rate: u32, channels: u16, bitrate_kbps: u32, frames: &[f32]) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new(&mut buf, sample_rate, channels, bitrate_kbps).unwrap();
        enc.encode(frames).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    buf.into_inner()
}

fn decode_all(data: Vec<u8>) -> (Vec<f32>, tpt_av_cadence_core::StreamInfo) {
    let mut dec = Mp3Decoder::open(Box::new(Cursor::new(data))).unwrap();
    let info = dec.info().clone();
    let channels = info.channels as usize;
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 4096 * channels];
    loop {
        let got = dec.decode(&mut buf).unwrap();
        if got == 0 {
            break;
        }
        out.extend_from_slice(&buf[..got * channels]);
    }
    (out, info)
}

fn sine_tone(sample_rate: u32, freq: f32, seconds: f32, amp: f32) -> Vec<f32> {
    let n = (sample_rate as f32 * seconds) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            (2.0 * std::f32::consts::PI * freq * t).sin() * amp
        })
        .collect()
}

fn white_noise(n: usize, amp: f32, seed: u32) -> Vec<f32> {
    let mut state = seed;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        (state as f32 / u32::MAX as f32) * 2.0 - 1.0
    };
    (0..n).map(|_| next() * amp).collect()
}

/// Goertzel-style single-bin DFT magnitude, used to check that a decoded
/// sine tone's energy is concentrated at its source frequency rather than
/// smeared across the spectrum or absent entirely.
fn dft_magnitude(signal: &[f32], sample_rate: u32, freq: f32) -> f64 {
    let n = signal.len();
    let w = 2.0 * std::f64::consts::PI * freq as f64 / sample_rate as f64;
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, &s) in signal.iter().enumerate() {
        let phase = w * i as f64;
        re += s as f64 * phase.cos();
        im -= s as f64 * phase.sin();
    }
    let _ = n;
    (re * re + im * im).sqrt()
}

fn pearson_correlation(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len()) as f64;
    let (mut sa, mut sb) = (0.0f64, 0.0f64);
    for i in 0..n as usize {
        sa += a[i] as f64;
        sb += b[i] as f64;
    }
    let (ma, mb) = (sa / n, sb / n);
    let (mut num, mut da, mut db) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..n as usize {
        let xa = a[i] as f64 - ma;
        let xb = b[i] as f64 - mb;
        num += xa * xb;
        da += xa * xa;
        db += xb * xb;
    }
    if da <= 0.0 || db <= 0.0 {
        return 0.0;
    }
    num / (da.sqrt() * db.sqrt())
}

fn best_delayed_correlation(decoded: &[f32], source: &[f32], max_delay: usize) -> (f64, usize) {
    let mut best = (0.0f64, 0usize);
    for delay in 0..=max_delay {
        let n = decoded.len().min(source.len()).saturating_sub(delay);
        if n < 1024 {
            break;
        }
        let corr = pearson_correlation(&decoded[delay..delay + n], &source[..n]);
        if corr.abs() > best.0.abs() {
            best = (corr, delay);
        }
    }
    best
}

#[test]
fn mono_sine_tone_produces_a_valid_decodable_stream() {
    let sample_rate = 44_100u32;
    let freq = 1000.0f32;
    let frames = sine_tone(sample_rate, freq, 1.0, 0.6);
    let data = encode(sample_rate, 1, 128, &frames);

    // Structural validity: a real Layer III sync word, and a plausible
    // frame count for CBR 128kbps/44100Hz over ~1 second.
    assert_eq!(&data[..2], &[0xFF, 0xFB], "no valid MPEG-1 Layer III sync");
    let expected_frames = frames.len() / 1152 + 1; // finish() pads a final partial frame
    let frame_bytes = 1152.0 * 128.0 * 125.0 / sample_rate as f64;
    assert!(
        (data.len() as f64 - expected_frames as f64 * frame_bytes).abs() < 2.0 * frame_bytes,
        "encoded size {} doesn't match ~{expected_frames} CBR frames of ~{frame_bytes} bytes",
        data.len()
    );

    let (decoded, info) = decode_all(data);
    assert_eq!(info.channels, 1);
    assert_eq!(info.sample_rate, sample_rate);
    assert!(!decoded.is_empty(), "decoder produced no samples");
    assert!(
        decoded.iter().all(|v| v.is_finite()),
        "decoded output contains NaN/Inf"
    );
}

/// End-to-end mono fidelity gate: the decoded tone must remain concentrated at
/// the source frequency and correlate after the bounded MP3 delay.
#[test]
fn mono_sine_tone_decodes_with_concentrated_energy() {
    let sample_rate = 44_100u32;
    let freq = 1000.0f32;
    let frames = sine_tone(sample_rate, freq, 1.0, 0.6);
    let data = encode(sample_rate, 1, 128, &frames);
    let (decoded, _info) = decode_all(data);

    // Energy at the source frequency must dominate energy at a
    // well-separated frequency (recognizably a tone, not noise).
    let at_freq = dft_magnitude(&decoded, sample_rate, freq);
    let off_freq = dft_magnitude(&decoded, sample_rate, freq * 2.3);
    assert!(
        at_freq > off_freq * 5.0,
        "sine tone energy not concentrated at {freq} Hz: at_freq={at_freq} off_freq={off_freq}"
    );

    let (corr, delay) = best_delayed_correlation(&decoded, &frames, 4096);
    assert!(
        corr > 0.5,
        "decoded tone doesn't correlate with source: corr={corr} delay={delay}"
    );
}

fn stereo_white_noise_frames(sample_rate: u32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let seconds = 0.5f32;
    let n = (sample_rate as f32 * seconds) as usize;
    let left = white_noise(n, 0.5, 0xC0FF_EE01);
    let right = white_noise(n, 0.5, 0x1234_5678);
    let mut frames = vec![0.0f32; n * 2];
    for i in 0..n {
        frames[i * 2] = left[i];
        frames[i * 2 + 1] = right[i];
    }
    (frames, left, right)
}

#[test]
fn stereo_white_noise_produces_a_valid_decodable_stream() {
    let sample_rate = 44_100u32;
    let (frames, _left, _right) = stereo_white_noise_frames(sample_rate);

    let data = encode(sample_rate, 2, 128, &frames);
    assert_eq!(&data[..2], &[0xFF, 0xFB]);

    let (decoded, info) = decode_all(data);
    assert_eq!(info.channels, 2);
    assert!(!decoded.is_empty());
    assert!(
        decoded.iter().all(|v| v.is_finite()),
        "decoded output contains NaN/Inf"
    );
}

/// End-to-end stereo fidelity gate: independently generated channels must
/// remain recognizable after delay alignment without collapsing together.
#[test]
fn stereo_white_noise_round_trips_recognizably() {
    let sample_rate = 44_100u32;
    let (frames, left, right) = stereo_white_noise_frames(sample_rate);
    let data = encode(sample_rate, 2, 128, &frames);
    let (decoded, _info) = decode_all(data);

    let decoded_left: Vec<f32> = decoded.iter().step_by(2).copied().collect();
    let decoded_right: Vec<f32> = decoded.iter().skip(1).step_by(2).copied().collect();
    let (corr_l, delay_l) = best_delayed_correlation(&decoded_left, &left, 4096);
    let (corr_r, delay_r) = best_delayed_correlation(&decoded_right, &right, 4096);
    assert!(
        corr_l > 0.3,
        "decoded left channel doesn't correlate with source noise: corr={corr_l} delay={delay_l}"
    );
    assert!(
        corr_r > 0.3,
        "decoded right channel doesn't correlate with source noise: corr={corr_r} delay={delay_r}"
    );
    // The two channels were independently generated noise: cross-channel
    // correlation should stay low, confirming independent (not
    // accidentally-summed/collapsed) stereo channels survive encoding.
    let cross = pearson_correlation(&decoded_left, &decoded_right);
    assert!(
        cross.abs() < 0.3,
        "decoded channels are suspiciously correlated with each other: cross={cross}"
    );
}

#[test]
fn silence_round_trips_to_silence() {
    let sample_rate = 44_100u32;
    let frames = vec![0.0f32; sample_rate as usize];
    let data = encode(sample_rate, 1, 128, &frames);
    let (decoded, _info) = decode_all(data);
    assert!(!decoded.is_empty());
    let max_abs = decoded.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    assert!(
        max_abs < 1e-3,
        "silence did not decode to near-silence: max_abs={max_abs}"
    );
}

#[test]
fn short_stream_finish_produces_one_padded_frame() {
    let sample_rate = 44_100u32;
    // Well under one 1152-sample frame.
    let frames = sine_tone(sample_rate, 440.0, 0.01, 0.5);
    let data = encode(sample_rate, 1, 128, &frames);
    assert_eq!(&data[..2], &[0xFF, 0xFB]);
    let (decoded, _info) = decode_all(data);
    assert!(decoded.len() >= frames.len());
}

#[test]
fn multiple_bitrates_and_sample_rates_produce_valid_streams() {
    for &sample_rate in &[32_000u32, 44_100, 48_000] {
        for &bitrate in &[64u32, 128, 192] {
            let frames = sine_tone(sample_rate, 500.0, 0.2, 0.4);
            let data = encode(sample_rate, 1, bitrate, &frames);
            let (decoded, info) = decode_all(data);
            assert_eq!(info.sample_rate, sample_rate);
            assert!(
                !decoded.is_empty(),
                "sr={sample_rate} br={bitrate}: decoder produced no samples"
            );
        }
    }
}
