//! Debug driver: decodes an ADTS file to interleaved f32le PCM.
//!
//! Usage: `cargo run --release --example aac_dump -- <input.aac> <out.f32>`
//! Set `AAC_DUMP=1` to additionally write per-frame spectral coefficient
//! dumps (pre/post TNS) to `target/dump/`.

use std::fs::File;
use std::io::Write;
use tpt_av_cadence_core::Decoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args.next().expect("usage: aac_dump <in.aac> <out.f32>");
    let output = args.next().expect("usage: aac_dump <in.aac> <out.f32>");

    let file = File::open(&input)?;
    let mut decoder = tpt_av_cadence_aac::AacDecoder::open(Box::new(file))?;
    let channels = usize::from(decoder.info().channels);
    eprintln!(
        "open {input}: {} Hz, {channels} ch",
        decoder.info().sample_rate
    );

    let mut out = std::io::BufWriter::new(File::create(&output)?);
    let mut buf = vec![0.0f32; 1024 * channels];
    let mut frames = 0u64;
    loop {
        match decoder.decode(&mut buf) {
            Ok(0) => break,
            Ok(_) => {
                for v in &buf {
                    out.write_all(&v.to_le_bytes())?;
                }
                frames += 1;
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
