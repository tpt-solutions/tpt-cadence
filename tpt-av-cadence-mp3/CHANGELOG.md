# Changelog

All notable changes to `tpt-av-cadence-mp3` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- MPEG Layer III (MP3) decoder core, functional against all ten bundled test
  streams, achieving >100dB SNR / <=1e-5 peak error versus FFmpeg.

### Known issues

- Real-time-safety audit not yet performed.
- Broader official conformance testing still open.
