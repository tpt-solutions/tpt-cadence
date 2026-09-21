//! Decodes a WAV file to raw interleaved f32 on stdout:
//! `cargo run -p tpt-av-cadence-wav --example wav_decode -- clip.wav > out.f32`

use std::fs::File;
use std::io::Write;

use tpt_av_cadence_core::FormatReader;
use tpt_av_cadence_wav::WavReader;

fn main() {
    let path = std::env::args().nth(1).expect("usage: decode <clip.wav>");
    let file = File::open(&path).expect("open input");
    let mut reader = WavReader::open(Box::new(file)).expect("open wav stream");
    let channels = reader.info().channels as usize;
    eprintln!(
        "wav: {} Hz, {} ch, {}-bit, total_frames {:?}",
        reader.info().sample_rate,
        channels,
        reader.info().bit_depth,
        reader.info().total_frames
    );
    let mut out = std::io::stdout();
    let mut buf = vec![0.0f32; 8192 * channels];
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
