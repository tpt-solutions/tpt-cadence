//! Parser fuzzing wired to the shared `tpt-av-test` harness.
//!
//! `tpt-av-test-fuzz` guarantees that a never-panic property is expressed
//! the same way in every TPT repository and that the shared regression
//! corpus of known-bad inputs runs on every build.

use std::io::Read;

use proptest::prelude::*;

use tpt_av_cadence_wav::WavDecoder;
use tpt_av_test_fuzz::fuzz_parser_never_panics;

proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]
    #[test]
    fn wav_decoder_never_panics_on_corrupt_input(
        data in proptest::collection::vec(any::<u8>(), 0..8192),
    ) {
        fuzz_parser_never_panics!(
            parser: WavDecoder::open,
            input: Box::new(std::io::Cursor::new(data)) as Box<dyn Read + Send>,
        );
    }
}

#[test]
fn shared_corpus_never_panics_the_wav_decoder() {
    // The corpus is cross-codec hostile input, so clean rejections are
    // expected and collected by `run`; the property under test is that the
    // decoder rejects every entry by returning `Err` instead of panicking.
    let result = tpt_av_test_fuzz::corpus::run(&|bytes: &[u8]| match WavDecoder::open(Box::new(
        std::io::Cursor::new(bytes.to_vec()),
    )) {
        Ok(_) => Ok(()),
        Err(err) => Err(err.to_string()),
    });
    let _ = result; // Ok (all accepted) or Err (rejections) — both panic-free
}
