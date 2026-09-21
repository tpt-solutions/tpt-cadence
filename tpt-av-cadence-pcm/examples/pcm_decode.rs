//! Decodes a headerless PCM file to raw interleaved f32 on stdout. Since raw
//! PCM carries no metadata, the format must be supplied on the command line:
//! `cargo run -p tpt-av-cadence-pcm --example pcm_decode -- audio.raw s16le 2 48000 > out.f32`

use std::fs::File;
use std::io::Write;

use tpt_av_cadence_core::{Decoder, SampleFormat};
use tpt_av_cadence_pcm::{ByteOrder, PcmDecoder, PcmFormat};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: decode <audio.raw> <s8|s16le|s16be|s24le|s24be|s32le|s32be|f32le|f32be|f64le|f64be> <channels> <sample_rate>");
    let format_str = args.next().expect("missing sample format");
    let channels: u16 = args
        .next()
        .expect("missing channel count")
        .parse()
        .expect("channels must be a number");
    let sample_rate: u32 = args
        .next()
        .expect("missing sample rate")
        .parse()
        .expect("sample_rate must be a number");

    let (sample_format, byte_order) = match format_str.as_str() {
        "s8" => (SampleFormat::Int8, ByteOrder::Little),
        "s16le" => (SampleFormat::Int16, ByteOrder::Little),
        "s16be" => (SampleFormat::Int16, ByteOrder::Big),
        "s24le" => (SampleFormat::Int24, ByteOrder::Little),
        "s24be" => (SampleFormat::Int24, ByteOrder::Big),
        "s32le" => (SampleFormat::Int32, ByteOrder::Little),
        "s32be" => (SampleFormat::Int32, ByteOrder::Big),
        "f32le" => (SampleFormat::Float32, ByteOrder::Little),
        "f32be" => (SampleFormat::Float32, ByteOrder::Big),
        "f64le" => (SampleFormat::Float64, ByteOrder::Little),
        "f64be" => (SampleFormat::Float64, ByteOrder::Big),
        other => panic!("unknown sample format {other:?}"),
    };

    let format = PcmFormat {
        sample_format,
        byte_order,
        channels,
        sample_rate,
    };
    let file = File::open(&path).expect("open input");
    let mut decoder = PcmDecoder::from_source(Box::new(file), format).expect("open pcm stream");
    eprintln!(
        "pcm: {} Hz, {} ch, {}-bit",
        decoder.info().sample_rate,
        channels,
        decoder.info().bit_depth
    );
    let mut out = std::io::stdout();
    let mut buf = vec![0.0f32; 8192 * channels as usize];
    loop {
        let frames = decoder.decode(&mut buf).expect("decode");
        if frames == 0 {
            break;
        }
        let bytes: Vec<u8> = buf[..frames * channels as usize]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        out.write_all(&bytes).expect("write");
    }
}
