//! Decode throughput benchmark against a bundled Ogg Vorbis fixture.

use std::hint::black_box;
use std::io::Cursor;
use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_vorbis::VorbisDecoder;

fn data_path(relative: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(relative)
}

fn decode_all(bytes: &[u8]) -> usize {
    let mut dec = VorbisDecoder::open(Box::new(Cursor::new(bytes.to_vec()))).expect("open");
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
    let fixture = data_path("stereo_44100_qm1.ogg");
    let bytes = std::fs::read(&fixture).expect("read fixture");

    let mut group = c.benchmark_group("vorbis_decode");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("stereo_44100_qm1", |b| {
        b.iter(|| black_box(decode_all(&bytes)))
    });
    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
