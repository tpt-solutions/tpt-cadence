//! Decode throughput benchmark. WAV is an uncompressed container, so the
//! fixture is built in-memory with the crate's own encoder (a real WAV
//! byte stream, not a hand-rolled approximation of one).

use std::hint::black_box;
use std::io::Cursor;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tpt_av_cadence_core::{Decoder, Encoder, SampleFormat};
use tpt_av_cadence_wav::{WavDecoder, WavEncoder};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const SECONDS: usize = 5;

fn build_fixture() -> Vec<u8> {
    // `WavEncoder` doesn't expose its sink back out, so round-trip through a
    // scratch file rather than an in-memory `Cursor` (a real WAV byte stream
    // either way, just sourced from disk instead of RAM).
    let path = std::env::temp_dir().join(format!("cadence_wav_bench_{}.wav", std::process::id()));
    {
        let file = std::fs::File::create(&path).expect("create scratch file");
        let mut enc = WavEncoder::new(file, SAMPLE_RATE, CHANNELS, SampleFormat::Int16)
            .expect("open encoder");

        let frames = SAMPLE_RATE as usize * SECONDS;
        let mut samples = vec![0.0f32; frames * CHANNELS as usize];
        for (i, chunk) in samples.chunks_mut(CHANNELS as usize).enumerate() {
            let t = i as f32 / SAMPLE_RATE as f32;
            let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
            for c in chunk {
                *c = s;
            }
        }
        enc.encode(&samples).expect("encode");
        enc.finish().expect("finish");
    }
    let bytes = std::fs::read(&path).expect("read scratch file");
    let _ = std::fs::remove_file(&path);
    bytes
}

fn decode_all(bytes: &[u8]) -> usize {
    let mut dec = WavDecoder::from_source(Box::new(Cursor::new(bytes.to_vec()))).expect("open");
    let mut buf = vec![0.0f32; 4096];
    let mut total = 0usize;
    loop {
        let n = dec.decode(&mut buf).expect("decode");
        if n == 0 {
            break;
        }
        total += n;
    }
    total
}

fn bench_decode(c: &mut Criterion) {
    let bytes = build_fixture();

    let mut group = c.benchmark_group("wav_decode");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("48k_stereo_i16_5s", |b| {
        b.iter(|| black_box(decode_all(&bytes)))
    });
    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
