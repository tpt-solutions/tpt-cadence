# Changelog

All notable changes to `tpt-av-cadence-mp3` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- MP3 encoder bit-allocation rework: encode tables for all 32 big_values
  Huffman books (mechanically derived from the decoder's own tables and
  verified bit-identical to FFmpeg's canonical code assignment), both
  count1 quadruple tables, exhaustive three-region book/region selection,
  count1 tails with mid-band `big_values` continuation, per-band
  scalefactor machinery (bit-exact parity with the decoder's
  `decode_scalefactors`), the >4095-bit `part2_3_length` overflow guard
  with an emit-time trim backstop, per-frame mid/side stereo
  (mode_ext bit 2) with the exact `(L±R)·2^-3/2` transform, and intra-frame
  bit-budget pooling. The ISO/LAME two-loop quantizer structure (psycho-
  acoustic thresholds, scalefactor amplification, compress selection) is
  implemented and unit-tested; amplification is currently gated off
  (`PSY_AMPLIFICATION_ROUNDS = 0`) pending root-cause of an
  inter-decoder divergence, tracked in the root `todo.md`.
- Generated FFmpeg-oracle conformance matrix (`tests/ffmpeg_oracle_matrix.rs`):
  the official ISO/IEC 11172-4 conformance bitstreams remain unobtainable
  (re-verified 2026-09-27), so 28 LAME-encoded streams are now synthesized at
  test time across MPEG-1/2/2.5 sample rates, the 8–320 kbps bitrate ladder,
  mono/stereo/joint modes, transient content, reservoir-off, and
  header-surgery variants (dual-channel mode, flag flips, Xing-strip). Each
  stream must byte-tile exactly per the ISO frame-size formula and match
  FFmpeg's decode at >100 dB SNR / <=1e-5 peak (LAME gapless-tag alignment
  applied where the muxer writes one). A companion test independently parses
  the corpus's side info and asserts the Layer III feature space
  (short/mixed/start/stop blocks, scfsi, bit-reservoir use, preflag,
  scalefac_scale, subblock gains, both count1 tables) is actually exercised.
- Encoder bitrate-ladder oracle in `tests/encoder_ffmpeg_crosscheck.rs`:
  every standard MPEG-1 bitrate in mono and stereo must decode cleanly in
  FFmpeg AND FFmpeg's decode of the same bytes must agree with this crate's
  decode at the suite gate (now passing for the full ladder after the
  encoder rework).
- MPEG Layer III (MP3) decoder core, functional against all ten bundled test
  streams, achieving >100dB SNR / <=1e-5 peak error versus FFmpeg.

### Known issues

- Broader official conformance testing still open (vectors unobtainable;
  the generated oracle matrix stands in — see above). Layer III intensity
  stereo and free-format bitrates are not exercised by any obtainable
  oracle material.

### Added

- `tests/iso_conformance.rs`: optional run over the mpg123 ISO/IEC
  11172-3 conformance streams (`MP3_CONS_DIR`; ignored by default — the
  streams live in the mpg123 SVN `test/` directory), comparing each
  Layer III stream against the FFmpeg decode at the suite's external
  gate.
- `tests/rt_safety.rs`: allocation-counting global-allocator test proving
  `Decoder::decode` performs zero allocations on successful calls across
  all ten bundled fixtures (error-path formatting is the accepted
  project-wide exception). Closes the real-time-safety audit item:
  reservoir bounds (clamped + frame-drop), mixed-block processing, and
  malformed-input proptests were already covered.
