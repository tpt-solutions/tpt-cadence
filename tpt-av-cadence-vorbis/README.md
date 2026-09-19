# tpt-av-cadence-vorbis

Ogg Vorbis I decoder for the `tpt-cadence` audio codec suite, built on the
shared `tpt-av-cadence-ogg` page/packet layer.

**Status:** ✅ Conformance-tested. Implemented:

- Full Vorbis I decode: codebooks (canonical Huffman + VQ lookups),
  floor 0/1 curves, residue 0/1/2, square-polar channel coupling,
  FFT-based synthesis MDCT, FFmpeg-compatible overlap-add, block-size
  switching, granule end-trimming, decode-and-discard seek
- Core `Decoder`/`FormatReader` impls (`VorbisDecoder`/`VorbisFormatReader`)

Conformance (`tests/conformance.rs`): six bundled libvorbis fixtures
(mono/stereo, 32/44.1/48 kHz, quality −1…4, heavy block switching) decode
at **136–138 dB SNR with exact output lengths** against the FFmpeg
reference, plus bit-identical seek(0) replay, sample-exact mid-stream
seek rejoin, and malformed-input no-panic coverage.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
