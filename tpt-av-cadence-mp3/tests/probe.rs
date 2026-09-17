// Scratch probe for initial bring-up; measures decode length and error
// against the bundled reference. Not part of the conformance suite.
use std::fs::File;
use std::path::Path;
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_mp3::Mp3Decoder;

fn decode_file(path: &Path) -> (usize, u32, usize, Vec<f32>) {
    let f = File::open(path).unwrap();
    let mut dec = Mp3Decoder::open(Box::new(f)).unwrap();
    let ch = dec.info().channels as usize;
    let sr = dec.info().sample_rate;
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 1152 * ch];
    loop {
        let n = dec.decode(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n * ch]);
    }
    (out.len() / ch.max(1), sr, ch, out)
}

#[test]
fn first_granule_matches_bundled_reference() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut decoder = Mp3Decoder::open(Box::new(
        File::open(base.join("mpeg1_44100_stereo_128k.mp3")).unwrap(),
    ))
    .unwrap();
    let mut pcm = [0.0; 576 * 2];
    assert_eq!(decoder.decode(&mut pcm).unwrap(), 576);
    let reference = std::fs::read(base.join("ref_128k.f32")).unwrap();
    for (i, (&actual, bytes)) in pcm.iter().zip(reference.chunks_exact(4)).enumerate() {
        let expected = f32::from_le_bytes(bytes.try_into().unwrap());
        assert!(
            (actual - expected).abs() <= 1e-5,
            "sample {i}: actual={actual:e}, expected={expected:e}"
        );
    }
}

#[test]
fn probe_128k() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let (frames, sr, ch, pcm) = decode_file(&base.join("mpeg1_44100_stereo_128k.mp3"));
    let reference = std::fs::read(base.join("ref_128k.f32")).unwrap();
    let ref_pcm: Vec<f32> = reference
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert_eq!(sr, 44100);
    assert_eq!(ch, 2);
    assert_eq!(frames, 133632);
    assert_eq!(
        pcm.len(),
        ref_pcm.len(),
        "decoded sample count differs from reference"
    );
    assert!(pcm.iter().all(|sample| sample.is_finite()));
    println!(
        "decoded frames={frames} sr={sr} ch={ch} ref_samples={}",
        ref_pcm.len() / 2
    );
    let n = pcm.len().min(ref_pcm.len());
    // per-granule SNR for the first 40 frames
    for fr in 0..40 {
        for gr in 0..2 {
            let lo = (fr * 2 + gr) * 576 * 2;
            let hi = lo + 576 * 2;
            if hi > n {
                break;
            }
            let mut ssq = 0.0f64;
            let mut rsq = 0.0f64;
            for i in lo..hi {
                let d = (pcm[i] - ref_pcm[i]) as f64;
                ssq += d * d;
                rsq += ref_pcm[i] as f64 * ref_pcm[i] as f64;
            }
            print!("f{fr}g{gr}:{:.1} ", 10.0 * (rsq / ssq.max(1e-20)).log10());
        }
    }
    println!();
    let mut max = 0.0f32;
    let mut sum_sq = 0.0;
    let mut ref_sq = 0.0;
    for i in 0..n {
        let d = (pcm[i] - ref_pcm[i]) as f64;
        max = max.max(d.abs() as f32);
        sum_sq += d * d;
        ref_sq += (ref_pcm[i] as f64) * (ref_pcm[i] as f64);
    }
    println!(
        "max_abs_diff={max} rms_diff={:.3e} ref_rms={:.3e} snr={:.1} dB",
        (sum_sq / n as f64).sqrt(),
        (ref_sq / n as f64).sqrt(),
        10.0 * (ref_sq / sum_sq).log10()
    );
    assert!(
        10.0 * (ref_sq / sum_sq).log10() > 100.0,
        "whole-file SNR collapsed: expected > 100 dB vs the bundled reference"
    );
    // First values for eyeballing.
    for i in 0..12 {
        print!("({:.4},{:.4}) ", pcm[i], ref_pcm[i]);
    }
    println!();
}
