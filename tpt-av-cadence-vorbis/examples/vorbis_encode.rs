//! Writes a two-second stereo test signal (a 440 Hz / 660 Hz chord with a
//! slow tremolo) to an Ogg Vorbis file at the given quality (0-10, default 5):
//! `cargo run -p tpt-av-cadence-vorbis --example vorbis_encode -- tone.ogg [quality]`

use std::f32::consts::PI;
use std::fs::File;

use tpt_av_cadence_core::Encoder;
use tpt_av_cadence_vorbis::VorbisEncoder;

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: vorbis_encode <out.ogg> [quality 0-10]");
    let quality: f32 = args
        .next()
        .map(|q| q.parse().expect("quality must be a number"))
        .unwrap_or(5.0);

    let file = File::create(&path).expect("create output");
    let mut encoder =
        VorbisEncoder::new(file, SAMPLE_RATE, CHANNELS, quality).expect("open vorbis encoder");

    let frames = SAMPLE_RATE as usize * 2;
    let mut buf = vec![0.0f32; frames * CHANNELS as usize];
    for i in 0..frames {
        let t = i as f32 / SAMPLE_RATE as f32;
        let tremolo = 0.75 + 0.25 * (2.0 * PI * 3.0 * t).sin();
        buf[i * 2] = (2.0 * PI * 440.0 * t).sin() * 0.4 * tremolo;
        buf[i * 2 + 1] = (2.0 * PI * 660.0 * t).sin() * 0.4 * tremolo;
    }
    encoder.encode(&buf).expect("encode");
    Encoder::finish(&mut encoder).expect("finish");
    eprintln!("wrote {frames} frames to {path} at quality {quality}");
}
