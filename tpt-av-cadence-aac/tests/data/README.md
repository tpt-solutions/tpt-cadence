# AAC test data

- `test.aac` — 1 s stereo 44.1 kHz AAC-LC ADTS file, encoded with FFmpeg's
  native encoder from a deterministic two-tone test WAV (generated in-test
  by the conformance suite).
- `test_ref.f32` — FFmpeg's decode of `test.aac` as raw interleaved f32le
  (reference implementation output for the comparison harness).
