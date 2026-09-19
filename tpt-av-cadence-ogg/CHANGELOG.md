# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `PageReader`: Ogg page/packet layer (RFC 3533) — capture-pattern and
  header parsing, packet reassembly across page/segment boundaries,
  granule + BOS/EOS metadata per completed packet, byte-position reset for
  seek support, and chained-link (fresh BOS) end-of-link detection.
- `page_crc`: the Ogg CRC-32, exposed for test-side page muxing.
- Extracted from `tpt-av-cadence-vorbis`'s private `ogg` module so Opus
  (RFC 7845) can share the same container layer.

### Changed

- `PageReader::restart`: rewind to the first byte of the stream and clear
  ALL page state (including the seen-BOS marker) so leading header pages
  can be parsed again without the stream's own BOS page being mistaken
  for a chained link; `reset` keeps its mid-stream seek semantics.
