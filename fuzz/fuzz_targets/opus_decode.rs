#![no_main]

use libfuzzer_sys::fuzz_target;
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_opus::{packet::parse_packet, OpusDecoder};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let Ok(mut decoder) = OpusDecoder::new(2) else {
        return;
    };
    let Ok(packet) = parse_packet(data) else {
        return;
    };
    let mut pcm = vec![0.0f32; packet.frame_count() * 5760 * 2];
    let _ = decoder.decode_packet(&packet, data, &mut pcm);
});
