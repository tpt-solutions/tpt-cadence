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

`Mp3Encoder` (`src/encoder.rs`) writes MPEG-1/2/2.5 Layer III across all
three version families — MPEG-1 at 32/44.1/48 kHz (32-320 kbps) and the
MPEG-2/2.5 LSF families at 16/22.05/24 and 8/11.025/12 kHz (8-160 kbps) —
with a fixed CBR bitrate, long blocks, mid/side stereo decided per frame
(independent stereo otherwise), full Huffman book/region/count1 selection,
and the ISO/LAME two-loop quantizer: an inner global-gain rate loop plus
outer per-band scalefactor amplification steered by a psychoacoustic model
(spreading function, ATH, tonality). The LSF families use their own
9-bit mixed-radix `scalefac_compress` search over the partitioned
scalefactor transmission. The full cross-frame bit reservoir is
implemented: unspent payload is banked (511 bytes on MPEG-1, 255 on LSF)
and lent to later frames via `main_data_begin`, with the granule lead-in
written into the exact bytes both decoder families reach back to.
Escape-coded lines are kept inside FFmpeg's requantization window so
encoder, this crate's decoder, and FFmpeg stay in exact agreement:
FFmpeg's decode of the encoder's output matches ours at 114-121 dB across
the whole MPEG-1 bitrate ladder and all nine tested LSF configurations,
for tonal, noise, and mid/side material alike
(`tests/encoder_ffmpeg_crosscheck.rs`), at unity absolute gain (decoded
amplitude equals the source; verified per-material by the scale-defect
regression).

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
