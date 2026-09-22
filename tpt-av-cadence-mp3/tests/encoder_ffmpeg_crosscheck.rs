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
