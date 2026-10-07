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
48 kHz mono/stereo, fullband, and all four CELT frame sizes. Rate control:
CBR with libopus-style fixed-size entropy storage (every packet stays at
the requested byte budget and cannot change decoder-side PVQ allocation
through silent overshoot), or loudness-adaptive constrained VBR where each
frame's budget tracks its dynamics around the average target. Stereo
coupling is complete: joint mid/side band coding, plus analysis-driven
intensity stereo (Y = ±X, phase discarded) for the high bands of
channel-similar content, and energy-adaptive dynamic-allocation boosts for
spectral peaks. RFC 7845 pre-skip and granule positions include the
measured 120-sample CELT overlap delay; `finish()` flushes the delayed
tail so decoders recover the exact original sample count.

A SILK encoder (`SilkEncoder`) and hybrid SILK+CELT packets are also
implemented: mono or adaptive mid/side stereo SILK at 8/12/16 kHz internal
rate in 10/20/40/60 ms packets, VBR payloads plus constant-size CBR
(`new_silk_cbr`), SILK-only and hybrid Ogg Opus streams, and stereo hybrid
(split per-channel rate targets). The SILK path follows the reference
closely: the 4-band VAD, Burg LPC, noise-shaping analysis (warped
autocorrelation, spectral tilt, harmonic shaping gain), the reference
noise-shaping quantizer with error feedback, the per-frame rate-control
loop that lands payloads on the requested budget, and the delayed-decision
quantizer behind `set_complexity`. Packet-loss tooling is included: DTX
(`new_silk_dtx`, 1-byte TOC-only packets decoded as comfort noise) and
LBRR/FEC (`set_packet_loss_perc`). The encoder's simulated reconstruction is
bit-identical to the real decoder's output, which the test suite pins for
every rate and packet-size combination.

The CELT encoder chooses its per-band TF resolution, PVQ spread and allocation
trim from the signal (ports of the reference analysis; `set_psychoacoustic(false)`
restores the fixed choices). Remaining: pitch pre-filter, masking-based
quality metrics, and the other format encoders — tracked in
[`todo.md`](https://github.com/tpt-solutions/tpt-cadence/blob/master/todo.md) at the repository root.

## Examples

`examples/opus_encode.rs` writes a test tone as Ogg Opus (`celt`, `vbr`, `silk` or
`hybrid` mode) and `examples/opus_decode.rs` decodes an Ogg Opus file; run with
`cargo run -p tpt-av-cadence-opus --example <name>`.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
