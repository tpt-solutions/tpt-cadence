//! Structural, seek-replay, and independent FFmpeg PCM coverage for ten streams.
//! Float PCM uses >100 dB SNR and <=1e-5 peak error, not bit-exact equality.
//! FFmpeg is a subprocess only, found on PATH. Set CADENCE_REQUIRE_FFMPEG=1
//! to fail instead of skipping when the executable is unavailable.
//!
//! For every stream we assert stream geometry, a complete decode (exact
//! expected sample count), finite output, and byte-for-byte deterministic
//! replay: decoding again after a seek to frame 0 must reproduce the full
//! stream's PCM exactly, which exercises seek, bit-reservoir reset, and
//! codec-state reset together.

use std::fs::File;
use std::path::{Path, PathBuf};
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_mp3::Mp3Decoder;

struct StreamCase {
    file: &'static str,
    sample_rate: u32,
    channels: u16,
    /// Complete frame-header count times 1152 (MPEG-1) or 576 (LSF),
    /// independently counted from the fixtures; includes encoder padding.
    samples_per_channel: usize,
}

const CASES: &[StreamCase] = &[
    StreamCase {
        file: "mpeg1_44100_mono_96k.mp3",
        sample_rate: 44100,
        channels: 1,
        samples_per_channel: 133632,
    },
    StreamCase {
        file: "mpeg1_44100_joint_160k.mp3",
        sample_rate: 44100,
        channels: 2,
        samples_per_channel: 133632,
    },
    StreamCase {
        file: "mpeg1_44100_stereo_128k.mp3",
        sample_rate: 44100,
        channels: 2,
        samples_per_channel: 133632,
    },
    StreamCase {
        file: "mpeg1_44100_stereo_192k_nosecond.mp3",
        sample_rate: 44100,
        channels: 2,
        samples_per_channel: 133632,
    },
    StreamCase {
        file: "mpeg1_48000_stereo_192k.mp3",
        sample_rate: 48000,
        channels: 2,
        samples_per_channel: 145152,
    },
    StreamCase {
        file: "mpeg1_32000_mono_b96k.mp3",
        sample_rate: 32000,
        channels: 1,
        samples_per_channel: 97920,
    },
    StreamCase {
        file: "mpeg2_16000_mono_b24k.mp3",
        sample_rate: 16000,
        channels: 1,
        samples_per_channel: 49536,
    },
    StreamCase {
        file: "mpeg2_22050_stereo_96k.mp3",
        sample_rate: 22050,
        channels: 2,
        samples_per_channel: 67392,
    },
    StreamCase {
        file: "mpeg2_24000_stereo_64k.mp3",
        sample_rate: 24000,
        channels: 2,
        samples_per_channel: 73152,
    },
    StreamCase {
        file: "mpeg25_8000_mono_16k.mp3",
        sample_rate: 8000,
        channels: 1,
        samples_per_channel: 25344,
    },
];

fn decode_all(path: &Path) -> (Mp3Decoder, Vec<f32>) {
    let mut dec = Mp3Decoder::from_source(Box::new(File::open(path).unwrap())).unwrap();
    let ch = dec.info().channels as usize;
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 1152 * ch];
    loop {
        let n = dec.decode(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n * ch]);
    }
    (dec, out)
}

#[test]
fn all_streams_decode_deterministically() {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for case in CASES {
        let path = base.join(case.file);
        let (mut dec, pcm) = decode_all(&path);
        let info = dec.info();
        assert_eq!(
            info.sample_rate, case.sample_rate,
            "{}: sample rate",
            case.file
        );
        assert_eq!(info.channels, case.channels, "{}: channel count", case.file);
        assert_eq!(
            pcm.len(),
            case.samples_per_channel * case.channels as usize,
            "{}: sample count",
            case.file
        );
        assert!(
            pcm.iter().all(|s| s.is_finite()),
            "{}: non-finite output",
            case.file
        );

        // Seek back to frame 0 and replay: must reproduce every sample.
        dec.seek(0).unwrap();
        let ch = case.channels as usize;
        let mut buf = vec![0.0f32; 1152 * ch];
        let mut replayed = 0usize;
        while replayed < pcm.len() {
            let want = ((pcm.len() - replayed) / ch).min(1152) * ch;
            let n = dec.decode(&mut buf[..want]).unwrap();
            assert!(n > 0, "{}: replay stalled at {}", case.file, replayed);
            assert_eq!(
                &pcm[replayed..replayed + n * ch],
                &buf[..n * ch],
                "{}: replay mismatch at {}",
                case.file,
                replayed
            );
            replayed += n * ch;
        }
    }
}

#[test]
fn mid_stream_seek_reproduces_reference_decode() {
    // Decode half, seek to that exact frame boundary, decode the rest, and
    // require the concatenation to equal the full linear decode. Only run on
    // the 128 kbps stream where a reference comparison exists.
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let (mut dec, pcm) = decode_all(&base.join("mpeg1_44100_stereo_128k.mp3"));
    let ch = dec.info().channels as usize;
    let half_frames = (pcm.len() / ch / 2) as u64;
    dec.seek(half_frames).unwrap();
    let mut buf = vec![0.0f32; 1152 * ch];
    let mut rest = Vec::new();
    loop {
        let n = dec.decode(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        rest.extend_from_slice(&buf[..n * ch]);
    }
    assert_eq!(rest.len(), pcm.len() / 2);
    assert_eq!(&pcm[pcm.len() / 2..], &rest[..]);
}

/// External oracle: no trimming, alignment search, gain correction, or
/// resampling compensation. These fixtures carry no gapless trimming tags.
#[test]
fn all_streams_match_ffmpeg() {
    use tpt_av_cadence_test_utils::reference::{decode_with_ffmpeg, ffmpeg_available};

    if !ffmpeg_available() {
        assert!(
            std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_none(),
            "FFmpeg is required but unavailable on PATH"
        );
        eprintln!("skipping external MP3 comparison: FFmpeg not on PATH");
        return;
    }
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut failures = Vec::new();
    for case in CASES {
        let path = base.join(case.file);
        let (_, pcm) = decode_all(&path);
        let reference = decode_with_ffmpeg(&path, case.sample_rate, case.channels).unwrap();
        assert_eq!(
            pcm.len(),
            reference.len(),
            "{}: reference length",
            case.file
        );
        assert!(!reference.is_empty());
        assert!(pcm.iter().chain(&reference).all(|x| x.is_finite()));
        let mut signal = 0.0f64;
        let mut error = 0.0f64;
        let mut peak = 0.0f64;
        for (&actual, &expected) in pcm.iter().zip(&reference) {
            let delta = f64::from(actual) - f64::from(expected);
            error += delta * delta;
            signal += f64::from(expected).powi(2);
            peak = peak.max(delta.abs());
        }
        assert!(signal > 0.0, "{}: silent reference", case.file);
        let snr = 10.0 * (signal / error).log10();
        eprintln!("{}: SNR={snr:.2} dB, max error={peak:.3e}", case.file);
        // Float implementations need not agree bit-for-bit. Require both
        // whole-stream fidelity and a bound on isolated sample errors.
        if snr <= 100.0 || peak > 1e-5 {
            failures.push(format!("{}: SNR={snr:.2}, peak={peak:.3e}", case.file));
        }
    }
    assert!(failures.is_empty(), "FFmpeg mismatches: {failures:#?}");
}
