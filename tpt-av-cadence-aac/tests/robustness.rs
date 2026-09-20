//! Bounded no-panic checks over arbitrary bytes and mutated/truncated
//! fixtures. No-panic results are not a proof of safety for all inputs.
use std::io::Cursor;

use proptest::prelude::*;
use tpt_av_cadence_aac::AacDecoder;
use tpt_av_cadence_core::Decoder;

fn exercise(bytes: Vec<u8>) {
    if let Ok(mut decoder) = AacDecoder::from_source(Box::new(Cursor::new(bytes))) {
        let mut pcm = vec![0.0f32; 2048 * decoder.info().channels as usize];
        for _ in 0..32 {
            match decoder.decode(&mut pcm) {
                Ok(0) | Err(_) => break,
                Ok(n) => assert!(n * decoder.info().channels as usize <= pcm.len()),
            }
        }
        let _ = decoder.seek(0);
        let _ = decoder.decode(&mut pcm);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn arbitrary_streams_do_not_panic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        exercise(bytes);
    }

    #[test]
    fn mutated_frames_do_not_panic(
        stereo in any::<bool>(),
        changes in prop::collection::vec((0usize..2048, any::<u8>()), 1..16),
        length in 8usize..2049,
    ) {
        let source: &[u8] = if stereo {
            include_bytes!("data/test.aac")
        } else {
            include_bytes!("data/tone.aac")
        };
        // Keep the first ADTS header intact so mutations reach the block
        // decoder rather than only the sync scan; skip the header's own
        // frame-length field so most frames still frame up.
        let mut bytes = source[..source.len().min(length)].to_vec();
        for (offset, value) in changes {
            let offset = 7 + (offset % bytes.len().saturating_sub(7).max(1));
            bytes[offset] ^= value;
        }
        exercise(bytes);
    }
}
