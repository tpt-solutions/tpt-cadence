# Changelog

All notable changes to `tpt-av-cadence-pcm` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Headerless PCM reader supporting signed/unsigned int 8/16/24/32-bit and
  float 32/64-bit sample formats, in both little-endian and big-endian byte
  order.
- Implements the shared `Decoder` trait from `tpt-av-cadence-core`.
- `PcmEncoder`: headerless writer for the same sample formats and byte orders.
  Example: `pcm_encode`.

### Changed

- Bulk-slab decode with format dispatch hoisted out of the per-sample loop
  (11-14x on the container decode benches, bit-identical output; performance
  pass, 2026-10-06).
