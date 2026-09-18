# Changelog

All notable changes to `tpt-av-cadence-core` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Initial `Decoder` and `FormatReader` traits defining the real-time-safe
  decode contract used by every format crate in the suite.
- `StreamInfo`, `Format`, `SampleFormat`, and `ChannelLayout` shared types.
- `CadenceError` unified error type.
- Sample conversion helpers (`int_to_f32`) and buffered/byte source
  abstractions for reading encoded audio.
