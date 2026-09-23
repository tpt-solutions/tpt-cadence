//! Decode throughput benchmark, run against the bundled conformance
//! fixtures so results reflect real encoded streams, not synthetic data.

use std::hint::black_box;
use std::io::Cursor;
use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_flac::FlacDecoder;

fn data_path(relative: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(relative)
}

fn decode_all(bytes: &[u8]) -> usize {
    let mut dec = FlacDecoder::open(Box::new(Cursor::new(bytes.to_vec()))).expect("open");
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
    let fixture = data_path("subset/01-blocksize-4096.flac");
    let bytes = std::fs::read(&fixture).expect("read fixture");

    let mut group = c.benchmark_group("flac_decode");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("01-blocksize-4096", |b| {
        b.iter(|| black_box(decode_all(&bytes)))
    });
    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
