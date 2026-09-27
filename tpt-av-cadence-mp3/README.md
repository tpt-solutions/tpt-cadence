# tpt-av-cadence-mp3

MPEG Layer III (MP3) decoder for the `tpt-cadence` audio codec suite.

**Status:** 🚧 Functional, but not yet fully audited. Achieves >100dB SNR /
<=1e-5 peak error versus FFmpeg across all ten bundled test streams, plus a
generated oracle matrix (`tests/ffmpeg_oracle_matrix.rs`): 28 LAME-encoded
streams spanning MPEG-1/2/2.5 sample rates, the 8–320 kbps bitrate ladder,
all channel modes, and header-surgery variants, each byte-tile-verified and
oracle-compared, with side-info feature coverage asserted so the corpus
cannot silently stop exercising short blocks, scfsi, or reservoir behavior.
The official ISO/IEC 11172-4 conformance bitstreams remain unobtainable
(re-verified 2026-09-27); this matrix is the systematic stand-in. A
real-time-safety audit is done; broader official conformance testing is
still open pending obtainable vectors.

## Encoder

`Mp3Encoder` (`src/encoder.rs`) writes MPEG-1 Layer III: a fixed CBR bitrate
(32-320 kbps), long blocks, mid/side stereo decided per frame (independent
stereo otherwise), full Huffman book/region/count1 selection, and a
global-gain rate loop per granule. Frames decode cleanly in this crate's own
decoder and in FFmpeg, whose decode of the encoder's output must agree with
ours at >=100 dB SNR across the whole bitrate ladder
(`tests/encoder_ffmpeg_crosscheck.rs`). Per-band scalefactor amplification
steered by a psychoacoustic model is implemented but temporarily disabled
pending root-cause of an inter-decoder divergence (tracked in the root
`todo.md`).

## Usage

The public decoder API follows the same `Decoder`/`FormatReader` shape used
throughout the suite (see the root [README](../README.md#quickstart) for the
general pattern). Consult `src/lib.rs` in this crate for the exact reader
type and entry points, as the public API is still settling while conformance
work continues.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
