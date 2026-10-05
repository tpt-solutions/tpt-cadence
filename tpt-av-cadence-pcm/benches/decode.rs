//! Decode throughput benchmark for headerless PCM: interleaved s16 little
//! endian, the most common raw dump layout. The fixture is the same
//! harmonic signal the other container benches use, serialized to bytes.

use std::hint::black_box;
use std::io::Cursor;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tpt_av_cadence_core::{Decoder, SampleFormat};
use tpt_av_cadence_pcm::{PcmDecoder, PcmFormat};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const SECONDS: usize = 5;

fn build_fixture() -> Vec<u8> {
    let frames = SAMPLE_RATE as usize * SECONDS;
    let mut samples = vec![0.0f32; frames * CHANNELS as usize];
    for (i, chunk) in samples.chunks_mut(CHANNELS as usize).enumerate() {
        let t = i as f32 / SAMPLE_RATE as f32;
        let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
        for c in chunk {
            *c = s;
        }
    }
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let q = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        bytes.extend_from_slice(&q.to_le_bytes());
    }
    bytes
}

fn decode_all(bytes: &[u8], format: PcmFormat) -> usize {
    let mut dec =
        PcmDecoder::from_source(Box::new(Cursor::new(bytes.to_vec())), format).expect("open");
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
    let format = PcmFormat {
        sample_format: SampleFormat::Int16,
        byte_order: tpt_av_cadence_pcm::ByteOrder::Little,
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE,
    };

    let mut group = c.benchmark_group("pcm_decode");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("48k_stereo_s16le_5s", |b| {
        b.iter(|| black_box(decode_all(&bytes, format)))
    });
    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
