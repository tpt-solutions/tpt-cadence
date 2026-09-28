# Changelog

All notable changes to `tpt-av-cadence-mp3` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **MPEG-2/2.5 (LSF) family encoding**: `Mp3Encoder` now accepts all three
  version families — MPEG-1 at 32/44.1/48 kHz (32-320 kbps), MPEG-2 at
  16/22.05/24 kHz and MPEG-2.5 at 8/11.025/12 kHz (8-160 kbps). LSF frames
  carry one 576-sample granule with the shrunken side info (9/17 bytes,
  8-bit `main_data_begin` capping the reservoir at 255 bytes, no scfsi, no
  preflag bit), and their 9-bit `scalefac_compress` is chosen by a search
  over the mixed-radix `SCF_MOD`/`SCF_PARTITIONS` tables (mirroring the
  decoder's decomposition exactly) for the fewest scalefactor bits that can
  carry every transmitted value, emitted as partitioned scalefactors. A new
  FFmpeg oracle gate (`lsf_encoder_agrees_with_ffmpeg`) covers nine
  rate/rate-family/channel/bitrate configurations at 116-121 dB
  inter-decoder agreement with unity absolute gain.
- Full cross-frame bit reservoir (`main_data_begin` reach-back, up to
  511 bytes): a frame's unspent payload tail is held back from the sink
  and lent to the next frame's budget; the next frame's granule stream
  head is written into the last `main_data_begin` bytes before its
  header, exactly where both decoder families reach back (FFmpeg saves
  the previous payload tail and skips to `8*main_data_begin`; minimp3-
  style decoders keep the same tail via a source offset). A regression
  (`bit_reservoir_banks_quiet_frames_and_borrows_for_loud_ones`) walks
  the emitted frame chain, asserts byte-exact tiling and `main_data_begin`
  engagement, and decodes the borrowed-to tail.
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
  implemented with window-aware amplification active
  (`PSY_AMPLIFICATION_ROUNDS = 48`).
- **Encoder absolute-gain defect FIXED (2^16 analysis↔synthesis gain).**
  The encoder pre-scaled its input by 32768 on top of an analyzer↔synth
  kernel pair that already carries 2^16, so every encoded stream decoded
  65536× too loud — full-scale clipping under FFmpeg — while every
  correlation- and SNR-based gate in the suite (amplitude-invariant by
  construction) stayed green. Verified directly: FFmpeg and this crate's
  decoder both reconstructed a 0.25 sine at RMS 75 (×300 at the first
  frame, ×65536 steady state), and this crate's decoder decodes a
  LAME-encoded reference with exact RMS parity (0.0839 vs 0.0841),
  pinning the defect on the encoder's analysis scale. The analyzer now
  runs at 0.5× input for unity gain (measured 1.000 across amplitudes),
  the psy ATH anchor is recalibrated to the new spectral scale, and the
  scale-defect regression now asserts decoded peak ≈ source peak.
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
  decode at the suite gate (passing for the full ladder: 113.8-120.5 dB
  across tonal, noise, and mid/side material).
- Tonal-material inter-decoder divergence FIXED: FFmpeg's integer requant
  (`l3_unscale`) zero-returns escape-coded lines whose combined shift
  leaves [0, 31]; at the global gains the rate loop produced (gg ~ 250),
  every escape line fell outside that window and decoded as silence in
  FFmpeg (a 0.6/1 kHz tone at 128 kbps: FFmpeg RMS 145 vs ours 27,476).
  The encoder now masks out-of-window escape lines to zero at quantization
  time (encoder, our decoder, and FFmpeg all agree) and the gg search
  treats such plans as non-fitting, rising to gains where the content is
  representable (matching LAME, whose granules for this material sit at
  gg <= ~103). The previously ignored `encoder_tonal_and_mono_scale_defects`
  regression is enabled and passes: tonal mono/stereo now agree with
  FFmpeg at 120.5/120.2 dB.
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
