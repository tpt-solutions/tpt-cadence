# Changelog

All notable changes to `tpt-av-cadence-opus` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Opus packet parser (TOC byte, frame framing) and range coder.
- Full CELT decoder (unit-tested).
- Full SILK decoder (unit-tested).

### Known issues

- CELT has an unresolved bit-exactness bug: a residual `final_range` desync
  versus the RFC 6716 conformance vectors (see `todo.md`).
- Hybrid SILK+CELT mode is not yet implemented.
- Not yet validated end-to-end against official RFC 6716 test vectors.
