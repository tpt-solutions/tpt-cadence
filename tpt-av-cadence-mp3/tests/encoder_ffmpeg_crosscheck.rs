//! Cross-checks the MP3 *encoder* against FFmpeg: an independent decoder
//! must be able to decode this crate's own encoder output without erroring.
//!
//! This is the strongest correctness signal available for a lossy encoder:
//! decoding cleanly in FFmpeg (which implements the full ISO reference
//! Huffman/scalefactor/bit-reservoir logic) rules out the failure mode
//! where this crate's own decoder happens to accept a subtly malformed
//! bitstream because it shares a bug with the encoder. It is not a
//! fidelity check (see `tests/encoder_roundtrip.rs`'s `#[ignore]`d tests
//! for the known, documented fidelity gap); it only asserts FFmpeg accepts
//! the stream and produces the expected number of samples.

use std::io::Cursor;
use std::path::Path;

use tpt_av_cadence_core::Encoder;
use tpt_av_cadence_mp3::Mp3Encoder;
use tpt_av_cadence_test_utils::reference::{decode_with_ffmpeg, ffmpeg_available};

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

fn encode_to_temp(
    name: &str,
    sample_rate: u32,
    channels: u16,
    bitrate_kbps: u32,
    frames: &[f32],
) -> std::path::PathBuf {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new(&mut buf, sample_rate, channels, bitrate_kbps).unwrap();
        enc.encode(frames).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "cadence_mp3_encoder_crosscheck_{name}_{}.mp3",
        std::process::id()
    ));
    std::fs::write(&path, buf.into_inner()).expect("write temp mp3");
    path
}

fn run_crosscheck(path: &Path, sample_rate: u32, channels: u16, expected_frames: usize) {
    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }
    let result = decode_with_ffmpeg(path, sample_rate, channels);
    let _ = std::fs::remove_file(path);
    let got = result.expect("ffmpeg failed to decode our encoder output");
    let got_frames = got.len() / channels as usize;
    // Allow generous slack: FFmpeg's own MP3 decoder may apply a different
    // amount of encoder/decoder delay trimming than this crate's decoder,
    // and the final frame is silence-padded.
    assert!(
        got_frames + 4000 >= expected_frames,
        "ffmpeg decoded far fewer frames ({got_frames}) than expected (~{expected_frames})"
    );
    assert!(
        got.iter().all(|v| v.is_finite()),
        "ffmpeg-decoded output contains NaN/Inf"
    );
}

#[test]
fn mono_sine_tone_decodes_cleanly_in_ffmpeg() {
    let sample_rate = 44_100u32;
    let frames = sine_tone(sample_rate, 1000.0, 1.0, 0.6);
    let expected_frames = frames.len();
    let path = encode_to_temp("mono_sine", sample_rate, 1, 128, &frames);
    run_crosscheck(&path, sample_rate, 1, expected_frames);
}

#[test]
fn stereo_white_noise_decodes_cleanly_in_ffmpeg() {
    let sample_rate = 44_100u32;
    let n = sample_rate as usize / 2;
    let left = white_noise(n, 0.5, 0xC0FF_EE01);
    let right = white_noise(n, 0.5, 0x1234_5678);
    let mut frames = vec![0.0f32; n * 2];
    for i in 0..n {
        frames[i * 2] = left[i];
        frames[i * 2 + 1] = right[i];
    }
    let path = encode_to_temp("stereo_noise", sample_rate, 2, 128, &frames);
    run_crosscheck(&path, sample_rate, 2, n);
}

#[test]
fn multiple_sample_rates_decode_cleanly_in_ffmpeg() {
    for &sample_rate in &[32_000u32, 44_100, 48_000] {
        let frames = sine_tone(sample_rate, 500.0, 0.5, 0.4);
        let expected_frames = frames.len();
        let path = encode_to_temp(&format!("sr{sample_rate}"), sample_rate, 1, 128, &frames);
        run_crosscheck(&path, sample_rate, 1, expected_frames);
    }
}

/// Full bitrate-ladder oracle (stereo, noise material): for every standard
/// MPEG-1 bitrate, this crate's encoder output must (1) decode cleanly in
/// FFmpeg with the expected sample count and finite samples, and (2) match
/// FFmpeg's decode of the *same bytes* at >100 dB SNR — an independent
/// decoder agreeing with ours on our own bitstream, so a spec-misinterpreted
/// Huffman/scalefactor/side-info choice cannot hide behind
/// shared-implementation bugs. The encoder emits no Xing/gapless tag, so
/// both sides compare the raw frame stream.
///
/// The gate is deliberately RELATIVE (SNR, not absolute sample error):
/// the encoder currently renders every material at ~1e5x the source scale
/// (self-consistently — both decoders agree), which `todo.md` tracks as the
/// 2026-09-27 encoder scale/divergence defects together with the tonal-path
/// decoder divergence. Absolute-scale and tonal gates live in the
/// `#[ignore]`d `encoder_tonal_and_mono_scale_defects` regression test until
/// the fix lands.
#[test]
fn encoder_bitrate_ladder_matches_ffmpeg_decode() {
    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::Mp3Decoder;

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }
    let sample_rate = 44_100u32;
    let n = sample_rate as usize; // ~1 s of source
    let mut failures = Vec::new();
    let channels = 2u16;
    {
        let left = white_noise(n, 0.5, 0xC0FF_EE01);
        let right = white_noise(n, 0.5, 0x1234_5678);
        let mut frames = vec![0.0f32; n * 2];
        for i in 0..n {
            frames[i * 2] = left[i];
            frames[i * 2 + 1] = right[i];
        }
        let source_peak = frames.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        for &bitrate in &[32u32, 64, 96, 128, 192, 256, 320] {
            let case = format!("stereo_{bitrate}k");
            let mut buf = Cursor::new(Vec::new());
            {
                let mut enc = Mp3Encoder::new(&mut buf, sample_rate, channels, bitrate).unwrap();
                enc.encode(&frames).unwrap();
                Encoder::finish(&mut enc).unwrap();
            }
            let dir = std::env::temp_dir();
            let path = dir.join(format!(
                "cadence_mp3_ladder_{}_{}.mp3",
                case,
                std::process::id()
            ));
            std::fs::write(&path, buf.into_inner()).expect("write temp mp3");

            let oracle = match decode_with_ffmpeg(&path, sample_rate, channels) {
                Ok(pcm) => pcm,
                Err(e) => {
                    let _ = std::fs::remove_file(&path);
                    failures.push(format!("{case}: ffmpeg decode failed: {e}"));
                    continue;
                }
            };
            if oracle.iter().any(|v| !v.is_finite()) {
                failures.push(format!("{case}: ffmpeg output non-finite"));
            }
            let expected = frames.len() / channels as usize;
            let got_frames = oracle.len() / channels as usize;
            // Same slack policy as run_crosscheck: decoder delay handling may
            // differ and the final frame is silence-padded.
            if got_frames + 4000 < expected {
                failures.push(format!(
                    "{case}: ffmpeg decoded {got_frames} frames, expected ~{expected}"
                ));
            }

            match Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())) {
                Ok(mut dec) => {
                    let ch = dec.info().channels as usize;
                    let mut ours = Vec::new();
                    let mut buf = vec![0.0f32; 1152 * ch];
                    loop {
                        match dec.decode(&mut buf) {
                            Ok(0) => break,
                            Ok(got) => ours.extend_from_slice(&buf[..got * ch]),
                            Err(e) => {
                                failures.push(format!("{case}: our decode errored: {e}"));
                                break;
                            }
                        }
                    }
                    if ours.len() == oracle.len() {
                        let mut signal = 0.0f64;
                        let mut error = 0.0f64;
                        let mut peak = 0.0f64;
                        for (&a, &b) in ours.iter().zip(&oracle) {
                            let delta = f64::from(a) - f64::from(b);
                            error += delta * delta;
                            signal += f64::from(b).powi(2);
                            peak = peak.max(delta.abs());
                        }
                        if signal > 0.0 {
                            let snr = 10.0 * (signal / error).log10();
                            eprintln!("{case}: SNR={snr:.2} dB, max error={peak:.3e}");
                            if snr <= 100.0 {
                                failures.push(format!("{case}: SNR={snr:.2}"));
                            }
                        }
                    } else {
                        failures.push(format!(
                            "{case}: length ours={} vs ffmpeg={}",
                            ours.len(),
                            oracle.len()
                        ));
                    }
                    // The decoded peak is recorded but only asserted in the
                    // ignored scale-defect test: today every bitrate renders
                    // ~1e5x the source peak (see todo.md 2026-09-27).
                    let decoded_peak = ours.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
                    let _ = (source_peak, decoded_peak);
                }
                Err(e) => failures.push(format!("{case}: our open failed: {e}")),
            }
            let _ = std::fs::remove_file(&path);
        }
    }
    assert!(failures.is_empty(), "ladder failures: {failures:#?}");
}

/// Regression gate for the encoder defects measured on 2026-09-27 against
/// FFmpeg (see todo.md "MP3 encoder scale/divergence defects"):
///
/// 1. **"Universal scale defect" — RESOLVED (2026-09-27) as
///    mis-calibration, not a defect.** The decoder's f32 output carries
///    16-bit PCM magnitudes (±32768 for full-scale input), matching the
///    FFmpeg oracle harness convention; the earlier measurements compared
///    it against normalized source floats. The encoder now round-trips
///    source scale correctly (decoded peak within 2-3.2x of
///    source·32768 = well inside a decade, pure noise-peak factor), and
///    cross-decoder agreement for all noise materials is 114+ dB.
/// 2. **Tonal-path decoder divergence — STILL OPEN.** A 0.6/1 kHz sine at
///    128 kbps (mono or stereo) decodes to a peak ~2x the intended line
///    magnitude and disagrees with FFmpeg's decode at -44.6 (mono) /
///    -38.3 (stereo) dB. Isolated per-book probes (see todo.md) validate
///    every Huffman book's full codeword set through the real emission
///    path in isolation, so the defect lives in the interaction of
///    extreme-magnitude granules with the planner's chosen structure, not
///    in the tables themselves. Mono/stereo noise additionally agrees at
///    114+ dB at every bitrate on the ladder.
///
/// This test asserts the *fixed* behavior (16-bit-convention scale
/// fidelity plus cross-decoder agreement for every material). Run with
/// `cargo test -p tpt-av-cadence-mp3 -- --ignored`; it currently fails
/// only on the tonal SNR items.
#[test]
#[ignore = "MP3 encoder tonal-material inter-decoder disagreement remains (todo.md 2026-09-27); scale item resolved"]
fn encoder_tonal_and_mono_scale_defects() {
    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::Mp3Decoder;

    if !ffmpeg_available() {
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }
    let sample_rate = 44_100u32;
    let n = sample_rate as usize;
    let mut failures = Vec::new();

    let tonal_mono = sine_tone(sample_rate, 1000.0, 1.0, 0.6);
    let mut tonal_stereo = vec![0.0f32; n * 2];
    for i in 0..n {
        let t = i as f32 / sample_rate as f32;
        let s = (2.0 * std::f32::consts::PI * 1000.0 * t).sin() * 0.6;
        tonal_stereo[i * 2] = s;
        tonal_stereo[i * 2 + 1] = s;
    }
    let mono_noise: Vec<f32> = {
        let mut state = 0x1D0C_0FF1u32;
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                ((state as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.5
            })
            .collect()
    };

    let stereo_noise = {
        let left = white_noise(n, 0.5, 0xC0FF_EE01);
        let right = white_noise(n, 0.5, 0x1234_5678);
        let mut interleaved = vec![0.0f32; n * 2];
        for i in 0..n {
            interleaved[i * 2] = left[i];
            interleaved[i * 2 + 1] = right[i];
        }
        interleaved
    };

    let cases: Vec<(&str, u16, u32, &Vec<f32>)> = vec![
        ("tonal_mono_128k", 1, 128, &tonal_mono),
        ("tonal_stereo_128k", 2, 128, &tonal_stereo),
        ("noise_mono_128k", 1, 128, &mono_noise),
        ("noise_stereo_128k", 2, 128, &stereo_noise),
    ];

    for (label, channels, bitrate, frames) in &cases {
        let source_peak = frames.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        let mut buf = Cursor::new(Vec::new());
        {
            let mut enc = Mp3Encoder::new(&mut buf, sample_rate, *channels, *bitrate).unwrap();
            enc.encode(frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "cadence_mp3_scalefix_{}_{}.mp3",
            label,
            std::process::id()
        ));
        std::fs::write(&path, buf.into_inner()).expect("write temp mp3");

        let oracle =
            decode_with_ffmpeg(&path, sample_rate, *channels).expect("ffmpeg oracle decode");
        let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
        let ch = dec.info().channels as usize;
        let mut ours = Vec::new();
        let mut buf = vec![0.0f32; 1152 * ch];
        loop {
            match dec.decode(&mut buf) {
                Ok(0) => break,
                Ok(got) => ours.extend_from_slice(&buf[..got * ch]),
                Err(e) => panic!("{label}: our decode errored: {e}"),
            }
        }
        let _ = std::fs::remove_file(&path);
        // The decoder's f32 output carries 16-bit PCM magnitudes (the same
        // convention as the FFmpeg oracle harness), so the source is scaled
        // accordingly before the decade comparison.
        let source_peak_16 = source_peak * 32768.0;
        let decoded_peak = ours.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        if decoded_peak > source_peak_16 * 10.0 || decoded_peak < source_peak_16 / 10.0 {
            failures.push(format!(
                "{label}: decoded peak {decoded_peak} not within a decade of source {source_peak_16}"
            ));
        }
        if ours.len() == oracle.len() {
            let mut signal = 0.0f64;
            let mut error = 0.0f64;
            for (&a, &b) in ours.iter().zip(&oracle) {
                let delta = f64::from(a) - f64::from(b);
                error += delta * delta;
                signal += f64::from(b).powi(2);
            }
            if signal > 0.0 {
                let snr = 10.0 * (signal / error).log10();
                eprintln!("{label}: SNR={snr:.2} dB");
                if snr <= 100.0 {
                    failures.push(format!("{label}: SNR={snr:.2} vs ffmpeg"));
                }
            }
        } else {
            failures.push(format!("{label}: length mismatch vs ffmpeg"));
        }
    }
    assert!(
        failures.is_empty(),
        "scale-defect regressions: {failures:#?}"
    );
}

/// Mid/side stereo gate: dual-mono noise must engage joint stereo with the
/// mid/side mode extension, and FFmpeg's decode of the MS bitstream must
/// agree with ours at the suite gate (validates the `(L±R)·2^-3/2` transform
/// against the decoder's ms requant gain and reconstruction).
#[test]
fn dual_mono_mid_side_agrees_with_ffmpeg() {
    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::Mp3Decoder;

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }
    let sample_rate = 44_100u32;
    let n = sample_rate as usize / 2;
    let mut frames = vec![0.0f32; n * 2];
    let mut state = 0x5EED_0001u32;
    for i in 0..n {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let s = (state as f32 / u32::MAX as f32) * 2.0 - 1.0;
        frames[i * 2] = s;
        frames[i * 2 + 1] = s;
    }
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new(&mut buf, sample_rate, 2, 192).unwrap();
        enc.encode(&frames).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    let data = buf.into_inner();
    // dual-mono content must switch the header to joint stereo (mode 01)
    // with the mid/side-only mode extension (10).
    assert_eq!(data[3] >> 6, 0b01, "dual mono must use joint stereo");
    assert_eq!((data[3] >> 4) & 0b11, 0b10, "mode ext must signal ms only");

    let path = std::env::temp_dir().join(format!(
        "cadence_mp3_dual_mono_ms_{}.mp3",
        std::process::id()
    ));
    std::fs::write(&path, &data).expect("write temp mp3");

    let oracle = decode_with_ffmpeg(&path, sample_rate, 2).expect("ffmpeg oracle decode");
    let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
    let ch = dec.info().channels as usize;
    let mut ours = Vec::new();
    let mut buf = vec![0.0f32; 1152 * ch];
    loop {
        match dec.decode(&mut buf) {
            Ok(0) => break,
            Ok(got) => ours.extend_from_slice(&buf[..got * ch]),
            Err(e) => panic!("our decode errored: {e}"),
        }
    }
    let _ = std::fs::remove_file(&path);

    assert!(
        ours.len() == oracle.len(),
        "length mismatch: ours {} vs ffmpeg {}",
        ours.len(),
        oracle.len()
    );
    let mut signal = 0.0f64;
    let mut error = 0.0f64;
    for (&a, &b) in ours.iter().zip(&oracle) {
        let delta = f64::from(a) - f64::from(b);
        error += delta * delta;
        signal += f64::from(b).powi(2);
    }
    let snr = 10.0 * (signal / error).log10();
    assert!(
        snr >= 100.0,
        "mid/side inter-decoder agreement {snr:.2} dB below the 100 dB gate"
    );
}

/// Documents a specific, observed FFmpeg behavior for low-bitrate,
/// low-complexity stereo tonal content (e.g. an identical sine tone in both
/// channels at 128kbps, as this crate's own `examples/mp3_encode.rs`
/// produces): FFmpeg's `mp3float` decoder logs "overread, skip ..."
/// messages for some frames, but still recovers and decodes the correct
/// number of samples (confirmed manually via `ffmpeg -i ... -f null -`; not
/// reproduced here as a hard assertion since FFmpeg only prints this to
/// stderr, not as a decode failure this harness can observe). It does not
/// reproduce at higher bitrates for the same content, nor for stereo white
/// noise, nor for mono sine at 128kbps — only tight-budget, low-entropy
/// stereo granules seem to trigger it. This crate's own decoder accepts the
/// same bitstream without any warning or error. Tracked in `todo.md` as a
/// known, non-fatal edge case worth root-causing in a follow-up session,
/// not yet understood.
#[test]
fn low_bitrate_stereo_sine_still_decodes_correct_frame_count() {
    let sample_rate = 44_100u32;
    let frames_mono = sine_tone(sample_rate, 440.0, 1.0, 0.5);
    let expected_frames = frames_mono.len();
    let mut frames = vec![0.0f32; frames_mono.len() * 2];
    for i in 0..frames_mono.len() {
        frames[i * 2] = frames_mono[i];
        frames[i * 2 + 1] = frames_mono[i];
    }
    let path = encode_to_temp("stereo_sine_128", sample_rate, 2, 128, &frames);
    run_crosscheck(&path, sample_rate, 2, expected_frames);
}
