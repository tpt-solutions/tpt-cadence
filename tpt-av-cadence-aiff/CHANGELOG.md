# Changelog

All notable changes to `tpt-av-cadence-aiff` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- AIFF/AIFC (big-endian IFF) chunk parser and `AiffDecoder`/`AiffReader`
  implementing the `Decoder`/`FormatReader` traits from `tpt-av-cadence-core`.
- Support for classic AIFF signed 8/16/24/32-bit big-endian PCM, and AIFF-C
  compression types `NONE`, `twos`, `sowt`, `FL32`, `in24`, and `ni24`.
- `ima4`, `ULAW`, and `ALAW` are explicitly rejected as unsupported.
- Bit-exact conformance tests.

### Added

- `tests/ffmpeg_crosscheck.rs`: bit-exact (tolerance 0) cross-check of
  synthetic 8/16/24/32-bit mono/stereo AIFF streams against the FFmpeg
  decode. Skips when FFmpeg is unavailable unless
  `CADENCE_REQUIRE_FFMPEG=1`.
