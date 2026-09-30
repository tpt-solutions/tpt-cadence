//! Decodes an Ogg Opus file to raw interleaved f32 (48 kHz) on stdout:
//! `cargo run -p tpt-av-cadence-opus --example opus_decode -- song.opus > out.f32`

use std::io::Write;

use tpt_av_cadence_core::FormatReader;
use tpt_av_cadence_opus::OggOpusReader;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: opus_decode <file.opus>");
    let file = std::fs::File::open(&path).expect("open input");
    let mut reader = OggOpusReader::open(Box::new(file)).expect("open opus stream");
    let info = reader.info();
    let channels = info.channels as usize;
    eprintln!(
        "opus: {} Hz, {} ch, total_frames {:?}",
        info.sample_rate, channels, info.total_frames
    );
    let mut out = std::io::stdout();
    let mut buf = vec![0.0f32; 5760 * channels];
    loop {
        let frames = reader.decoder().decode(&mut buf).expect("decode");
        if frames == 0 {
            break;
        }
        let bytes: Vec<u8> = buf[..frames * channels]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        out.write_all(&bytes).expect("write");
    }
}
