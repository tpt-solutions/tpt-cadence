# Changelog

All notable changes to `tpt-av-cadence-test-utils` will be documented in this
file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- FFmpeg-comparison helpers (`assert_bit_exact_vs_ffmpeg`) that skip
  gracefully when FFmpeg is not installed locally.
- MD5 checksum utilities used to verify FLAC decode output against IETF
  testbench vectors.
- Fuzz-testing helpers shared across decoder crates.

### Notes

- Internal dev-only crate; not intended for external use.
