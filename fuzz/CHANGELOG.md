# Changelog

All notable changes to the `tpt-cadence-fuzz` harness will be documented in this file.

This crate is excluded from the workspace and is not published. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- `cargo-fuzz` harness with ten targets over every parser-facing entry point:
  `opus_packet`, `opus_decode`, `opus_ogg_decode`, `vorbis_stream`, `ogg_pages`,
  `mp3_decode`, `flac_decode`, `aiff_decode`, `wav_decode`, and `aac_adts`. See
  `README.md` for the full table.

### Changed

- `fuzz/Cargo.lock` is now tracked, so fuzz builds resolve the same dependency
  set (added alongside the AAC SBR underflow fix).

### Notes

- Targets run on Linux and macOS only. libFuzzer is not supported on
  Windows/MSVC.
- Crash regressions found by fuzzing are kept as in-crate tests where possible:
  the AAC SBR grid underflow case (`tpt-av-cadence-aac/tests/fuzz_regressions.rs`)
  and the Ogg Opus reader stall input (`tpt-av-cadence-opus/tests/data/fuzz_ogg_stall.bin`).
