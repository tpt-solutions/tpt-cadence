//! Never-panic property tests for the Opus packet parser and range coder.

use proptest::prelude::*;
use tpt_av_cadence_opus::packet::parse_packet;
use tpt_av_cadence_opus::RangeDecoder;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_packets_never_panic(data in proptest::collection::vec(any::<u8>(), 0..1300)) {
        if let Ok(packet) = parse_packet(&data) {
            // Frame ranges must be in-bounds regardless of input.
            for i in 0..packet.frame_count() {
                if let Some((start, end)) = packet.frame_range(i) {
                    prop_assert!(end <= data.len() && start <= end);
                }
            }
        }
        // The range decoder must tolerate arbitrary (even empty) frames.
        let mut dec = RangeDecoder::new(&data);
        for _ in 0..32 {
            let _ = dec.decode_bit_logp(3);
            let _ = dec.decode_uint(300);
        }
    }
}
