//! WASM build feasibility spike for `tpt-cadence` (see `todo.md`'s
//! "WASM build feasibility spike" entry). This crate is intentionally
//! tiny: it exposes exactly one function, decoding a WAV byte buffer to
//! interleaved `f32` PCM, to prove the decoder crates compile *and run
//! correctly* under `wasm32-unknown-unknown` — not to be a general-purpose
//! WASM API surface for the suite (a real one would want to expose every
//! format, streaming decode, and proper JS-side error types).
//!
//! Build: `cargo build -p tpt-av-cadence-wasm-demo --target wasm32-unknown-unknown --release`
//! Bind: `wasm-bindgen target/wasm32-unknown-unknown/release/tpt_av_cadence_wasm_demo.wasm --out-dir pkg --target nodejs`
//! Run: see `test.js` in this directory for a minimal Node.js harness that
//! feeds it a synthetic WAV and checks the decoded samples.

use tpt_av_cadence_core::FormatReader;
use tpt_av_cadence_wav::WavReader;
use wasm_bindgen::prelude::*;

/// Decodes a complete WAV file (as bytes) to interleaved `f32` PCM.
/// Returns an empty array on any decode error (WASM-exported functions
/// keep error handling simple here; a real API would propagate a proper
/// error type to JS instead).
#[wasm_bindgen]
pub fn decode_wav_to_f32(bytes: &[u8]) -> Vec<f32> {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let Ok(mut reader) = WavReader::open(Box::new(cursor)) else {
        return Vec::new();
    };
    let channels = reader.info().channels as usize;
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 4096 * channels.max(1)];
    while let Ok(frames) = reader.decoder().decode(&mut buf) {
        if frames == 0 {
            break;
        }
        out.extend_from_slice(&buf[..frames * channels]);
    }
    out
}

/// Returns the sample rate of a WAV file's stream info, or 0 on error.
/// A second exported function alongside `decode_wav_to_f32` so the demo
/// also proves `FormatReader::info()` (not just `decode()`) works through
/// the wasm-bindgen boundary.
#[wasm_bindgen]
pub fn wav_sample_rate(bytes: &[u8]) -> u32 {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    match WavReader::open(Box::new(cursor)) {
        Ok(reader) => reader.info().sample_rate,
        Err(_) => 0,
    }
}
