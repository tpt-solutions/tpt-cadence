# Changelog

All notable changes to `tpt-av-cadence-wav` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.0] - 2026-11-04

### Added

- RIFF/WAVE chunk parser and `WavDecoder`/`WavReader` implementing the
  `Decoder`/`FormatReader` traits from `tpt-av-cadence-core`.
- Support for 8/16/24/32-bit integer PCM and 32/64-bit IEEE float WAVE data.
- Bit-exact conformance tests cross-checked against FFmpeg output.
- `WavEncoder` (`encode`/`finish`): writes 8/16/24/32-bit integer and 32/64-bit
  float WAVE files. The RIFF and data sizes are patched in `finish()`.
  Round trips are verified against the decoder. Example: `wav_encode`.

### Changed

- Bulk-slab frame decode with format dispatch hoisted out of the per-sample
  loop. This gives 11-14x on the container decode benches with bit-identical
  output (performance pass, 2026-10-06).
