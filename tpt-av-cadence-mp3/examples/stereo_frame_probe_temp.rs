use std::fs::File;
use tpt_av_cadence_core::Encoder;
use tpt_av_cadence_mp3::Mp3Encoder;

fn main() {
    let sample_rate = 44_100u32;
    let n = sample_rate as usize;
    let mut frames = Vec::with_capacity(n * 2);
    for i in 0..n {
        let sample = 0.6 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate as f32).sin();
        frames.extend_from_slice(&[sample, sample]);
    }
    let file = File::create("target/stereo_probe.mp3").unwrap();
    let mut encoder = Mp3Encoder::new(file, sample_rate, 2, 128).unwrap();
    encoder.encode(&frames).unwrap();
    encoder.finish().unwrap();
}
