//! Bounded no-panic checks, not a proof of safety for all malformed streams.
use std::io::Cursor;

use proptest::prelude::*;
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_mp3::Mp3Decoder;

fn exercise(bytes: Vec<u8>) {
    if let Ok(mut decoder) = Mp3Decoder::from_source(Box::new(Cursor::new(bytes))) {
        let mut pcm = [0.0; 2304];
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
        lsf in any::<bool>(),
        changes in prop::collection::vec((0usize..2048, any::<u8>()), 1..16),
        length in 4usize..2049,
    ) {
        let source: &[u8] = if lsf {
            include_bytes!("data/mpeg2_16000_mono_b24k.mp3")
        } else {
            include_bytes!("data/mpeg1_44100_stereo_128k.mp3")
        };
        // Preserve the first two headers so mutations reach decoding, not
        // only the initial sync probe. Both fixtures have a 44-byte ID3 tag.
        let frame_len = if lsf { 108 } else { 417 };
        let mut bytes = source[44..44 + length].to_vec();
        for (offset, value) in changes {
            let offset = offset % bytes.len();
            if offset >= 4 && !(frame_len..frame_len + 4).contains(&offset) {
                bytes[offset] ^= value;
            }
        }
        exercise(bytes);
    }
}
