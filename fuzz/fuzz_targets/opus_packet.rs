#![no_main]

use libfuzzer_sys::fuzz_target;
use tpt_av_cadence_opus::packet::parse_packet;

fuzz_target!(|data: &[u8]| {
    let _ = parse_packet(data);
});
