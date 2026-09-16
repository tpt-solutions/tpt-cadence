use std::fs::File;
use std::io::Read;

fn valid(h: &[u8]) -> bool {
    h.len() >= 4
        && h[0] == 0xFF
        && ((h[1] & 0xF0) == 0xF0 || (h[1] & 0xFE) == 0xE2)
        && ((h[1] >> 1) & 3) == 1
        && (h[2] >> 4) != 0
        && (h[2] >> 4) != 15
        && ((h[2] >> 2) & 3) != 3
}

#[test]
fn dbg_find() {
    let mut f = File::open(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/mpeg1_44100_stereo_128k.mp3"
    ))
    .unwrap();
    let mut d = Vec::new();
    f.read_to_end(&mut d).unwrap();
    let d = &d[44..];
    println!("valid at 0: {}", valid(&d[..]));
    for i in 0..900 {
        if d[i] == 0xFF && valid(&d[i..]) {
            println!("sync at {i}: {:02x?}", &d[i..i + 4]);
        }
    }
}
