//! Cross-checks the FLAC decoder against FFmpeg on the bundled
//! conformance fixtures. FLAC is lossless, so both decoders must produce
//! bit-identical PCM (tolerance 0).
//!
//! FFmpeg is a subprocess only, found on PATH. Set CADENCE_REQUIRE_FFMPEG=1
//! to fail instead of skipping when the executable is unavailable.

use std::path::Path;

use tpt_av_cadence_core::{f32_to_int, int_to_f32, Encoder};
use tpt_av_cadence_flac::{FlacDecoder, FlacEncoder};
use tpt_av_cadence_test_utils::reference::{
    assert_bit_exact_vs_ffmpeg, decode_with_ffmpeg, ffmpeg_available, ConformanceError,
};

fn crosscheck(path: &Path) -> Result<(), ConformanceError> {
    let file = std::fs::File::open(path)?;
    let mut decoder = FlacDecoder::open(Box::new(file))?;
    assert_bit_exact_vs_ffmpeg(path, &mut decoder, 0.0)
}

#[test]
fn flac_matches_ffmpeg_bit_exact() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut checked = 0usize;
    let mut skipped = 0usize;

    for sub in ["subset", "uncommon"] {
        let dir = data.join(sub);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("flac") {
                continue;
            }
            match crosscheck(&path) {
                Ok(()) => checked += 1,
                Err(ConformanceError::ReferenceUnavailable) => skipped += 1,
                Err(e) => panic!("{}: {e}", path.display()),
            }
        }
    }

    assert!(checked + skipped > 0, "no FLAC fixtures found to check");
    if skipped > 0 {
        assert!(
            std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_none(),
            "FFmpeg is required but unavailable on PATH"
        );
        eprintln!("skipped {skipped} fixtures: FFmpeg not on PATH");
    }
}

/// Cross-checks the FLAC *encoder*: FFmpeg must be able to decode this
/// crate's own encoder output, and the result must be bit-exact against the
/// (bit-depth-quantized) source PCM used to build it — a stronger signal
/// than round-tripping through this crate's own decoder alone, since it
/// rules out a mutual bug shared by the encoder and decoder.
#[test]
fn flac_encoder_output_decodes_bit_exact_in_ffmpeg() {
    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }

    let sample_rate = 44_100u32;
    let channels = 2u16;
    let bits_per_sample = 16u16;
    let frames = sample_rate as usize; // 1 second

    // A 440 Hz tone in one channel and white noise in the other: exercises
    // both the FIXED-predictor-friendly and residual-heavy encode paths in
    // one file.
    let mut state: u32 = 0xC0FF_EE01;
    let mut next_noise = || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        (state as f32 / u32::MAX as f32) * 2.0 - 1.0
    };
    let mut source = vec![0.0f32; frames * channels as usize];
    for i in 0..frames {
        let t = i as f32 / sample_rate as f32;
        source[i * 2] = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.6;
        source[i * 2 + 1] = next_noise() * 0.6;
    }
    let expected: Vec<f32> = source
        .iter()
        .map(|&s| int_to_f32(f32_to_int(s, bits_per_sample), bits_per_sample))
        .collect();

    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "cadence_flac_encoder_crosscheck_{}.flac",
        std::process::id()
    ));
    {
        let file = std::fs::File::create(&path).expect("create temp flac");
        let mut encoder =
            FlacEncoder::new(file, sample_rate, channels, bits_per_sample).expect("open encoder");
        encoder.encode(&source).expect("encode");
        Encoder::finish(&mut encoder).expect("finish");
    }

    let result = decode_with_ffmpeg(&path, sample_rate, channels);
    let _ = std::fs::remove_file(&path);
    let got = result.expect("ffmpeg failed to decode our encoder output");

    assert_eq!(
        got.len(),
        expected.len(),
        "ffmpeg decoded a different sample count than expected"
    );
    for (index, (a, b)) in got.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            a, b,
            "sample {index} mismatch (tpt-encoded, ffmpeg-decoded)"
        );
    }
}
