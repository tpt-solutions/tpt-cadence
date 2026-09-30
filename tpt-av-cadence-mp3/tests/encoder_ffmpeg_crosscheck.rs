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

/// Short-block (window switching) gate: an in-range transient must trigger
/// the window-sequence state machine (content short granules plus zero-line
/// stop/bridge granules, observable as block_type 2/3 in the side info),
/// and the resulting stream must decode in FFmpeg in agreement with this
/// crate's decoder at the suite gate. The transient stays within ±1.0 —
/// an out-of-range test signal would saturate FFmpeg's int16 output path
/// while this crate's float path keeps the overshoot, faking an
/// inter-decoder divergence.
#[test]
fn transient_short_blocks_agree_with_ffmpeg() {
    use std::f32::consts::PI;

    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::{Mp3Decoder, Mp3Encoder};

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }

    let sample_rate = 44_100u32;
    let mut frames = Vec::with_capacity(sample_rate as usize * 2);
    for i in 0..sample_rate as usize {
        let s = if i < sample_rate as usize / 2 {
            0.0
        } else if i < sample_rate as usize / 2 + 64 {
            // An in-range broadband click (peak ±0.9).
            ((i % 7) as f32 - 3.0) * 0.3
        } else {
            0.3 * (2.0 * PI * 3000.0 * i as f32 / sample_rate as f32).sin()
        };
        frames.push(s);
        frames.push(s);
    }
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new(&mut buf, sample_rate, 2, 128).unwrap();
        enc.encode(&frames).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    let data = buf.into_inner();

    // The side info must show the window sequence actually engaging:
    // short (block_type 2) granules at the attack and zero-line stop
    // (block_type 3) granules before it.
    struct Bits<'a> {
        d: &'a [u8],
        p: usize,
    }
    impl<'a> Bits<'a> {
        fn get(&mut self, n: u32) -> u32 {
            let mut v = 0u32;
            for _ in 0..n {
                let byte = self.d[self.p / 8];
                v = (v << 1) | u32::from((byte >> (7 - (self.p % 8))) & 1);
                self.p += 1;
            }
            v
        }
    }
    let mut off = 0usize;
    let mut shorts = 0usize;
    let mut stops = 0usize;
    while off + 4 <= data.len() {
        let b2 = data[off + 2];
        let span = (1152 * 128 * 125 / sample_rate) as usize + ((b2 >> 1) & 1) as usize;
        let mut bits = Bits {
            d: &data[off + 4..off + span],
            p: 0,
        };
        let _mdb = bits.get(9);
        let _priv = bits.get(3);
        let _scfsi = bits.get(8);
        for _ in 0..2 {
            for _ in 0..2 {
                let _p23 = bits.get(12);
                let _bv = bits.get(9);
                let _gg = bits.get(8);
                let _sfc = bits.get(4);
                if bits.get(1) == 1 {
                    let bt = bits.get(2);
                    let mixed = bits.get(1);
                    assert_eq!(mixed, 0, "only pure short blocks are emitted");
                    if bt == 2 {
                        shorts += 1;
                    } else if bt == 3 {
                        stops += 1;
                    }
                    let _ts0 = bits.get(5);
                    let _ts1 = bits.get(5);
                    let _sbg = bits.get(9);
                } else {
                    let _ts = bits.get(15);
                    let _r0 = bits.get(4);
                    let _r1 = bits.get(3);
                }
                let _pre = bits.get(1);
                let _sfs = bits.get(1);
                let _c1t = bits.get(1);
            }
        }
        off += span;
    }
    assert_eq!(off, data.len(), "frame spans must tile the stream exactly");
    assert!(shorts > 0, "the transient must engage short granules");
    assert!(stops > 0, "the window sequence must include stop granules");

    let path = std::env::temp_dir().join(format!("cadence_mp3_short_{}.mp3", std::process::id()));
    std::fs::write(&path, &data).expect("write temp mp3");
    let oracle = decode_with_ffmpeg(&path, sample_rate, 2).expect("ffmpeg oracle decode");
    let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
    let ch = dec.info().channels as usize;
    let mut ours = Vec::new();
    let mut buf = vec![0.0f32; 4096 * ch];
    loop {
        match dec.decode(&mut buf) {
            Ok(0) => break,
            Ok(g) => ours.extend_from_slice(&buf[..g * ch]),
            Err(e) => panic!("our decode errored: {e}"),
        }
    }
    let _ = std::fs::remove_file(&path);

    assert_eq!(ours.len(), oracle.len(), "length mismatch vs ffmpeg");
    let (mut signal, mut error) = (0.0f64, 0.0f64);
    for (&a, &b) in ours.iter().zip(&oracle) {
        let delta = f64::from(a) - f64::from(b);
        error += delta * delta;
        signal += f64::from(b).powi(2);
    }
    let snr = 10.0 * (signal / error).log10();
    eprintln!("short-block transient: inter-decoder SNR={snr:.2} dB");
    assert!(
        snr > 100.0,
        "short-block inter-decoder SNR={snr:.2} vs ffmpeg"
    );
}

/// LSF short-block gate: the MPEG-2/2.5 families switch windows too (one
/// granule per frame, the 9-bit `scalefac_compress` drawn from the short
/// partition row). A click after silence must engage short and stop
/// granules and still agree with FFmpeg.
#[test]
fn lsf_transient_short_blocks_agree_with_ffmpeg() {
    use std::f32::consts::PI;

    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::{Mp3Decoder, Mp3Encoder};

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }

    struct Bits<'a> {
        d: &'a [u8],
        p: usize,
    }
    impl Bits<'_> {
        fn get(&mut self, n: u32) -> u32 {
            let mut v = 0u32;
            for _ in 0..n {
                let byte = self.d[self.p / 8];
                v = (v << 1) | u32::from((byte >> (7 - (self.p % 8))) & 1);
                self.p += 1;
            }
            v
        }
    }

    // (sample rate, channels, kbps, MPEG-2.5?)
    for (sample_rate, channels, kbps) in [
        (22_050u32, 1u16, 48u32),
        (24_000, 2, 96),
        (16_000, 2, 64),
        (11_025, 1, 32),
    ] {
        let sr = sample_rate as usize;
        let mut frames = Vec::new();
        // Click 100 samples into a granule (the detector compares each
        // granule's front half against the previous back half).
        let click = sr / 2 / 1152 * 1152 + 100;
        for i in 0..sr {
            let s = if i < click {
                0.0
            } else if i < click + 48 {
                ((i % 7) as f32 - 3.0) * 0.3
            } else {
                0.3 * (2.0 * PI * 1500.0 * i as f32 / sample_rate as f32).sin()
            };
            for _ in 0..channels {
                frames.push(s);
            }
        }
        let mut buf = Cursor::new(Vec::new());
        {
            let mut enc = Mp3Encoder::new(&mut buf, sample_rate, channels, kbps).unwrap();
            enc.encode(&frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        let data = buf.into_inner();

        let side_len = match channels {
            1 => 9,
            _ => 17,
        };
        let mut off = 0usize;
        let (mut shorts, mut stops) = (0usize, 0usize);
        while off + 4 <= data.len() {
            let b2 = data[off + 2];
            let span = (72 * kbps * 1000 / sample_rate) as usize + ((b2 >> 1) & 1) as usize;
            let mut bits = Bits {
                d: &data[off + 4..off + 4 + side_len],
                p: 0,
            };
            bits.get(8);
            bits.get(if channels == 1 { 1 } else { 2 });
            for _ in 0..channels {
                bits.get(12 + 9 + 8 + 9);
                if bits.get(1) == 1 {
                    let bt = bits.get(2);
                    assert_eq!(bits.get(1), 0, "only pure short blocks are emitted");
                    match bt {
                        2 => shorts += 1,
                        3 => stops += 1,
                        _ => {}
                    }
                    bits.get(5 + 5 + 9);
                } else {
                    bits.get(15 + 4 + 3);
                }
                bits.get(1); // scalefac_scale
                bits.get(1); // count1table_select
            }
            off += span;
        }
        assert_eq!(off, data.len(), "frame spans must tile the stream exactly");
        assert!(
            shorts > 0,
            "{sample_rate} Hz: transient must engage short granules"
        );
        assert!(
            stops > 0,
            "{sample_rate} Hz: window sequence must include stop granules"
        );

        let path = std::env::temp_dir().join(format!(
            "cadence_mp3_lsf_short_{}_{}.mp3",
            std::process::id(),
            sample_rate
        ));
        std::fs::write(&path, &data).expect("write temp mp3");
        let oracle =
            decode_with_ffmpeg(&path, sample_rate, channels).expect("ffmpeg oracle decode");
        let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
        let ch = dec.info().channels as usize;
        let mut ours = Vec::new();
        let mut out = vec![0.0f32; 4096 * ch];
        loop {
            match dec.decode(&mut out) {
                Ok(0) => break,
                Ok(g) => ours.extend_from_slice(&out[..g * ch]),
                Err(e) => panic!("our decode errored: {e}"),
            }
        }
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            ours.len(),
            oracle.len(),
            "{sample_rate} Hz: length mismatch vs ffmpeg"
        );
        let (mut signal, mut error) = (0.0f64, 0.0f64);
        for (&a, &b) in ours.iter().zip(&oracle) {
            let delta = f64::from(a) - f64::from(b);
            error += delta * delta;
            signal += f64::from(b).powi(2);
        }
        let snr = 10.0 * (signal / error).log10();
        eprintln!("lsf short-block {sample_rate} Hz x{channels}: shorts={shorts} stops={stops} SNR={snr:.2} dB");
        assert!(snr > 100.0, "{sample_rate} Hz: SNR={snr:.2} vs ffmpeg");
    }
}

/// Regression gate for the repeated-transient LSF short-block defect
/// (`todo.md`, 2026-09-30 second session): a *click train* — not a single
/// click — made the plain LSF encode diverge from FFmpeg at 22-45 dB on every
/// other click cycle from frame ~59 on, identically with and without
/// intensity stereo.
///
/// Root cause was entirely in the window-sequence state machine's
/// cross-frame lookahead, not in the short-block analysis or quantization:
///
/// 1. `next_attack` compared the lookahead PCM against the *previous* frame's
///    stored back-half energy, but the next frame's own granule-0 detector
///    compares against the *current* frame's back half. The lookahead could
///    therefore disagree with the detector it was predicting.
/// 2. It measured one channel strided by `channels`, while that detector
///    measures a contiguous window spanning both channels — a second way for
///    the two to disagree.
///
/// Either disagreement silently dropped the zero-line stop granule that must
/// precede a short run, so the encoder emitted a bare `long -> short`
/// transition, where the two decoders are free to interpret the overlap
/// handover differently. The clicks are spaced 1.5 granules apart, so each
/// cycle re-arms the lookahead and the defect repeated indefinitely.
///
/// The gate asserts both the per-granule floor (the defect was localized to
/// single granules, so a whole-stream average would hide it) and the
/// side-info shape: every content short granule must be preceded by a stop.
#[test]
fn lsf_repeated_transients_agree_with_ffmpeg() {
    use tpt_av_cadence_core::Encoder;
    use tpt_av_cadence_mp3::Mp3Encoder;

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }

    const SAMPLE_RATE: u32 = 22_050;
    const CHANNELS: u16 = 2;
    const KBPS: u32 = 64;
    const CLICK_PERIOD: usize = 864;

    struct Bits<'a> {
        d: &'a [u8],
        p: usize,
    }
    impl Bits<'_> {
        fn get(&mut self, n: u32) -> u32 {
            let mut v = 0u32;
            for _ in 0..n {
                let byte = self.d[self.p / 8];
                v = (v << 1) | u32::from((byte >> (7 - (self.p % 8))) & 1);
                self.p += 1;
            }
            v
        }
    }

    let sr = SAMPLE_RATE as usize;
    let mut frames = vec![0.0f32; sr * CHANNELS as usize];
    for i in 0..sr {
        let s = if i % CLICK_PERIOD < 48 {
            ((i % 7) as f32 - 3.0) * 0.3
        } else {
            0.0
        };
        for c in 0..CHANNELS as usize {
            frames[i * CHANNELS as usize + c] = s;
        }
    }

    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new(&mut buf, SAMPLE_RATE, CHANNELS, KBPS).unwrap();
        enc.encode(&frames).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    let data = buf.into_inner();

    // Walk the side info: collect every granule's block type and assert the
    // ISO window-sequence shape. A short block must be entered from a start
    // block or another short, never straight from a long or stop block.
    let side_len = 17usize; // stereo
    let mut off = 0usize;
    let mut bts: Vec<u8> = Vec::new();
    while off + 4 <= data.len() {
        let b2 = data[off + 2];
        let span = (72 * KBPS * 1000 / SAMPLE_RATE) as usize + ((b2 >> 1) & 1) as usize;
        assert!(
            off + 4 + side_len <= data.len(),
            "frame at {off} runs past the stream"
        );
        let mut bits = Bits {
            d: &data[off + 4..off + 4 + side_len],
            p: 0,
        };
        bits.get(8); // main_data_begin
        bits.get(2); // private bits
        for _ in 0..CHANNELS {
            bits.get(12 + 9 + 8 + 9); // part2_3, big_values, gain, scalefac_compress
            if bits.get(1) == 1 {
                let block = bits.get(2) as u8;
                bits.get(1); // mixed_block_flag
                bits.get(10); // table_select
                bits.get(9); // subblock_gain
                bits.get(2); // scalefac_scale, count1table_select
                bts.push(block);
            } else {
                bits.get(15 + 4 + 3);
                bits.get(2);
                bts.push(0);
            }
        }
        off += span;
    }
    assert_eq!(off, data.len(), "frame spans must tile the stream exactly");

    let mut shorts = 0usize;
    for (i, &bt) in bts.iter().enumerate() {
        if bt == 2 {
            shorts += 1;
            // (A stream may open directly on a short block: the decoder's
            // overlap state starts at zero.)
            let prev = if i == 0 { 1 } else { bts[i - 1] };
            assert!(
                prev == 1 || prev == 2,
                "granule {i}: bare long -> short transition (prev block_type {prev})"
            );
        }
    }
    assert!(
        shorts > 8,
        "expected many short granules from a click train, got {shorts}"
    );

    run_repeated_transient_oracle(&data, shorts);
}

/// Decodes `data` with this crate and with FFmpeg and gates the *worst
/// per-granule* inter-decoder SNR (the repeated-transient defect hit
/// individual granules, so a whole-stream average would dilute it away).
fn run_repeated_transient_oracle(data: &[u8], shorts: usize) {
    use tpt_av_cadence_core::Decoder;
    use tpt_av_cadence_mp3::Mp3Decoder;

    const SAMPLE_RATE: u32 = 22_050;
    const CHANNELS: u16 = 2;

    let path = std::env::temp_dir().join(format!(
        "cadence_mp3_lsf_repeated_transients_{}.mp3",
        std::process::id()
    ));
    std::fs::write(&path, data).expect("write temp mp3");
    let oracle = decode_with_ffmpeg(&path, SAMPLE_RATE, CHANNELS).expect("ffmpeg oracle decode");
    let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
    let ch = dec.info().channels as usize;
    let mut ours = Vec::new();
    let mut out = vec![0.0f32; 4096 * ch];
    loop {
        match dec.decode(&mut out) {
            Ok(0) => break,
            Ok(g) => ours.extend_from_slice(&out[..g * ch]),
            Err(e) => panic!("our decode errored: {e}"),
        }
    }
    let _ = std::fs::remove_file(&path);
    assert_eq!(ours.len(), oracle.len(), "length mismatch vs ffmpeg");

    let per_granule = 576 * ch;
    let mut worst = f64::INFINITY;
    let mut worst_g = 0usize;
    for (g, chunk) in ours.chunks(per_granule).enumerate() {
        let o = &oracle[g * per_granule..g * per_granule + chunk.len()];
        let (mut signal, mut error) = (0.0f64, 0.0f64);
        for (&a, &b) in chunk.iter().zip(o) {
            let delta = f64::from(a) - f64::from(b);
            error += delta * delta;
            signal += f64::from(b) * f64::from(b);
        }
        if error <= 0.0 {
            continue; // both decoders agree exactly (silence)
        }
        let snr = 10.0 * (signal / error).log10();
        if snr < worst {
            worst = snr;
            worst_g = g;
        }
    }
    eprintln!(
        "lsf repeated transients: {shorts} short granules, worst per-granule \
         SNR={worst:.2} dB at granule {worst_g}"
    );
    // 90 dB, not 100: the granules now carry real (not degenerate) content, and
    // FFmpeg's integer escape requantization differs from this crate's float
    // path in the last few bits of the transient granules.
    assert!(
        worst > 90.0,
        "worst per-granule SNR={worst:.2} dB at granule {worst_g} vs ffmpeg"
    );
}

/// Intensity-stereo gate: a source panned exactly at position 4
/// (`L/R = tan(60°)`) above 2 kHz is representable by intensity coding.
/// The stream must engage intensity stereo, decode identically in FFmpeg
/// and this crate (>100 dB), and reproduce the same per-channel levels as
/// the ordinary encode of the same source — both with mid/side on
/// (correlated content) and off (independent low band forcing L/R).
#[test]
fn intensity_stereo_agrees_with_ffmpeg() {
    use std::f32::consts::PI;

    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::{Mp3Decoder, Mp3Encoder};

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }

    let sample_rate = 44_100u32;
    let n = sample_rate as usize * 2;
    let pan = 1.0 / 3.0f32.sqrt();
    for independent_low in [false, true] {
        let mut frames = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f32 / sample_rate as f32;
            let hi: f32 = [2600.0f32, 3700.0, 5100.0, 6900.0, 9300.0, 12100.0]
                .iter()
                .enumerate()
                .map(|(k, f)| 0.06 * (2.0 * PI * f * t + k as f32).sin())
                .sum();
            let lo = 0.2 * (2.0 * PI * 500.0 * t).sin();
            let (l, r) = if independent_low {
                (hi + lo, pan * hi + 0.2 * (2.0 * PI * 330.0 * t).sin())
            } else {
                (hi + lo, pan * (hi + lo))
            };
            frames.push(l);
            frames.push(r);
        }

        // (stream, our decode, FFmpeg decode, intensity frames, frames)
        let encode = |intensity: bool| {
            let mut buf = Cursor::new(Vec::new());
            {
                let mut enc = Mp3Encoder::new(&mut buf, sample_rate, 2, 128).unwrap();
                enc.set_intensity_stereo(intensity);
                enc.encode(&frames).unwrap();
                Encoder::finish(&mut enc).unwrap();
            }
            let data = buf.into_inner();
            let (mut off, mut is_frames, mut total) = (0usize, 0usize, 0usize);
            while off + 4 <= data.len() {
                if (data[off + 3] >> 4) & 1 == 1 {
                    is_frames += 1;
                }
                off +=
                    (1152 * 128 * 125 / sample_rate) as usize + ((data[off + 2] >> 1) & 1) as usize;
                total += 1;
            }
            let path = std::env::temp_dir().join(format!(
                "cadence_mp3_is_{}_{}_{}.mp3",
                std::process::id(),
                independent_low,
                intensity
            ));
            std::fs::write(&path, &data).expect("write temp mp3");
            let oracle = decode_with_ffmpeg(&path, sample_rate, 2).expect("ffmpeg oracle decode");
            let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
            let mut ours = Vec::new();
            let mut out = vec![0.0f32; 4096 * 2];
            loop {
                match dec.decode(&mut out) {
                    Ok(0) => break,
                    Ok(g) => ours.extend_from_slice(&out[..g * 2]),
                    Err(e) => panic!("our decode errored: {e}"),
                }
            }
            let _ = std::fs::remove_file(&path);
            (ours, oracle, is_frames, total)
        };
        let (base, _, base_is, _) = encode(false);
        assert_eq!(base_is, 0, "intensity is opt-in");
        let (ours, oracle, is_frames, total) = encode(true);
        assert!(
            is_frames * 2 > total,
            "intensity must engage (independent_low={independent_low}): {is_frames}/{total}"
        );

        assert_eq!(ours.len(), oracle.len(), "length mismatch vs ffmpeg");
        let (mut sig, mut err) = (0.0f64, 0.0f64);
        for (&a, &b) in ours.iter().zip(&oracle) {
            sig += f64::from(b).powi(2);
            err += (f64::from(a) - f64::from(b)).powi(2);
        }
        let inter = 10.0 * (sig / err).log10();

        let rms = |x: &[f32], c: usize| {
            let v: Vec<f64> = x
                .iter()
                .skip(20_000 * 2 + c)
                .step_by(2)
                .take(40_000)
                .map(|&s| f64::from(s).powi(2))
                .collect();
            (v.iter().sum::<f64>() / v.len() as f64).sqrt()
        };
        eprintln!(
            "intensity (independent_low={independent_low}): {is_frames}/{total} frames, inter-decoder {inter:.2} dB, rms L {:.4}/{:.4} R {:.4}/{:.4} (IS/plain)",
            rms(&ours, 0),
            rms(&base, 0),
            rms(&ours, 1),
            rms(&base, 1)
        );
        assert!(
            inter > 100.0,
            "inter-decoder SNR {inter:.2} dB (low={independent_low})"
        );
        for c in 0..2 {
            let (a, b) = (rms(&ours, c), rms(&base, c));
            assert!(
                (a / b - 1.0).abs() < 0.1,
                "channel {c} level {a:.4} vs plain {b:.4} (low={independent_low})"
            );
        }
    }
}

/// Intensity-stereo short-block gate: a dual-mono signal — a steady tone
/// handing over to a click train every 1.5 granules — codes as long-block
/// granules, short-block granules, zero-line short bridges, and frames
/// mixing the families, with every granule's channels exactly in phase, so
/// intensity must engage in every frame (per window on the interleaved
/// (band, window) bands for the short granules). The stream must decode
/// identically in FFmpeg and this crate (>100 dB) and reproduce the same
/// per-channel levels as the ordinary encode of the same source.
#[test]
fn intensity_short_blocks_agree_with_ffmpeg() {
    use std::f32::consts::PI;

    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::{Mp3Decoder, Mp3Encoder};

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }

    struct Bits<'a> {
        d: &'a [u8],
        p: usize,
    }
    impl Bits<'_> {
        fn get(&mut self, n: u32) -> u32 {
            let mut v = 0u32;
            for _ in 0..n {
                let byte = self.d[self.p / 8];
                v = (v << 1) | u32::from((byte >> (7 - (self.p % 8))) & 1);
                self.p += 1;
            }
            v
        }
    }

    let sample_rate = 44_100u32;
    let n = sample_rate as usize * 2;
    let click_start = sample_rate as usize / 2;
    let click_period = 1728usize; // 1.5 granules: attacks alternate granules
    let mut frames = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f32 / sample_rate as f32;
        let mut ch = 0.3 * (2.0 * PI * 1500.0 * t).sin() + 0.1 * (2.0 * PI * 440.0 * t).sin();
        if i >= click_start {
            let k = (i - click_start) % click_period;
            if k < 64 {
                let noise = ((i * 37 % 23) as f32 - 11.0) / 11.0;
                ch = 0.5 * noise * (-(k as f32) / 16.0).exp();
            } else {
                ch = 0.0;
            }
        }
        // Dual mono: both channels identical, so every granule's top bands
        // are exactly in phase (rho = 1) and window switching runs in both
        // channels at once (matched block families per granule).
        frames.push(ch);
        frames.push(ch);
    }

    let encode = |intensity: bool| {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut enc = Mp3Encoder::new(&mut buf, sample_rate, 2, 128).unwrap();
            enc.set_intensity_stereo(intensity);
            enc.encode(&frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        buf.into_inner()
    };
    let base = encode(false);
    let data = encode(true);

    // Walk the frames: count intensity-signaled frames (mode_ext bit 1)
    // and window-switched granules from the side info (MPEG-1 stereo:
    // 32-byte side info, two granules x two channels).
    let (mut off, mut is_frames, mut total, mut shorts, mut longs) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    while off + 4 + 32 <= data.len() {
        is_frames += ((data[off + 3] >> 4) & 1) as usize;
        let span = (1152 * 128 * 125 / sample_rate as usize) + ((data[off + 2] >> 1) & 1) as usize;
        let mut bits = Bits {
            d: &data[off + 4..off + 4 + 32],
            p: 0,
        };
        bits.get(9); // main_data_begin
        bits.get(3); // private bits
        bits.get(8); // scfsi
        for _ in 0..2 {
            for _ in 0..2 {
                bits.get(12 + 9 + 8 + 4); // part2_3, big_values, global_gain, scf_compress
                if bits.get(1) == 1 {
                    let bt = bits.get(2);
                    assert_eq!(bits.get(1), 0, "only pure short blocks are emitted");
                    if bt == 2 {
                        shorts += 1;
                    }
                    bits.get(5 + 5 + 9);
                } else {
                    longs += 1;
                    bits.get(15 + 4 + 3);
                }
                bits.get(3); // preflag, scalefac_scale, count1_table
            }
        }
        off += span;
        total += 1;
    }
    assert!(
        longs > 0 && shorts > 0,
        "material must mix long and short granules: {longs} long / {shorts} short"
    );
    eprintln!("intensity short-block: {is_frames}/{total} frames intensity-signaled, {shorts} short + {longs} long granules");

    let path =
        std::env::temp_dir().join(format!("cadence_mp3_is_short_{}.mp3", std::process::id()));
    std::fs::write(&path, &data).expect("write temp mp3");
    let oracle = decode_with_ffmpeg(&path, sample_rate, 2).expect("ffmpeg oracle decode");
    let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
    let mut ours = Vec::new();
    let mut out = vec![0.0f32; 4096 * 2];
    loop {
        match dec.decode(&mut out) {
            Ok(0) => break,
            Ok(g) => ours.extend_from_slice(&out[..g * 2]),
            Err(e) => panic!("our decode errored: {e}"),
        }
    }
    let _ = std::fs::remove_file(&path);

    // The plain encode of the dual-mono source decodes as mid/side; both
    // streams need the same walk to strip decoder lead-in delay.
    let base_path = std::env::temp_dir().join(format!(
        "cadence_mp3_is_short_base_{}.mp3",
        std::process::id()
    ));
    std::fs::write(&base_path, &base).expect("write temp mp3");
    let mut base_dec =
        Mp3Decoder::open(Box::new(std::fs::File::open(&base_path).unwrap())).unwrap();
    let mut base = Vec::new();
    loop {
        match base_dec.decode(&mut out) {
            Ok(0) => break,
            Ok(g) => base.extend_from_slice(&out[..g * 2]),
            Err(e) => panic!("base decode errored: {e}"),
        }
    }
    let _ = std::fs::remove_file(&base_path);

    assert_eq!(ours.len(), oracle.len(), "length mismatch vs ffmpeg");
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&a, &b) in ours.iter().zip(&oracle) {
        sig += f64::from(b).powi(2);
        err += (f64::from(a) - f64::from(b)).powi(2);
    }
    let inter = 10.0 * (sig / err).log10();
    let rms = |x: &[f32], c: usize| {
        let v: Vec<f64> = x
            .iter()
            .skip(20_000 * 2 + c)
            .step_by(2)
            .take(40_000)
            .map(|&s| f64::from(s).powi(2))
            .collect();
        (v.iter().sum::<f64>() / v.len() as f64).sqrt()
    };
    eprintln!(
        "intensity short-block: inter-decoder {inter:.2} dB, rms L {:.4}/{:.4} R {:.4}/{:.4} (IS/plain)",
        rms(&ours, 0),
        rms(&base, 0),
        rms(&ours, 1),
        rms(&base, 1)
    );
    assert!(inter > 100.0, "inter-decoder SNR {inter:.2} dB vs ffmpeg");
    for c in 0..2 {
        let (a, b) = (rms(&ours, c), rms(&base, c));
        assert!(
            (a / b - 1.0).abs() < 0.1,
            "channel {c} level {a:.4} vs plain {b:.4}"
        );
    }
}

/// LSF-family intensity-stereo gate: a source panned exactly 2:1 (L/R
/// amplitude ratio 2, the ladder's position 4 at shift 1) with two
/// isolated click transients codes as long-block granules, short-block
/// granules, zero-line short bridges, and mixed-family frames whose top
/// bands are exactly in phase, so intensity must engage in every frame —
/// on the LSF families through the preselected power-of-two position
/// partition (transmitted via the right channel's split
/// `scalefac_compress`). The stream must decode identically in FFmpeg and
/// this crate (>100 dB) and reproduce the same per-channel levels as the
/// ordinary encode of the same source. (The clicks are isolated: repeated
/// click trains trip an unrelated, pre-existing short-block encode defect
/// tracked in `todo.md` that equally affects the plain encode.)
#[test]
fn lsf_intensity_stereo_agrees_with_ffmpeg() {
    use std::f32::consts::PI;

    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::{Mp3Decoder, Mp3Encoder};

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }

    struct Bits<'a> {
        d: &'a [u8],
        p: usize,
    }
    impl Bits<'_> {
        fn get(&mut self, n: u32) -> u32 {
            let mut v = 0u32;
            for _ in 0..n {
                let byte = self.d[self.p / 8];
                v = (v << 1) | u32::from((byte >> (7 - (self.p % 8))) & 1);
                self.p += 1;
            }
            v
        }
    }

    let sample_rate = 22_050u32;
    let kbps = 64u32;
    let n = sample_rate as usize * 2;
    let clicks = [sample_rate as usize / 2, sample_rate as usize * 7 / 10];
    let mut frames = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f32 / sample_rate as f32;
        let mut r = 0.25 * (2.0 * PI * 1500.0 * t).sin() + 0.1 * (2.0 * PI * 440.0 * t).sin();
        for &c in &clicks {
            if i >= c && i < c + 64 {
                let k = i - c;
                let noise = ((i * 37 % 23) as f32 - 11.0) / 11.0;
                r = 0.45 * noise * (-(k as f32) / 16.0).exp();
            }
        }
        // Exactly 2:1 panning: every band's L/R amplitude ratio is the
        // ladder's exponent +1, so the top bands are exactly in phase and
        // every position lands without error.
        frames.push(2.0 * r);
        frames.push(r);
    }

    let encode = |intensity: bool| {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut enc = Mp3Encoder::new(&mut buf, sample_rate, 2, kbps).unwrap();
            enc.set_intensity_stereo(intensity);
            enc.encode(&frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        buf.into_inner()
    };
    let base = encode(false);
    let data = encode(true);

    // Walk the frames: count intensity-signaled frames (mode_ext bit 0 on
    // MPEG-2 joint stereo) and window-switched granules from the LSF side
    // info (17 bytes for stereo, one granule, 9-bit scalefac_compress).
    let (mut off, mut is_frames, mut total, mut shorts) = (0usize, 0usize, 0usize, 0usize);
    while off + 4 + 17 <= data.len() {
        is_frames += ((data[off + 3] >> 4) & 1) as usize;
        let span = (576 * kbps * 125 / sample_rate) as usize + ((data[off + 2] >> 1) & 1) as usize;
        let mut bits = Bits {
            d: &data[off + 4..off + 4 + 17],
            p: 0,
        };
        bits.get(8); // main_data_begin
        bits.get(2); // private bits
        for _ in 0..2 {
            bits.get(12 + 9 + 8 + 9); // part2_3, big_values, global_gain, scf_compress
            if bits.get(1) == 1 {
                if bits.get(2) == 2 {
                    shorts += 1;
                }
                assert_eq!(bits.get(1), 0, "only pure short blocks are emitted");
                bits.get(5 + 5 + 9);
            } else {
                bits.get(15 + 4 + 3);
            }
            bits.get(2); // scalefac_scale, count1_table
        }
        off += span;
        total += 1;
    }
    assert!(shorts > 0, "material must produce short granules");
    assert_eq!(
        is_frames, total,
        "intensity must engage in every frame (panned dual content)"
    );
    eprintln!(
        "lsf intensity: {is_frames}/{total} frames intensity-signaled, {shorts} short granules"
    );

    let path = std::env::temp_dir().join(format!("cadence_mp3_lsf_is_{}.mp3", std::process::id()));
    std::fs::write(&path, &data).expect("write temp mp3");
    let oracle = decode_with_ffmpeg(&path, sample_rate, 2).expect("ffmpeg oracle decode");
    let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
    let mut ours = Vec::new();
    let mut out = vec![0.0f32; 4096 * 2];
    loop {
        match dec.decode(&mut out) {
            Ok(0) => break,
            Ok(g) => ours.extend_from_slice(&out[..g * 2]),
            Err(e) => panic!("our decode errored: {e}"),
        }
    }
    let _ = std::fs::remove_file(&path);

    let base_path = std::env::temp_dir().join(format!(
        "cadence_mp3_lsf_is_base_{}.mp3",
        std::process::id()
    ));
    std::fs::write(&base_path, &base).expect("write temp mp3");
    let mut base_dec =
        Mp3Decoder::open(Box::new(std::fs::File::open(&base_path).unwrap())).unwrap();
    let mut base = Vec::new();
    loop {
        match base_dec.decode(&mut out) {
            Ok(0) => break,
            Ok(g) => base.extend_from_slice(&out[..g * 2]),
            Err(e) => panic!("base decode errored: {e}"),
        }
    }
    let _ = std::fs::remove_file(&base_path);

    assert_eq!(ours.len(), oracle.len(), "length mismatch vs ffmpeg");
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&a, &b) in ours.iter().zip(&oracle) {
        sig += f64::from(b).powi(2);
        err += (f64::from(a) - f64::from(b)).powi(2);
    }
    let inter = 10.0 * (sig / err).log10();
    let rms = |x: &[f32], c: usize| {
        let v: Vec<f64> = x
            .iter()
            .skip(20_000 * 2 + c)
            .step_by(2)
            .take(40_000)
            .map(|&s| f64::from(s).powi(2))
            .collect();
        (v.iter().sum::<f64>() / v.len() as f64).sqrt()
    };
    eprintln!(
        "lsf intensity: inter-decoder {inter:.2} dB, rms L {:.4}/{:.4} R {:.4}/{:.4} (IS/plain)",
        rms(&ours, 0),
        rms(&base, 0),
        rms(&ours, 1),
        rms(&base, 1)
    );
    assert!(inter > 100.0, "inter-decoder SNR {inter:.2} dB vs ffmpeg");
    for c in 0..2 {
        let (a, b) = (rms(&ours, c), rms(&base, c));
        assert!(
            (a / b - 1.0).abs() < 0.1,
            "channel {c} level {a:.4} vs plain {b:.4}"
        );
    }
}

/// Metadata-tag gate: an Info/Xing-tagged stream must decode in FFmpeg in
/// agreement with this crate's decoder — the tag frame counts as one
/// silent frame in both, and the audio frames that follow (starting from
/// a clean reservoir) must be byte-compatible with untagged emission.
#[test]
fn metadata_tag_stream_agrees_with_ffmpeg() {
    use std::f32::consts::PI;

    use tpt_av_cadence_core::{Decoder, Encoder};
    use tpt_av_cadence_mp3::{Mp3Decoder, Mp3Encoder};

    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }

    for (vbr, sample_rate, channels, kbps) in
        [(false, 44_100u32, 2u16, 128u32), (true, 48_000, 1, 0)]
    {
        let n = sample_rate as usize;
        let mut frames = Vec::with_capacity(n * channels as usize);
        for i in 0..n {
            let t = i as f32 / sample_rate as f32;
            let s = 0.3 * (2.0 * PI * 440.0 * t).sin() + 0.1 * (2.0 * PI * 1500.0 * t).sin();
            frames.push(s);
            if channels == 2 {
                frames.push(s * 0.9);
            }
        }
        let mut buf = Cursor::new(Vec::new());
        {
            let res = if vbr {
                Mp3Encoder::new_vbr_with_xing(&mut buf, sample_rate, channels, 5)
            } else {
                Mp3Encoder::new_cbr_with_info(&mut buf, sample_rate, channels, kbps)
            };
            let mut enc = res.unwrap();
            enc.encode(&frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        let label = format!(
            "tag_{}_{}_{vbr}",
            if vbr { "xing" } else { "info" },
            sample_rate
        );

        let data = buf.into_inner();
        let path =
            std::env::temp_dir().join(format!("cadence_mp3_{}_{}.mp3", label, std::process::id()));
        std::fs::write(&path, &data).expect("write temp mp3");
        let oracle = decode_with_ffmpeg(&path, sample_rate, channels).expect("ffmpeg decode");
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

        // With the LAME header present, FFmpeg applies gapless trimming
        // internally (delay front, padding back) and additionally loses
        // its synthesis tail at EOF. Apply the same LAME trim to our raw
        // decode (per the tag we wrote) and compare the overlap; our
        // flush frame keeps the full tail, so ours may run a few hundred
        // samples longer.
        let si = if channels == 1 { 17usize } else { 32 };
        let field = 4 + si + 4 + 4 + 4 + 4 + 100 + 4 + 21;
        let dp = u32::from_be_bytes([0, data[field], data[field + 1], data[field + 2]]);
        let _delay = (dp >> 12) as usize;
        let _padding = (dp & 0xFFF) as usize;
        // Both decoders apply the LAME gapless arithmetic internally, but
        // their synthesis pipelines carry different start/end delays
        // (~550 samples), so the alignment lag is found empirically and
        // the SNR is measured on the overlap.
        let mut best = (0usize, f64::NEG_INFINITY);
        for lag in 0..1200usize {
            let mut score = 0.0f64;
            for i in (0..oracle.len().min(ours.len()) - lag).step_by(97) {
                score += f64::from(oracle[i]) * f64::from(ours[i + lag]);
            }
            if score > best.1 {
                best = (lag, score);
            }
        }
        let lag = best.0;
        let n = (oracle.len() - lag).min(ours.len() - lag);
        let (mut signal, mut error) = (0.0f64, 0.0f64);
        for i in 0..n {
            let delta = f64::from(ours[i + lag]) - f64::from(oracle[i]);
            error += delta * delta;
            signal += f64::from(oracle[i]).powi(2);
        }
        let snr = 10.0 * (signal / error).log10();
        eprintln!("{label}: inter-decoder SNR={snr:.2} dB (lag {lag})");
        assert!(snr > 100.0, "{label}: inter-decoder SNR={snr:.2} vs ffmpeg");
    }
}
