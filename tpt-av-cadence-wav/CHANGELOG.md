# Changelog

All notable changes to `tpt-av-cadence-wav` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- RIFF/WAVE chunk parser and `WavDecoder`/`WavReader` implementing the
  `Decoder`/`FormatReader` traits from `tpt-av-cadence-core`.
- Support for 8/16/24/32-bit integer PCM and 32/64-bit IEEE float WAVE data.
- Bit-exact conformance tests cross-checked against FFmpeg output.
