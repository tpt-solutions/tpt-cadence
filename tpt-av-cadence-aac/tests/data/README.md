# AAC test data

- `tone.aac` — 1 s mono 44.1 kHz AAC-LC ADTS file, encoded with FFmpeg's
  native encoder from a deterministic 1 kHz sine WAV.
- `tone_ref.f32` — FFmpeg's decode of `tone.aac` as raw interleaved f32le
  (reference implementation output for the comparison harness).
- `test.aac` — 1 s stereo 44.1 kHz AAC-LC ADTS file, encoded with FFmpeg's
  native encoder from a deterministic two-tone test WAV (generated in-test by
  the conformance suite).
- `test_ref.f32` — FFmpeg's decode of `test.aac` as raw interleaved f32le
  (reference implementation output for the comparison harness).

The conformance suite (`tests/conformance.rs`) requires >100 dB whole-stream
SNR and <=1e-5 peak error against these references — a tolerance-based
float comparison, not bit-exact equality.
