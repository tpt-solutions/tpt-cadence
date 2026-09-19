# Changelog

All notable changes to `tpt-av-cadence-flac` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- FLAC parser and `FlacDecoder`/`FlacReader` implementing the
  `Decoder`/`FormatReader` traits from `tpt-av-cadence-core`.
- Full frame/subframe model: Constant, Verbatim, Fixed (orders 0-4), and LPC
  (orders 1-32) subframes.
- Partitioned Rice and Rice2 residual decoding with escaped partitions,
  wasted-bit removal, and left/side, right/side, and mid/side stereo
  decorrelation.
- Fixed and variable block sizes, CRC-8/CRC-16 integrity checking, and
  STREAMINFO metadata parsing.
- MD5-verified against the official IETF FLAC decoder testbench vectors.

### Added

- `tests/ffmpeg_crosscheck.rs`: bit-exact (tolerance 0) cross-check of
  every bundled subset/uncommon fixture against the FFmpeg decode.
  Skips when FFmpeg is unavailable unless `CADENCE_REQUIRE_FFMPEG=1`.
