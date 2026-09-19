//! Optional conformance run against the ISO/IEC 11172-3 conformance
//! bitstreams that mpg123's `mp3conscheck` uses.
//!
//! The streams are not redistributable from this repository; they live in
//! the mpg123 SVN `test/` directory. To run this test:
//!
//! 1. `svn export svn://scm.orgis.org/mpg123/test mp3_cons` (or fetch the
//!    `test/` directory from any mpg123 mirror) and keep the Layer III
//!    compliance streams (`compl*.mp3` / `fl*.mp3` and companions).
//! 2. `MP3_CONS_DIR=<path to that directory> cargo test -p tpt-av-cadence-mp3
//!    --release -- --ignored --nocapture`
//!
//! For every `*.mp3` in the directory we decode the stream and compare it
//! against the in-repo FFmpeg 7.1 decode with the suite's external gate
//! (>100 dB SNR, <=1e-5 peak error). The ISO reference PCM files shipped
//! upstream come from the 1990s ISO reference decoder and are NOT
//! bit-exact against modern decoders, so FFmpeg is used as the oracle —
//! the same policy as the bundled ten-stream suite. Streams FFmpeg itself
//! cannot decode (Layer I/II) are reported and skipped.
//!
//! Ignored by default (needs MP3_CONS_DIR).

use std::path::PathBuf;

use tpt_av_cadence_mp3::Mp3Decoder;
use tpt_av_cadence_test_utils::reference::{assert_bit_exact_vs_ffmpeg, ConformanceError};

#[test]
#[ignore = "needs MP3_CONS_DIR pointing at the mpg123 ISO conformance streams"]
fn iso_conformance_streams_vs_ffmpeg() {
    let dir = std::env::var_os("MP3_CONS_DIR")
        .map(PathBuf::from)
        .expect("MP3_CONS_DIR is not set; see this test's module documentation");
    let entries = std::fs::read_dir(&dir).expect("reading MP3_CONS_DIR");

    let mut checked = 0usize;
    let mut unsupported = 0usize;
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("mp3") {
            continue;
        }
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => panic!("{}: {e}", path.display()),
        };
        let mut decoder = match Mp3Decoder::open(Box::new(file)) {
            Ok(d) => d,
            Err(_) => {
                unsupported += 1;
                eprintln!(
                    "{}: not decodable (Layer I/II or unrecognized)",
                    path.display()
                );
                continue;
            }
        };
        match assert_bit_exact_vs_ffmpeg(&path, &mut decoder, 1e-5) {
            Ok(()) => checked += 1,
            Err(ConformanceError::ReferenceUnavailable) => {
                assert!(
                    std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_none(),
                    "FFmpeg is required but unavailable on PATH"
                );
                eprintln!("skipping: FFmpeg not on PATH");
                return;
            }
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }

    assert!(checked > 0, "no conformance streams were checked");
    if unsupported > 0 {
        eprintln!("{unsupported} streams unsupported (non-Layer III) and skipped");
    }
}
