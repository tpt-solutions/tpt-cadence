# Changelog

All notable changes to `tpt-av-cadence-core` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.0] - 2026-11-04

### Added

- Initial `Decoder` and `FormatReader` traits defining the real-time-safe
  decode contract used by every format crate in the suite.
- `StreamInfo`, `Format`, `SampleFormat`, and `ChannelLayout` shared types.
- `CadenceError` unified error type.
- Sample conversion helpers (`int_to_f32`) and buffered/byte source
  abstractions for reading encoded audio.
- `Encoder` trait (`encode(&[f32])`, `finish()`) implemented by every format
  writer in the suite (WAV, AIFF, PCM, FLAC, MP3, Vorbis, Ogg Opus).

### Changed

- The buffered source reads in 64 KiB chunks (performance pass, 2026-10-06).
