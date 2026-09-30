//! Regenerates the binary fixtures in `fixtures/` that `test.js` (the
//! Node.js wasm smoke test) decodes. The fixtures are committed, so this
//! only needs to run when the signal or an encoder changes — the encoders
//! are deterministic, so regeneration reproduces the same bytes:
//!
//! ```text
//! cargo test -p tpt-av-cadence-wasm-demo --test gen_fixtures -- --ignored
//! ```
//!
//! Every generated file is produced by this workspace's own encoders: the
//! smoke test deliberately decodes our encoders' output through our wasm
//! decoders. The AAC fixture is not generated (the AAC encoder is out of
//! scope for licensing reasons); `test.js` reads the AAC crate's bundled
//! `tone.aac` directly instead.

use std::f32::consts::PI;
use std::io::Cursor;

use tpt_av_cadence_core::{Encoder, FormatReader};
use tpt_av_cadence_flac::{FlacEncoder, FlacReader};
use tpt_av_cadence_mp3::Mp3Encoder;
use tpt_av_cadence_opus::OggOpusEncoder;
use tpt_av_cadence_vorbis::VorbisEncoder;

/// 0.4 s of 440 Hz sine at amplitude 0.4, mono — loud enough for a
/// well-above-noise RMS check, short enough to keep the fixtures small.
fn tone(sample_rate: u32, samples: usize) -> Vec<f32> {
    (0..samples)
        .map(|i| 0.4 * (2.0 * PI * 440.0 * i as f32 / sample_rate as f32).sin())
        .collect()
}

fn encode_to_vec(
    encode: impl FnOnce(&mut Cursor<Vec<u8>>) -> Result<(), tpt_av_cadence_core::CadenceError>,
) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    encode(&mut buf).expect("fixture encode");
    buf.into_inner()
}

#[test]
#[ignore = "regenerates the committed fixtures/ binaries; run explicitly"]
fn generate_fixtures() {
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    std::fs::create_dir_all(&out).unwrap();

    // FLAC: lossless, so test.js asserts exact sample values.
    let (sr, n) = (44_100u32, 17_640usize);
    let src = tone(sr, n);
    let flac = encode_to_vec(|sink| {
        let mut enc = FlacEncoder::new(sink, sr, 1, 16)?;
        enc.encode(&src)?;
        Encoder::finish(&mut enc)
    });
    std::fs::write(out.join("tone.flac"), &flac).unwrap();

    // MP3: MPEG-2.5 at the smallest standard rate (8 kHz mono, 8 kbps).
    let (sr8, n8) = (8_000u32, 3_200usize);
    let src8 = tone(sr8, n8);
    let mp3 = encode_to_vec(|sink| {
        let mut enc = Mp3Encoder::new(sink, sr8, 1, 8)?;
        enc.encode(&src8)?;
        Encoder::finish(&mut enc)
    });
    std::fs::write(out.join("tone.mp3"), &mp3).unwrap();

    // Ogg Opus: fullband mono at a low CBR.
    let (sr48, n48) = (48_000u32, 19_200usize);
    let src48 = tone(sr48, n48);
    let opus = encode_to_vec(|sink| {
        let mut enc = OggOpusEncoder::new(sink, sr48, 1, 16_000)?;
        enc.encode(&src48)?;
        Encoder::finish(&mut enc)
    });
    std::fs::write(out.join("tone.opus"), &opus).unwrap();

    // Ogg Vorbis: mono at a mid quality.
    let vorbis = encode_to_vec(|sink| {
        let mut enc = VorbisEncoder::new(sink, sr, 1, 3.0)?;
        enc.encode(&src)?;
        Encoder::finish(&mut enc)
    });
    std::fs::write(out.join("tone.vorbis"), &vorbis).unwrap();

    // Print what our decoders report for every fixture (including the
    // AAC crate's bundled file that test.js reads in place) so the
    // expectations in test.js can be pinned against reality.
    for (name, bytes, rate) in [
        ("tone.flac", &flac, sr),
        ("tone.mp3", &mp3, sr8),
        ("tone.opus", &opus, sr48),
        ("tone.vorbis", &vorbis, sr),
    ] {
        println!("{name}: {} bytes", bytes.len());
        let (pcm, reported) = match name.rsplit('.').next().unwrap() {
            "flac" => decode_probe::<FlacReader>(bytes),
            "mp3" => decode_probe::<tpt_av_cadence_mp3::Mp3Reader>(bytes),
            "opus" => decode_probe::<tpt_av_cadence_opus::OggOpusReader>(bytes),
            _ => decode_probe::<tpt_av_cadence_vorbis::VorbisFormatReader>(bytes),
        };
        let rms = (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len().max(1) as f32).sqrt();
        println!(
            "  decoded {} samples, reported rate {reported} (source {rate}), rms {rms:.4}",
            pcm.len()
        );
    }
    let aac = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tpt-av-cadence-aac/tests/data/tone.aac"
    ))
    .unwrap();
    println!("tone.aac (bundled): {} bytes", aac.len());
    let (pcm, reported) = decode_probe::<tpt_av_cadence_aac::AacReader>(&aac);
    let rms = (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len().max(1) as f32).sqrt();
    println!(
        "  decoded {} samples, reported rate {reported}, rms {rms:.4}",
        pcm.len()
    );
}

fn decode_probe<R: FormatReader>(bytes: &[u8]) -> (Vec<f32>, u32) {
    let mut reader = R::open(Box::new(Cursor::new(bytes.to_vec()))).expect("fixture re-decode");
    let (channels, rate) = {
        let info = reader.info();
        (info.channels as usize, info.sample_rate)
    };
    let mut pcm = Vec::new();
    let mut buf = vec![0.0f32; 4096 * channels.max(1)];
    while let Ok(frames) = reader.decoder().decode(&mut buf) {
        if frames == 0 {
            break;
        }
        pcm.extend_from_slice(&buf[..frames * channels]);
    }
    (pcm, rate)
}
