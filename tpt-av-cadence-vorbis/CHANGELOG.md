# Changelog

All notable changes to `tpt-av-cadence-vorbis` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Ogg Vorbis I decoder (implemented across prior sessions on the shared
  `tpt-av-cadence-ogg` page layer; this session brought it to conformance):
  codebook decode with canonical Huffman trees, floor 0/1 curves, residue
  0/1/2, square-polar channel coupling, FFT-based synthesis MDCT with
  FFmpeg-compatible overlap-add, block-size switching, granule-based end
  trimming, and decode-and-discard seek.
- `tests/conformance.rs`: six bundled libvorbis-encoded fixtures (mono and
  stereo, 32/44.1/48 kHz, quality −1…4, and an impulse-train fixture that
  forces heavy long/short block switching), each decoded and compared
  sample-for-sample against the FFmpeg reference at a >100 dB SNR gate.
  Measured: 136–138 dB on every fixture with exact output lengths. Also
  covers seek(0) bit-identical replay, mid-stream seek exact rejoin, and
  malformed-input no-panic handling.
- `tests/data/`: the bundled fixtures and FFmpeg f32le references, with a
  README documenting regeneration.
- Removed the private `ogg` module (page/packet layer): it now lives in
  the shared `tpt-av-cadence-ogg` crate, where Opus (RFC 7845) reuses it.
  Behavior is unchanged.

### Fixed

- **Codebook header layout**: the parser consumed a nonexistent 16-bit
  version field (the spec has none — layout is sync(24), dimensions(16),
  entries(24)), shifting every subsequent field and rejecting every real
  stream.
- **Residue setup cascade order**: the cascade bit vectors of ALL
  classifications are read first, then the book numbers in a second pass
  (spec §8.2); reading them interleaved desynced the setup header.
- **Audio packet header**: long-window packets carry exactly TWO flag bits
  ([previous_window_flag], [next_window_flag]); a third "window" bit was
  being consumed, corrupting every long-block packet.
- **Residue accumulation across packets**: the spec zeroes the return
  vectors per packet because residue decoding ADDS into them; the decoder
  only cleared the upper half of its buffers, so mono (residue type 1)
  streams accumulated stale spectral values and degraded progressively.
  (The type-2 path already zeroed its own scratch, which is why stereo
  was unaffected.)
- **End trimming**: output is capped at the EOS page's granule from the
  moment the page is pulled (`total_frames` set at the same time); a
  page's leading granule is never used as a start offset (a first page's
  granule counts samples at its END and is nonzero even at the stream
  start).
- `seek()` now re-parses the headers, only resumes at true audio pages,
  and lands sample-exactly (verified by the bit-identical mid-stream
  rejoin test).
- Removed session debug scaffolding (VORBIS_TRACE paths) and all crate
  warnings; `cargo clippy --all-targets -- -D warnings` is clean.
