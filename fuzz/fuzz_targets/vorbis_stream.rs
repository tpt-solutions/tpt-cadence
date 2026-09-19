#![no_main]

use libfuzzer_sys::fuzz_target;
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_vorbis::VorbisDecoder;

fuzz_target!(|data: &[u8]| {
    if let Ok(mut decoder) =
        VorbisDecoder::from_source(Box::new(std::io::Cursor::new(data.to_vec())))
    {
        let channels = decoder.info().channels.max(1) as usize;
        let mut buf = vec![0.0f32; 8192 * channels];
        loop {
            match decoder.decode(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    }
});
