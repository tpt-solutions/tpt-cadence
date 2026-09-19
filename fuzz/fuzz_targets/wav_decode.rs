#![no_main]

use libfuzzer_sys::fuzz_target;
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_wav::WavDecoder;

fuzz_target!(|data: &[u8]| {
    let Ok(mut dec) = WavDecoder::open(Box::new(std::io::Cursor::new(data.to_vec()))) else {
        return;
    };
    let ch = dec.info().channels.max(1) as usize;
    let mut buf = vec![0.0f32; 4096 * ch];
    loop {
        match dec.decode(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
});
