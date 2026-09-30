// Node.js smoke test proving the wasm32-unknown-unknown build of
// tpt-cadence's decoder crates actually decodes correctly through the
// wasm-bindgen boundary, for every supported format — not just
// "compiles". See src/lib.rs for the build/bind steps that produce ./pkg.
//
// Lossless formats (WAV/AIFF synthesized here, FLAC from this workspace's
// own encoder, committed under fixtures/) are checked sample-exactly;
// lossy formats (MP3/Opus/Vorbis from this workspace's own encoders, AAC
// from the AAC crate's bundled tone.aac) are checked for sample rate,
// duration, and level.
//
// Run: node test.js

const fs = require("fs");
const path = require("path");
const {
  decode_wav_to_f32,
  wav_sample_rate,
  decode_aiff_to_f32,
  aiff_sample_rate,
  decode_flac_to_f32,
  flac_sample_rate,
  decode_mp3_to_f32,
  mp3_sample_rate,
  decode_aac_to_f32,
  aac_sample_rate,
  decode_opus_to_f32,
  opus_sample_rate,
  decode_vorbis_to_f32,
  vorbis_sample_rate,
} = require("./pkg/tpt_av_cadence_wasm_demo.js");

const fixture = (name) =>
  new Uint8Array(fs.readFileSync(path.join(__dirname, "fixtures", name)));

function check(name, ok, detail) {
  if (!ok) {
    throw new Error(`${name}: ${detail}`);
  }
}

function rms(pcm) {
  let acc = 0;
  for (const s of pcm) {
    acc += s * s;
  }
  return Math.sqrt(acc / Math.max(1, pcm.length));
}

function checkLossy(name, pcm, rate, expectedRate, expectedSamples, expectedRms) {
  check(name, rate === expectedRate, `expected sample_rate=${expectedRate}, got ${rate}`);
  check(name, pcm.length > 0, "decoded no samples");
  check(
    name,
    pcm.length >= 0.7 * expectedSamples && pcm.length <= 1.5 * expectedSamples,
    `expected ~${expectedSamples} samples, got ${pcm.length}`
  );
  for (let i = 0; i < pcm.length; i++) {
    check(name, Number.isFinite(pcm[i]), `sample ${i} is not finite`);
  }
  const ratio = rms(pcm) / expectedRms;
  check(
    name,
    ratio > 0.6 && ratio < 1.35,
    `rms ${rms(pcm).toFixed(4)} is ${ratio.toFixed(3)}x the source's ${expectedRms.toFixed(4)}`
  );
  console.log(`PASS: ${name} (${rate} Hz, ${pcm.length} samples, rms ${rms(pcm).toFixed(4)})`);
}

// --- WAV: synthesized here, lossless, exact ------------------------------

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

const wavSamples = [0, 16384, -16384, 32767];
const wavRate = wav_sample_rate(buildWav(wavSamples));
check("wav", wavRate === 8000, `expected sample_rate=8000, got ${wavRate}`);
const wavPcm = decode_wav_to_f32(buildWav(wavSamples));
check("wav", wavPcm.length === wavSamples.length, `expected ${wavSamples.length} samples, got ${wavPcm.length}`);
wavSamples.forEach((s, i) => {
  const diff = Math.abs(wavPcm[i] - s / 32768);
  check("wav", diff <= 1e-6, `sample ${i}: expected ${s / 32768}, got ${wavPcm[i]}`);
});
console.log(`PASS: wav (${wavRate} Hz, ${wavPcm.length} samples, exact)`);

// --- AIFF: synthesized here (big-endian IFF), lossless, exact ------------

function buildAiff(samples) {
  const dataBytes = samples.length * 2;
  const buf = Buffer.alloc(54 + dataBytes);
  buf.write("FORM", 0, "ascii");
  buf.writeUInt32BE(46 + dataBytes, 4);
  buf.write("AIFF", 8, "ascii");
  buf.write("COMM", 12, "ascii");
  buf.writeUInt32BE(18, 16); // COMM payload size
  buf.writeUInt16BE(1, 20); // mono
  buf.writeUInt32BE(samples.length, 22); // frame count
  buf.writeUInt16BE(16, 26); // bits per sample
  // 80-bit IEEE-754 extended float for 44100.0.
  Buffer.from([0x40, 0x0e, 0xac, 0x44, 0, 0, 0, 0, 0, 0]).copy(buf, 28);
  buf.write("SSND", 38, "ascii");
  buf.writeUInt32BE(8 + dataBytes, 42);
  buf.writeUInt32BE(0, 46); // data offset
  buf.writeUInt32BE(0, 50); // block size
  samples.forEach((s, i) => buf.writeInt16BE(s, 54 + i * 2));
  return new Uint8Array(buf);
}

const aiffSamples = [0, 16384, -16384, 32767];
const aiffRate = aiff_sample_rate(buildAiff(aiffSamples));
check("aiff", aiffRate === 44100, `expected sample_rate=44100, got ${aiffRate}`);
const aiffPcm = decode_aiff_to_f32(buildAiff(aiffSamples));
check("aiff", aiffPcm.length === aiffSamples.length, `expected ${aiffSamples.length} samples, got ${aiffPcm.length}`);
aiffSamples.forEach((s, i) => {
  const diff = Math.abs(aiffPcm[i] - s / 32768);
  check("aiff", diff <= 1e-6, `sample ${i}: expected ${s / 32768}, got ${aiffPcm[i]}`);
});
console.log(`PASS: aiff (${aiffRate} Hz, ${aiffPcm.length} samples, exact)`);

// --- FLAC: this workspace's own encoder's output, lossless, exact -------

const flacBytes = fixture("tone.flac");
const flacRate = flac_sample_rate(flacBytes);
const flacPcm = decode_flac_to_f32(flacBytes);
const flacSourceSamples = 17640;
check("flac", flacRate === 44100, `expected sample_rate=44100, got ${flacRate}`);
check("flac", flacPcm.length === flacSourceSamples, `expected ${flacSourceSamples} samples, got ${flacPcm.length}`);
// The source is 0.4 * sin(2*pi*440*t/sr); the FLAC round trip must land
// within two int16 quanta of it (f32 analysis vs JS f64 sin).
let flacMaxDiff = 0;
for (let i = 0; i < flacPcm.length; i++) {
  const src = 0.4 * Math.sin((2 * Math.PI * 440 * i) / 44100);
  flacMaxDiff = Math.max(flacMaxDiff, Math.abs(flacPcm[i] - src));
}
check("flac", flacMaxDiff <= 2 / 32768, `max sample deviation ${flacMaxDiff} exceeds 2 int16 quanta`);
console.log(`PASS: flac (${flacRate} Hz, ${flacPcm.length} samples, exact within 2 int16 quanta)`);

// --- Lossy formats: rate, duration, and level ----------------------------

// MP3: our encoder, MPEG-2.5 8 kHz mono 8 kbps (0.4 s source).
checkLossy(
  "mp3",
  decode_mp3_to_f32(fixture("tone.mp3")),
  mp3_sample_rate(fixture("tone.mp3")),
  8000,
  3200,
  0.4 / Math.SQRT2
);

// Ogg Opus: our encoder, 48 kHz mono 16 kbps CBR (0.4 s source; the
// container recovers the exact input sample count).
checkLossy(
  "opus",
  decode_opus_to_f32(fixture("tone.opus")),
  opus_sample_rate(fixture("tone.opus")),
  48000,
  19200,
  0.4 / Math.SQRT2
);

// Ogg Vorbis: our encoder, 44.1 kHz mono quality 3 (0.4 s source).
checkLossy(
  "vorbis",
  decode_vorbis_to_f32(fixture("tone.vorbis")),
  vorbis_sample_rate(fixture("tone.vorbis")),
  44100,
  17640,
  0.4 / Math.SQRT2
);

// AAC: the AAC crate's bundled tone.aac (no AAC encoder exists here).
const aacBytes = new Uint8Array(
  fs.readFileSync(path.join(__dirname, "../tpt-av-cadence-aac/tests/data/tone.aac"))
);
checkLossy(
  "aac",
  decode_aac_to_f32(aacBytes),
  aac_sample_rate(aacBytes),
  44100,
  46080,
  0.3373
);

console.log("PASS: all formats decode correctly under wasm32");
