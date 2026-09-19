#![no_main]

use libfuzzer_sys::fuzz_target;
use tpt_av_cadence_core::BufferedSource;
use tpt_av_cadence_ogg::PageReader;

fuzz_target!(|data: &[u8]| {
    let source = Box::new(std::io::Cursor::new(data.to_vec()));
    let mut reader = PageReader::new(BufferedSource::new(source, 1024), 1 << 16);
    let mut out = vec![0u8; 1 << 16];
    loop {
        match reader.next_packet(&mut out) {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return,
        }
    }
});
