# tpt-av-cadence-vorbis

Ogg Vorbis I decoder and encoder for the `tpt-cadence` audio codec suite, built on the
shared `tpt-av-cadence-ogg` page/packet layer.

**Status:** ✅ Conformance-tested. Implemented:

- Full Vorbis I decode: codebooks (canonical Huffman + VQ lookups),
  floor 0/1 curves, residue 0/1/2, square-polar channel coupling,
  FFT-based synthesis MDCT, FFmpeg-compatible overlap-add, block-size
  switching, granule end-trimming, decode-and-discard seek
- Core `Decoder`/`FormatReader` impls (`VorbisDecoder`/`VorbisFormatReader`)
- `VorbisEncoder` (floor 1, coupled residue type 1, quality 0..=10; FFmpeg-verified):
  - Block switching (2048/256) and a Bark-band masking model driving the floor
  - Per-file adaptive Huffman books via `VorbisEncoder::new_adaptive` (whole-stream
    buffering; nothing is written before `finish`). The streaming `VorbisEncoder::new`
    keeps the static books.

The masking constants have not been validated by listening tests, so perceptual
quality is unconfirmed; SNR is not the right yardstick for a masking-driven coder.
See `todo.md` in the repository root.

Conformance (`tests/conformance.rs`): six bundled libvorbis fixtures
(mono/stereo, 32/44.1/48 kHz, quality −1…4, heavy block switching) decode
at **136–138 dB SNR with exact output lengths** against the FFmpeg
reference, plus bit-identical seek(0) replay, sample-exact mid-stream
seek rejoin, and malformed-input no-panic coverage.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
