# Conformance dashboard

Generated 2026-09-23 06:48 UTC by `tools/conformance_dashboard.py`. Numbers are scraped from each
crate's own conformance test output (see that crate's `tests/conformance.rs`) — this
script doesn't measure anything independently, it just aggregates what's already printed.

## WAV (bit-exact, no SNR — pass/fail only)

`cargo test -p tpt-av-cadence-wav`: **43 passed**, 0 failed, 0 ignored.

_No per-stream SNR lines emitted by this crate's default test run — either it's a bit-exact crate that asserts equality directly instead of printing an SNR, or its FFmpeg-oracle comparison was skipped because FFmpeg isn't on `PATH` in this environment (check the raw test output for a "skipping"/"FFmpeg not on PATH" line)._

## AIFF (bit-exact, no SNR — pass/fail only)

`cargo test -p tpt-av-cadence-aiff`: **40 passed**, 0 failed, 0 ignored.

_No per-stream SNR lines emitted by this crate's default test run — either it's a bit-exact crate that asserts equality directly instead of printing an SNR, or its FFmpeg-oracle comparison was skipped because FFmpeg isn't on `PATH` in this environment (check the raw test output for a "skipping"/"FFmpeg not on PATH" line)._

## FLAC (bit-exact via embedded MD5 — pass/fail only)

`cargo test -p tpt-av-cadence-flac`: **60 passed**, 0 failed, 0 ignored.

_No per-stream SNR lines emitted by this crate's default test run — either it's a bit-exact crate that asserts equality directly instead of printing an SNR, or its FFmpeg-oracle comparison was skipped because FFmpeg isn't on `PATH` in this environment (check the raw test output for a "skipping"/"FFmpeg not on PATH" line)._

## MP3 (FFmpeg-oracle SNR, needs FFmpeg on PATH)

`cargo test -p tpt-av-cadence-mp3`: **3 passed**, 0 failed, 0 ignored.

_No per-stream SNR lines emitted by this crate's default test run — either it's a bit-exact crate that asserts equality directly instead of printing an SNR, or its FFmpeg-oracle comparison was skipped because FFmpeg isn't on `PATH` in this environment (check the raw test output for a "skipping"/"FFmpeg not on PATH" line)._

## AAC-LC / HE-AAC (FFmpeg-oracle SNR)

`cargo test -p tpt-av-cadence-aac`: **10 passed**, 0 failed, 0 ignored.

| Stream | SNR (dB) |
| :--- | ---: |
| PCE stream tone.aac | 123.77 |
| tone.aac | 123.77 |
| raw config stream | 123.77 |
| PCE stream test.aac | 123.39 |
| test.aac | 123.39 |

## Ogg Vorbis I (FFmpeg-oracle SNR)

`cargo test -p tpt-av-cadence-vorbis`: **4 passed**, 0 failed, 1 ignored.

| Stream | SNR (dB) |
| :--- | ---: |
| mono_32000_q2 | 136.76 |
| mono_44100_q4 | 137.42 |
| stereo_44100_q4 | 137.01 |
| stereo_44100_qm1 | 137.28 |
| stereo_48000_q0 | 136.49 |
| transients_44100_q3 | 138.10 |
| quad_44100_q4 | 136.86 |
