#![no_main]

use libfuzzer_sys::fuzz_target;
use tpt_av_cadence_core::FormatReader;
use tpt_av_cadence_opus::OggOpusReader;

fuzz_target!(|data: &[u8]| {
    let Ok(mut reader) =
        OggOpusReader::from_source(Box::new(std::io::Cursor::new(data.to_vec())))
    else {
        return;
    };
    let dec = reader.decoder();
    let ch = dec.info().channels.max(1) as usize;
    let mut buf = vec![0.0f32; 4096 * ch];
    loop {
        match dec.decode(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }

    // Seek(0) + replay must reproduce the same PCM (linear seek-and-discard).
    let mut first: Option<Vec<u8>> = None;
    let dec = reader.decoder();
    if dec.seek(0).is_err() {
        return;
    }
    loop {
        match dec.decode(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let snapshot: Vec<u8> =
                    buf[..n * ch].iter().flat_map(|f| f.to_le_bytes()).collect();
                if let Some(prev) = &first {
                    assert_eq!(
                        &snapshot,
                        prev,
                        "seek(0) replay diverged from the first pass"
                    );
                } else {
                    first = Some(snapshot);
                }
            }
        }
    }
});
