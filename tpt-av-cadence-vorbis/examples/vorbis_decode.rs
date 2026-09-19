//! Decodes an Ogg Vorbis file to raw interleaved f32 on stdout:
//! `cargo run -p tpt-av-cadence-vorbis --example vorbis_decode -- file.ogg > out.f32`

use std::io::{Read, Write};

use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_vorbis::VorbisDecoder;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: vorbis_decode <file.ogg>");
    let file = std::fs::File::open(&path).expect("open input");
    let mut decoder = VorbisDecoder::open(Box::new(file)).expect("open vorbis stream");
    let channels = decoder.info().channels;
    let total = decoder.info().total_frames;
    eprintln!(
        "vorbis: {} Hz, {} ch, total_frames {:?}",
        decoder.info().sample_rate,
        channels,
        total
    );
    let mut out = std::io::stdout();
    let mut buf = vec![0.0f32; 8192 * channels as usize];
    loop {
        let frames = decoder.decode(&mut buf).expect("decode");
        if frames == 0 {
            break;
        }
        let bytes: Vec<u8> = buf[..frames * channels as usize]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        out.write_all(&bytes).expect("write");
    }
    let _ = std::io::stdin().read(&mut [0u8]).ok(); // keep stdout alive
}
