// Minimal Node.js harness proving the wasm32-unknown-unknown build of
// tpt-cadence's decoder crates actually decodes correctly through the
// wasm-bindgen boundary, not just "compiles". See this crate's src/lib.rs
// doc comment for the build/bind steps that produce ./pkg.
//
// Run: node test.js

const { decode_wav_to_f32, wav_sample_rate } = require("./pkg/tpt_av_cadence_wasm_demo.js");

// Build a minimal 16-bit mono WAV in memory: RIFF/WAVE, fmt (PCM, 1ch,
// 8000 Hz, 16-bit), data chunk with 4 known samples.
function buildWav(samples) {
  const dataBytes = samples.length * 2;
  const buf = Buffer.alloc(44 + dataBytes);
  buf.write("RIFF", 0, "ascii");
  buf.writeUInt32LE(36 + dataBytes, 4);
  buf.write("WAVE", 8, "ascii");
  buf.write("fmt ", 12, "ascii");
  buf.writeUInt32LE(16, 16); // fmt chunk size
  buf.writeUInt16LE(1, 20); // PCM
  buf.writeUInt16LE(1, 22); // mono
  buf.writeUInt32LE(8000, 24); // sample rate
  buf.writeUInt32LE(8000 * 2, 28); // byte rate
  buf.writeUInt16LE(2, 32); // block align
  buf.writeUInt16LE(16, 34); // bits per sample
  buf.write("data", 36, "ascii");
  buf.writeUInt32LE(dataBytes, 40);
  samples.forEach((s, i) => buf.writeInt16LE(s, 44 + i * 2));
  return new Uint8Array(buf);
}

const samples = [0, 16384, -16384, 32767];
const wav = buildWav(samples);

const rate = wav_sample_rate(wav);
if (rate !== 8000) {
  throw new Error(`expected sample_rate=8000, got ${rate}`);
}

const pcm = decode_wav_to_f32(wav);
if (pcm.length !== samples.length) {
  throw new Error(`expected ${samples.length} decoded samples, got ${pcm.length}`);
}

const expected = samples.map((s) => s / 32768);
for (let i = 0; i < expected.length; i++) {
  const diff = Math.abs(pcm[i] - expected[i]);
  if (diff > 1e-6) {
    throw new Error(`sample ${i}: expected ${expected[i]}, got ${pcm[i]}`);
  }
}

console.log("PASS: wasm32 WAV decode is correct (sample_rate=" + rate + ", " + pcm.length + " samples)");
