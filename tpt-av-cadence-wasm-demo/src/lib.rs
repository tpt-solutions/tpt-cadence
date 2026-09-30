//! WASM smoke demo for the `tpt-cadence` decoder crates (grew out of
//! `todo.md`'s "WASM build feasibility spike", which proved the pattern
//! with WAV alone). Exports one `decode_<format>_to_f32` /
//! `<format>_sample_rate` pair per supported format, so every decoder
//! crate is exercised *through the wasm-bindgen boundary*, not just
//! compiled for `wasm32-unknown-unknown`. This is deliberately not a
//! general-purpose WASM API surface (a real one would stream, propagate
//! proper JS error types, and expose metadata); each function opens the
//! whole buffer as a cursor and decodes it to the end.
//!
//! The Node.js harness in `test.js` (run by CI's `wasm` job) verifies each
//! format end to end: lossless formats (WAV/AIFF synthesized in JS, FLAC
//! from this workspace's own encoder) must decode sample-exactly; lossy
//! formats (MP3/Opus/Vorbis from this workspace's own encoders, AAC from
//! the AAC crate's bundled `tone.aac`) are checked for sample rate,
//! duration, and level.
//!
//! Build: `cargo build -p tpt-av-cadence-wasm-demo --target wasm32-unknown-unknown --release`
//! Bind: `wasm-bindgen target/wasm32-unknown-unknown/release/tpt_av_cadence_wasm_demo.wasm --out-dir pkg --target nodejs`
//! Run: `node test.js` (see the CI `wasm` job).

use tpt_av_cadence_aac::AacReader;
use tpt_av_cadence_aiff::AiffReader;
use tpt_av_cadence_core::FormatReader;
use tpt_av_cadence_flac::FlacReader;
use tpt_av_cadence_mp3::Mp3Reader;
use tpt_av_cadence_opus::OggOpusReader;
use tpt_av_cadence_vorbis::VorbisFormatReader;
use tpt_av_cadence_wav::WavReader;
use wasm_bindgen::prelude::*;

/// Opens `bytes` with `R` and decodes the whole stream to interleaved
/// `f32` PCM, returning `(pcm, sample_rate)` — `(empty, 0)` on any error
/// (WASM-exported functions keep error handling simple here; a real API
/// would propagate a proper error type to JS instead).
fn decode_all<R: FormatReader>(bytes: &[u8]) -> (Vec<f32>, u32) {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let Ok(mut reader) = R::open(Box::new(cursor)) else {
        return (Vec::new(), 0);
    };
    let (channels, sample_rate) = {
        let info = reader.info();
        (info.channels as usize, info.sample_rate)
    };
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 4096 * channels.max(1)];
    while let Ok(frames) = reader.decoder().decode(&mut buf) {
        if frames == 0 {
            break;
        }
        out.extend_from_slice(&buf[..frames * channels]);
    }
    (out, sample_rate)
}

macro_rules! format_exports {
    ($decode:ident, $rate:ident, $reader:ty) => {
        /// Decodes a complete file (as bytes) to interleaved `f32` PCM.
        /// Returns an empty array on any decode error.
        #[wasm_bindgen]
        pub fn $decode(bytes: &[u8]) -> Vec<f32> {
            decode_all::<$reader>(bytes).0
        }

        /// Returns the stream's sample rate, or 0 on error. A second
        /// export per format so the demo also proves
        /// `FormatReader::info()` (not just `decode()`) works through the
        /// wasm-bindgen boundary.
        #[wasm_bindgen]
        pub fn $rate(bytes: &[u8]) -> u32 {
            decode_all::<$reader>(bytes).1
        }
    };
}

format_exports!(decode_wav_to_f32, wav_sample_rate, WavReader);
format_exports!(decode_aiff_to_f32, aiff_sample_rate, AiffReader);
format_exports!(decode_flac_to_f32, flac_sample_rate, FlacReader);
format_exports!(decode_mp3_to_f32, mp3_sample_rate, Mp3Reader);
format_exports!(decode_aac_to_f32, aac_sample_rate, AacReader);
format_exports!(decode_opus_to_f32, opus_sample_rate, OggOpusReader);
format_exports!(decode_vorbis_to_f32, vorbis_sample_rate, VorbisFormatReader);
