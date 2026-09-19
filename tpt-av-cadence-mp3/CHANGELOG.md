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

### Added

- `tests/iso_conformance.rs`: optional run over the mpg123 ISO/IEC
  11172-3 conformance streams (`MP3_CONS_DIR`; ignored by default — the
  streams live in the mpg123 SVN `test/` directory), comparing each
  Layer III stream against the FFmpeg decode at the suite's external
  gate.
- `tests/rt_safety.rs`: allocation-counting global-allocator test proving
  `Decoder::decode` performs zero allocations on successful calls across
  all ten bundled fixtures (error-path formatting is the accepted
  project-wide exception). Closes the real-time-safety audit item:
  reservoir bounds (clamped + frame-drop), mixed-block processing, and
  malformed-input proptests were already covered.
