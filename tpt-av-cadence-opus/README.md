# tpt-av-cadence-opus

Opus (RFC 6716) decoder for the `tpt-cadence` audio codec suite, with an
Ogg Opus (RFC 7845) container layer.

**Status:** ✅ Conformance-tested. Implemented: 

- Packet parser (TOC byte, frame framing) and the bit-exact range coder
- Full CELT decoder and full SILK decoder
- Full top-level `OpusDecoder` (the `opus_decoder.c` state machine):
  SILK-only, CELT-only, and hybrid packets, mode-transition crossfades,
  5 ms CELT redundancy frames, hybrid low-band mixing, DTX/PLC
- Ogg Opus container: `OpusHead`/`OpusTags`, pre-skip and end-trim
  granule bookkeeping, output gain, and `OggOpusReader` implementing the
  core `Decoder`/`FormatReader` traits

Conformance against the official RFC 6716 test vectors
(`tests/conformance.rs`, `#[ignore]`d — needs `OPUS_TESTVECTORS_DIR`):
**100% `final_range` match on every packet of all 12 vectors** (16,073
packets). The SILK-only vectors (02–04) decode bit-exact in PCM; versus a
live libopus 1.5.2 build the other vectors reach 37–110 dB SNR, with the
residual an accepted, platform-specific float-ULP gap (libopus's SIMD
accumulation order; the `.dec` reference files themselves drift from
modern libopus at the same scale). `final_range` is the durable
bit-exactness contract.

## Encoder status

A CELT-only `Encoder` and top-level Ogg Opus writer are implemented for
48 kHz mono/stereo, fullband, CBR, and all four CELT frame sizes. CELT
entropy output uses libopus-style fixed-size storage, so every packet stays
at the requested byte budget and cannot change decoder-side PVQ allocation
through silent overshoot. RFC 7845 pre-skip and granule positions include the
measured 120-sample CELT overlap delay; `finish()` flushes the delayed tail so
decoders recover the exact original sample count. This remains foundation
work: SILK/hybrid encoding, psychoacoustic tuning, and stereo coupling are
still open and tracked in [`todo.md`](../todo.md) at the repository root.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
