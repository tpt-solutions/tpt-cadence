//! Cross-checks the FLAC decoder against FFmpeg on the bundled
//! conformance fixtures. FLAC is lossless, so both decoders must produce
//! bit-identical PCM (tolerance 0).
//!
//! FFmpeg is a subprocess only, found on PATH. Set CADENCE_REQUIRE_FFMPEG=1
//! to fail instead of skipping when the executable is unavailable.

use std::path::Path;

use tpt_av_cadence_flac::FlacDecoder;
use tpt_av_cadence_test_utils::reference::{assert_bit_exact_vs_ffmpeg, ConformanceError};

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

    assert!(checked > 0, "no FLAC fixtures were cross-checked");
    if skipped > 0 {
        assert!(
            std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_none(),
            "FFmpeg is required but unavailable on PATH"
        );
        eprintln!("skipped {skipped} fixtures: FFmpeg not on PATH");
    }
}
