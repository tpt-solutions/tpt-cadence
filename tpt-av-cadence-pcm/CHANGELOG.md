# Changelog

All notable changes to `tpt-av-cadence-pcm` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Headerless PCM reader supporting signed/unsigned int 8/16/24/32-bit and
  float 32/64-bit sample formats, in both little-endian and big-endian byte
  order.
- Implements the shared `Decoder` trait from `tpt-av-cadence-core`.
