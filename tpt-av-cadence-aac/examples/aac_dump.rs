//! Debug driver: decodes an ADTS file to interleaved f32le PCM.
//!
//! Usage: `cargo run --release --example aac_dump -- <input.aac> <out.f32>`

use std::fs::File;
use std::io::Write;
use tpt_av_cadence_core::Decoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args.next().expect("usage: aac_dump <in.aac> <out.f32>");
    let output = args.next().expect("usage: aac_dump <in.aac> <out.f32>");

    let file = File::open(&input)?;
    // AAC_RAW_ASC=<hex> + raw blocks: decode via the from_config path.
    let mut decoder = if let Ok(hex_asc) = std::env::var("AAC_RAW_ASC") {
        let bytes: Vec<u8> = (0..hex_asc.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex_asc[i..i + 2], 16).unwrap())
            .collect();
        let asc = tpt_av_cadence_aac::AudioSpecificConfig::parse(&bytes)?;
        tpt_av_cadence_aac::AacDecoder::from_config(&asc, Box::new(file))?
    } else {
        tpt_av_cadence_aac::AacDecoder::open(Box::new(file))?
    };
    let channels = usize::from(decoder.info().channels);
    eprintln!(
        "open {input}: {} Hz, {channels} ch",
        decoder.info().sample_rate
    );

    let mut out = std::io::BufWriter::new(File::create(&output)?);
    // Sized for the maximum channel count: channel-configuration-0 streams
    // only learn their channel count from the first decoded PCE. The
    // pre-configuration length 6720 is divisible by every channel count
    // 1..=8, so the decoder's buffer check passes whichever count the PCE
    // declares.
    let mut buf = vec![0.0f32; 6720];
    let mut frames = 0u64;
    loop {
        let channels = usize::from(decoder.info().channels);
        let want = if channels == 0 {
            buf.len()
        } else {
            1024 * channels
        };
        match decoder.decode(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                let channels = usize::from(decoder.info().channels).max(1);
                for v in &buf[..n * channels] {
                    out.write_all(&v.to_le_bytes())?;
                }
                frames += n as u64;
            }
            Err(e) => {
                eprintln!("error after {frames} frames: {e}");
                break;
            }
        }
    }
    eprintln!("wrote {frames} frames to {output}");
    Ok(())
}
