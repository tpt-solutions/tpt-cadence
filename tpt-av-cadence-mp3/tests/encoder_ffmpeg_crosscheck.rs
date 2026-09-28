//! Cross-checks the MP3 *encoder* against FFmpeg: an independent decoder
//! must be able to decode this crate's own encoder output without erroring.
//!
//! This is the strongest correctness signal available for a lossy encoder:
//! decoding cleanly in FFmpeg (which implements the full ISO reference
//! Huffman/scalefactor/bit-reservoir logic) rules out the failure mode
//! where this crate's own decoder happens to accept a subtly malformed
//! bitstream because it shares a bug with the encoder. It is not a
//! fidelity check (see `tests/encoder_roundtrip.rs` for the round-trip
//! gates); it only asserts FFmpeg accepts the stream and produces the
//! expected number of samples.

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
        let _source_peak = frames.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
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
/// 1. **"Universal scale defect" — RESOLVED (2026-09-28, re-resolved).**
///    The true defect was a 2^16 analysis↔synthesis gain mismatch: the
///    encoder pre-scaled its input by 32768 on top of an analyzer↔synth
///    kernel pair that already carries 2^16, so every stream decoded
///    65536× too loud (clipping to full scale under FFmpeg) — invisible
///    to correlation-based gates, which is why it masqueraded as
///    "mis-calibration". The analyzer now runs at 0.5× input for unity
///    gain; the decoder's f32 output is true normalized PCM (±1 full
///    scale), the same convention as the FFmpeg oracle harness, and
///    cross-decoder agreement is 114-120 dB for all measured materials.
/// 2. **Tonal-path decoder divergence — RESOLVED (2026-09-27, later the
///    same day).** Root cause: FFmpeg's `l3_unscale` requantizes
///    escape-coded lines (|ix| >= 15) through an integer mantissa shift
///    with a zero-return guard for shifts outside [0, 31]; at the global
///    gains our rate loop produced (gg ~ 250), every escape line fell
///    outside that window and decoded as exact ZERO in FFmpeg while our
///    float path rendered them at full precision. The encoder now zeroes
///    out-of-window escape lines at quantization time (so encoder, our
///    decoder, and FFmpeg agree) and the gg search treats such plans as
///    non-fitting, rising to gains where the content is representable
///    (LAME does the same: its granules for this material sit at
///    gg <= ~103). Measured: tonal mono/stereo agree with FFmpeg at
///    120+ dB (was -44/-38 dB before the fix).
///
/// This test asserts the fixed behavior (16-bit-convention scale fidelity
/// plus cross-decoder agreement for every material)./// This test asserts the *fixed* behavior (16-bit-convention scale
/// fidelity plus cross-decoder agreement for every material). Run with
/// `cargo test -p tpt-av-cadence-mp3 -- --ignored`; it currently fails
/// only on the tonal SNR items.
#[test]
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
        // The decoder's f32 output is true normalized PCM (±1 full scale,
        // the same convention as the FFmpeg oracle harness) and the
        // encoder round-trips unity gain, so the decoded peak must track
        // the source peak directly.
        let decoded_peak = ours.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        if decoded_peak > source_peak * 10.0 || decoded_peak < source_peak / 10.0 {
            failures.push(format!(
                "{label}: decoded peak {decoded_peak} not within a decade of source {source_peak}"
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

/// MPEG-2/2.5 (LSF) encoder gate: every supported LSF sample rate must
/// produce streams that FFmpeg decodes in agreement with this crate's own
/// decoder, at unity absolute gain. Spans the MPEG-2 (16/22.05/24 kHz) and
/// MPEG-2.5 (8/11.025/12 kHz) families across low, mid, and high bitrates,
/// exercising the 8-bit `main_data_begin` reservoir cap, the 9-bit
/// mixed-radix `scalefac_compress` search, and the partitioned scalefactor
/// emission.
#[test]
fn lsf_encoder_agrees_with_ffmpeg() {
    use std::f32::consts::PI;

    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::Mp3Decoder;

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }

    let cases: &[(u32, u16, u32)] = &[
        (24000, 2, 128),
        (24000, 1, 56),
        (22050, 2, 80),
        (16000, 2, 64),
        (16000, 1, 32),
        (12000, 1, 32),
        (11025, 2, 48),
        (8000, 2, 24),
        (8000, 1, 8),
    ];
    let mut failures = Vec::new();
    for &(sample_rate, channels, kbps) in cases {
        // One second of tonal + noise content at a moderate level, slightly
        // decorrelated between channels for stereo cases.
        let n = sample_rate as usize;
        let mut frames = Vec::with_capacity(n * channels as usize);
        let mut state = 0xBEEF_u32;
        for i in 0..n {
            let t = i as f32 / sample_rate as f32;
            let tone = 0.3 * (2.0 * PI * 440.0 * t).sin() + 0.15 * (2.0 * PI * 1300.0 * t).sin();
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let noise = (state as f32 / u32::MAX as f32 - 0.5) * 0.1;
            let s = tone + noise;
            frames.push(s);
            if channels == 2 {
                frames.push(s * 0.9);
            }
        }
        let source_peak = frames.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        let mut buf = Cursor::new(Vec::new());
        {
            let mut enc = Mp3Encoder::new(&mut buf, sample_rate, channels, kbps).unwrap();
            enc.encode(&frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "cadence_mp3_lsf_{}_{}_{}.mp3",
            sample_rate,
            channels,
            std::process::id()
        ));
        std::fs::write(&path, buf.into_inner()).expect("write temp mp3");

        let oracle = match decode_with_ffmpeg(&path, sample_rate, channels) {
            Ok(o) => o,
            Err(e) => {
                failures.push(format!("{sample_rate}/{channels}/{kbps}: ffmpeg error {e}"));
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
        let ch = dec.info().channels as usize;
        let mut ours = Vec::new();
        let mut buf = vec![0.0f32; 4096 * ch];
        loop {
            match dec.decode(&mut buf) {
                Ok(0) => break,
                Ok(g) => ours.extend_from_slice(&buf[..g * ch]),
                Err(e) => panic!("{sample_rate} Hz: our decode errored: {e}"),
            }
        }
        let _ = std::fs::remove_file(&path);

        let label = format!("lsf_{sample_rate}_{channels}ch_{kbps}k");
        let decoded_peak = ours.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        if decoded_peak > source_peak * 10.0 || decoded_peak < source_peak / 10.0 {
            failures.push(format!(
                "{label}: decoded peak {decoded_peak} not within a decade of source {source_peak}"
            ));
        }
        if ours.len() != oracle.len() {
            failures.push(format!(
                "{label}: length mismatch ours {} vs ffmpeg {}",
                ours.len(),
                oracle.len()
            ));
            continue;
        }
        let (mut signal, mut error) = (0.0f64, 0.0f64);
        for (&a, &b) in ours.iter().zip(&oracle) {
            let delta = f64::from(a) - f64::from(b);
            error += delta * delta;
            signal += f64::from(b).powi(2);
        }
        if signal > 0.0 {
            let snr = 10.0 * (signal / error).log10();
            eprintln!("{label}: inter-decoder SNR={snr:.2} dB");
            if snr <= 100.0 {
                failures.push(format!("{label}: inter-decoder SNR={snr:.2} vs ffmpeg"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "LSF encoder regressions: {failures:#?}"
    );
}

/// VBR encoder gate: `Mp3Encoder::new_vbr` picks a bitrate index per frame;
/// the resulting mixed-rate stream must still decode in FFmpeg in agreement
/// with this crate's decoder (per-frame self-describing headers + the bit
/// reservoir absorbing the size differences), at unity absolute gain, and
/// actually vary the bitrate across frames.
#[test]
fn vbr_encoder_agrees_with_ffmpeg() {
    use std::f32::consts::PI;

    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::Mp3Decoder;

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }

    let cases: &[(u32, u16, u8)] = &[(44_100, 2, 3), (48_000, 1, 6), (24_000, 2, 4)];
    let mut failures = Vec::new();
    for &(sample_rate, channels, quality) in cases {
        // One second of tonal + noise content at a moderate level.
        let n = sample_rate as usize;
        let mut frames = Vec::with_capacity(n * channels as usize);
        let mut state = 0xCAFE_u32;
        for i in 0..n {
            let t = i as f32 / sample_rate as f32;
            let tone = 0.3 * (2.0 * PI * 440.0 * t).sin() + 0.15 * (2.0 * PI * 1500.0 * t).sin();
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let noise = (state as f32 / u32::MAX as f32 - 0.5) * 0.1;
            let s = tone + noise;
            frames.push(s);
            if channels == 2 {
                frames.push(s * 0.9);
            }
        }
        let source_peak = frames.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        let mut buf = Cursor::new(Vec::new());
        {
            let mut enc = Mp3Encoder::new_vbr(&mut buf, sample_rate, channels, quality).unwrap();
            enc.encode(&frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        let data = buf.into_inner();
        let label = format!("vbr_{sample_rate}_{channels}ch_q{quality}");

        // The stream must contain more than one bitrate index (quality
        // varies slightly across the material), and FFmpeg must accept it.
        let mut indexes = std::collections::BTreeSet::new();
        let mut off = 0usize;
        while off + 4 <= data.len() {
            indexes.insert(u32::from(data[off + 2]) >> 4);
            // Frame span from the ISO formula (sample rate family-aware).
            let kbps = match (data[off + 1] >> 3) & 3 {
                3 => [
                    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
                ][(data[off + 2] >> 4) as usize],
                _ => [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160]
                    [(data[off + 2] >> 4) as usize],
            };
            let id_bits = (data[off + 1] >> 3) & 3;
            assert!(id_bits != 1, "reserved version ID");
            let sr_family = if id_bits == 3 { 1u32 } else { 2 };
            let base = match (data[off + 2] >> 2) & 3 {
                0 => 44100,
                1 => 48000,
                2 => 32000,
                _ => panic!("reserved sample rate"),
            };
            let sr = base / sr_family / if id_bits == 0 { 2 } else { 1 };
            let samples: u32 = if id_bits == 3 { 1152 } else { 576 };
            let span = samples * kbps * 125 / sr + ((data[off + 2] >> 1) & 1) as u32;
            off += span as usize;
        }
        assert_eq!(off, data.len(), "{label}: frame spans must tile exactly");
        assert!(
            indexes.len() > 1,
            "{label}: VBR must vary the bitrate index, saw {indexes:?}"
        );

        let path = std::env::temp_dir().join(format!(
            "cadence_mp3_vbr_{}_{}_{}.mp3",
            sample_rate,
            channels,
            std::process::id()
        ));
        std::fs::write(&path, &data).expect("write temp mp3");
        let oracle = match decode_with_ffmpeg(&path, sample_rate, channels) {
            Ok(o) => o,
            Err(e) => {
                failures.push(format!("{label}: ffmpeg error {e}"));
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
        let ch = dec.info().channels as usize;
        let mut ours = Vec::new();
        let mut buf = vec![0.0f32; 4096 * ch];
        loop {
            match dec.decode(&mut buf) {
                Ok(0) => break,
                Ok(g) => ours.extend_from_slice(&buf[..g * ch]),
                Err(e) => panic!("{label}: our decode errored: {e}"),
            }
        }
        let _ = std::fs::remove_file(&path);

        let decoded_peak = ours.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        if decoded_peak > source_peak * 10.0 || decoded_peak < source_peak / 10.0 {
            failures.push(format!(
                "{label}: decoded peak {decoded_peak} not within a decade of source {source_peak}"
            ));
        }
        if ours.len() != oracle.len() {
            failures.push(format!(
                "{label}: length mismatch ours {} vs ffmpeg {}",
                ours.len(),
                oracle.len()
            ));
            continue;
        }
        let (mut signal, mut error) = (0.0f64, 0.0f64);
        for (&a, &b) in ours.iter().zip(&oracle) {
            let delta = f64::from(a) - f64::from(b);
            error += delta * delta;
            signal += f64::from(b).powi(2);
        }
        if signal > 0.0 {
            let snr = 10.0 * (signal / error).log10();
            eprintln!("{label}: inter-decoder SNR={snr:.2} dB");
            if snr <= 100.0 {
                failures.push(format!("{label}: inter-decoder SNR={snr:.2} vs ffmpeg"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "VBR encoder regressions: {failures:#?}"
    );
}
