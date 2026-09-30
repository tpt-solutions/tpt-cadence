# Changelog

All notable changes to `tpt-cadence` are documented here, grouped by dated
development milestone. Entries before 0.1.0 are organized by date rather than semantic
version; 0.1.0 is the first crates.io release. Format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- Ogg Vorbis encoder (`tpt-av-cadence-vorbis`): `VorbisEncoder::new(sink,
  rate, 1|2 channels, quality 0..=10)` — single 2048-sample block
  size/mode, floor 1 (34 posts fitted and then decoded back through the
  real `Floor1::decode` so the residue is computed against the exact
  curve), residue type 1 over static Huffman-shaped lattice VQ books with
  square-polar stereo coupling, and exact input length via granule
  trimming, with headers round-tripped through `parse_setup` at
  construction. Gates: round-trip SNR, exact odd/short input lengths, and
  FFmpeg agreement (136 dB inter-decoder, 1ch/2ch, 44.1/48 kHz). The floor
  rule blends envelope-tracking with flat noise (alpha 0.7); on the sample
  music fixture ~150 kbps reaches ~23 dB SNR vs libvorbis' 16.7 dB at
  134 kbps. Block switching, per-file adaptive Huffman books, and a real
  psychoacoustic model remain future work (see `todo.md`).
- MP3 intensity stereo, opt-in (`tpt-av-cadence-mp3`):
  `Mp3Encoder::set_intensity_stereo` codes the top in-phase scalefactor
  bands as one mono source plus per-band pan positions carried in the
  right channel's scalefactors. Engages per frame when every granule's top
  bands are strongly in-phase (rho >= 0.9), per window on short-block
  granules, and on every version family — MPEG-1 pans quantize to the
  12-step `PAN` table, the LSF families to the ISO/IEC 13818-3
  quarter-step ladder through a jointly-fitted position/width partition
  search (the right channel's `scalefac_compress` LSB carries the
  intensity scale). FFmpeg cross-check gates: 119-120 dB inter-decoder on
  long-block material, 102.8 dB across mixed long/short granules, 121.4 dB
  on LSF, with per-channel levels within 2% of the plain encode.
- MP3 short-block switching enabled, plus Info/Xing + LAME gapless tags
  (`tpt-av-cadence-mp3`): the previously gated short-block machinery now
  runs on MPEG-1 (attack-driven block-type state machine with zero-line
  stop/bridge granules for decoder-agreement-safe window handover) and on
  MPEG-2/2.5 (one granule per frame, short-row scalefactor partition
  table), gated by FFmpeg cross-checks at 110-123 dB inter-decoder.
  `new_cbr_with_info`/`new_vbr_with_xing` write Info/Xing headers with the
  LAME gapless fields (measured encoder delay, flush padding, seek TOC),
  patched in place after encoding; FFmpeg applies the same trimming our
  decoder does on the tagged streams.
- `silk_NSQ_del_dec` delayed-decision noise-shaping quantization
  (`tpt-av-cadence-opus`): an exact port of `silk/NSQ_del_dec.c`
  (1-4 pruning paths, 40-sample decision delay, per-path warping
  feedback, the voiced subframe-2 tree reset), wired behind
  `SilkEncoder::set_complexity(0..=10)` via the reference's own dispatch.
  Validated by a 64-configuration differential against `decode_core` and
  bit-exact complexity-10 round trips including CBR; on the suite's
  speech fixture complexity 10 improves A-weighted SNR by 9-13 dB while
  shrinking the payload.
- MP3 encoder completions (`tpt-av-cadence-mp3`): full bit-reservoir
  borrowing (`main_data_begin` reach-back, up to 511 banked bytes, byte-
  exact frame tiling for mixed-rate streams); the output-scale defect
  fixed (the analyzer ran at 32768x input on top of an analyzer/synth
  kernel pair carrying 2^16, so every stream decoded 65536x too loud and
  survived every correlation-based gate; the analyzer now measures unity
  gain and the decade gate asserts decoded ~= source peak); MPEG-2/2.5
  (LSF) encoding at 8-160 kbps (one 576-sample granule, 9/17-byte side
  info, 9-bit mixed-radix `scalefac_compress` search); and loudness- and
  quality-targeting per-frame VBR (`Mp3Encoder::new_vbr`, quality 0..=9,
  per-frame bitrate-ladder selection absorbed by the reservoir).
  FFmpeg-oracle gates pass across all three families (110-124 dB
  inter-decoder with unity gain); this supersedes the "long blocks only,
  bit-reservoir-free" scope note on the early MP3 entry below.
- SILK noise-shaping analysis (`tpt-av-cadence-opus`): a new
  `silk::noise_shape` module ports
  `silk_noise_shape_analysis_FLP`, `silk_warped_autocorrelation_FLP` and
  the `silk_NSQ_wrapper_FLP` float-to-fixed conversion — per-subframe
  shaping filters from a *frequency-warped* autocorrelation (order 16,
  5 ms look-ahead, `WARPING_MULTIPLIER` warping, the reference's
  `warped_gain`/`warped_true2monic_coefs`/`limit_coefs` magnitude
  limiting), the smoothed spectral tilt and harmonic shaping gain, and the
  rate/distortion factor `Lambda`. The per-subframe **gains** it produces
  replace the encoder's frame-level "shaping proxy" and are measured
  **+0.8 to +1.2 dB SNR** on synthetic speech across 8–48 kbps (8 kbps
  5.99 -> 6.82, 16 kbps 12.29 -> 13.36, 24 kbps 16.17 -> 17.29, 32 kbps
  19.30 -> 20.45, 48 kbps 24.53 -> 25.70 dB at 16 kHz/20 ms) for +5-8%
  payload, with the encoder's simulated reconstruction still bit-identical
  to the real decoder. New: a frame-RMS-derived `speech_activity_Q8`
  stand-in (which also feeds the analysis's activity-dependent terms), the
  5 ms shaping look-ahead region in the encoder's `x_buf` (zero-filled past
  the frame, which is what the reference reads past the end of a packet),
  and `smlawt` in `sigproc` (see Fixed).
  The shaping filter, tilt, harmonic gain and `Lambda` are computed and
  range-checked but not yet closed into the NSQ's error-feedback loop: both
  that loop and the RD rate term were implemented and measured, and without
  the reference's per-frame rate-control loop they cost 8-29 dB (level
  runaway on non-speech content, payload starvation from the rate term).
  `todo.md` records the measurements and pairs the work with the
  rate-control loop.

- MP3 systematic FFmpeg-oracle conformance matrix (`tpt-av-cadence-mp3`):
  with the official ISO/IEC 11172-4 conformance bitstreams still
  unobtainable (re-verified 2026-09-27: mpg123 SVN `test/` only, no
  client-free access, no mirrors), `tests/ffmpeg_oracle_matrix.rs` now
  synthesizes 28 LAME-encoded streams at test time — MPEG-1/2/2.5 sample
  rates, the 8–320 kbps ladder, mono/stereo/joint modes, transient content,
  reservoir-off, and header surgery (dual-channel mode, flag flips,
  Xing-strip) — and requires each stream to byte-tile exactly per the ISO
  frame-size formula and match FFmpeg's decode at the suite gate
  (>100 dB SNR / <=1e-5 peak), applying LAME gapless-tag alignment where
  the muxer writes one. A companion test independently re-parses the
  corpus's headers and side info and asserts the Layer III feature space
  (all four block types incl. mixed, scfsi, bit-reservoir use, preflag,
  scalefac_scale, subblock gains, both count1 tables, count1-only and
  zero-length granules, mid/side mode_ext) is genuinely exercised, with
  the bundled fixtures byte-tile-checked too. The encoder cross-check
  gained a full 32–320 kbps stereo ladder gated on FFmpeg acceptance and
  FFmpeg-vs-ours PCM agreement of the same bytes — and the ladder
  immediately surfaced two real encoder defects (a ~1e5× output-scale
  error in all materials, and cross-decoder divergence on tonal granules),
  tracked with measurements in `todo.md` behind an `#[ignore]`d regression
  test. Remaining documented oracle gaps: Layer III intensity stereo
  (since covered by the encoder-side FFmpeg cross-check gates — see the
  intensity entry above),
  CRC-protected whole streams (separately covered by `crc_streams.rs`),
  and free-format bitrates.
- Perceptual quality metrics (`tpt-av-cadence-test-utils`): a shared
  `quality` module with A-weighted SNR (IEC 61672 error-spectrum
  weighting via per-segment spectral analysis) and ITU-T P.561-style
  segmental SNR (per-20 ms SNR clamped to [-10, +35] dB, averaged). The
  SILK encoder suite gains a perceptual regression test (A-weighted SNR
  > 10 dB, segmental > 5 dB on shaped speech at 32 kbps), making
  noise-shaping quality — and any future `silk_NSQ_del_dec` work —
  measurable beyond waveform SNR.
- NLSF interpolation search (`tpt-av-cadence-opus`): the SILK encoder
  now implements `silk_find_LPC_FLP`'s second half — for 20 ms frames
  with established prediction state, a second Burg run over the last
  10 ms produces a last-half NLSF vector, and interpolation coefficients
  k = 3..0 are evaluated by filtering the first 10 ms with the
  interpolated filter (`silk_interpolate` between the previous quantized
  NLSF and the last-half NLSF) and comparing first-half residual
  energies; the winning coefficient (or 4 = no interpolation) is
  transmitted, and the encoder builds PredCoef[0] from the interpolated
  NLSF exactly as the decoder reconstructs it. The NSQ's per-subframe
  A_Q12 selection and rewhitening schedule already handle
  `LSF_interpolation_flag` (ported with `nsq_ref`).
- Modified Burg LPC analysis (`tpt-av-cadence-opus`):
  `src/silk/lpc_analysis.rs` ports `silk/float/burg_modified_FLP.c`
  exactly — the incremental correlation-row updates (C_first_row /
  C_last_row / CAf / CAb), the per-order reflection coefficient with the
  `minInvGain` prediction-gain cap, and the residual-energy fallback
  when the cap is hit — replacing the autocorrelation+Schur+k2a stand-in
  in the SILK encoder's LPC analysis (`silk_find_LPC_FLP` structure).
  The reference conditions the analysis on the previous NLSF vector;
  the foundation's variant applies the stability cap via `min_inv_gain`
  (1e-4, ~80 dB max prediction gain) and keeps A2NLSF's own bandwidth
  expansion, both documented. Payloads at fixed bitrates shift slightly
  (different LPC residuals change the NSQ excitation), and the bitrate
  tracking test was rewritten for the now-active per-frame rate control:
  payloads land on the caller's budget (32 kbps mono → ~80 B ±35%,
  48 kbps → 120 B ±35%) with a monotonicity check across budgets,
  replacing the old over-delivery ratio test. The SILK-mode stereo CBR
  total range extends to 128 kbps (per-channel rate control enforces
  the budget).
- Reference 4-band voice-activity detection (`tpt-av-cadence-opus`):
  `src/silk/vad.rs` ports `silk/VAD.c` exactly — the `silk_ana_filt_bank_1`
  two-band allpass filterbank cascade (0-1/1-2/2-4/4-8 kHz bands), the
  differentiator HP filter on the lowest band, per-subframe band energies,
  `silk_VAD_GetNoiseLevels`' inverse-energy noise smoothing with
  fast-initial and high-energy update coefficients, the SNR-based
  `speech_activity_Q8` sigmoid with power scaling, `input_tilt_Q15`, and
  the per-band `input_quality_bands_Q15` sigmoids. The encoder's
  `speech_activity_q8` now comes from the VAD (replacing the smoothed
  frame-RMS stand-in), and `noise_shape_analysis` consumes the real band
  qualities (previously held at the maximum), so the background-SNR
  reduction, LF shaping strength, harmonic HP noise, and Lambda all key
  off the genuine activity/quality measures as the reference intends.
  Supporting helpers ported: `silk_sigm_Q15`, `silk_lin2log`,
  `silk_CLZ_FRAC`/`silk_SQRT_APPROX` (`silk/Inlines.h`),
  `silk_ana_filt_bank_1` (`silk/ana_filt_bank_1.c`). Waveform-SNR floors
  re-baselined fractionally (the genuine activity measure reads lower
  than the stand-in on synthetic speech, enabling the intended
  background-SNR reduction).
- Reference noise-shaping quantizer + per-frame rate control
  (`tpt-av-cadence-opus`): the SILK quality-iteration item is closed.
  `src/silk/nsq_ref.rs` ports `silk/NSQ.c` exactly — `silk_nsq_state`
  (xq/sLTP_shp_Q14/sLPC_Q14/sAR2 shaping states), the two-candidate
  `Lambda`-rate-distortion `silk_noise_shape_quantizer` with the full
  n_AR/n_LF/n_LTP shaping error-feedback loop, the voiced rewhitening
  path, and `silk_nsq_scale_states`' gain-change adjustments — replacing
  the foundation's simplified candidate-window quantizer. The encoder's
  frame encode now runs `silk_encode_frame_FLP`'s per-frame rate-control
  loop: gains are re-quantized per iteration at a bisected `gainMult`
  scale (×3/2 up, ×4/5 down, bounds interpolation, Lambda ×1.5 bump with
  quantizer-offset zeroing when only over-budget attempts are found),
  measured on a scratch range coder (bit deltas are position-
  independent), with per-channel state snapshots rolled back between
  attempts and only the accepted attempt committed. Payloads now land on
  the caller's bitrate budget instead of ~60-100% over; all existing
  bit-exactness, SNR, and integration gates pass unchanged. The port's
  first integration attempt was unstable (inflated payloads, overflow
  aborts) — root cause was `scale_states` consuming the whole frame
  instead of the per-subframe slice; fixed and verified via the
  bit-exactness differential.
- SILK low-bitrate redundancy / FEC (`tpt-av-cadence-opus`):
  `SilkEncoder::set_packet_loss_perc` and
  `OggOpusEncoder::set_packet_loss_perc` implement LBRR encoding — each
  payload carries re-serialized copies of the previous packet's ACTIVE
  coded frames (side-info indices + excitation pulses, plus the stereo
  MS predictor and mid-only flag), written in the decoder's exact
  normal-decode LBRR skip order (per-channel VAD + packet-LBRR flags,
  per-frame LBRR flags with the side's flag gated on its coded/mid-only
  decision, then frame-major / channel-minor with per-channel
  conditional coding chained along the LBRR flags). The LBRR chain is
  self-consistent by construction: it ends exactly at the previous
  packet's last-frame decoder state (`LastGainIndex`, `ec_prev`), so the
  regular frames' conditional decisions are unaffected. Two reference
  behaviors pinned during development: LBRR covers ACTIVE frames only
  (an inactive frame's signal-type symbol has no LBRR encoding — the
  naive copy underflows the type-symbol arithmetic), and the stored
  frames serialize from the CBR retry loop's FINAL attempt so the
  redundancy always matches the emitted payload. Tests: LBRR streams
  decode bit-identically to non-LBRR streams (regular frames untouched)
  with ~1.4x packet growth, DTX+LBRR interop, and the full suite.
- SILK discontinuous transmission (`tpt-av-cadence-opus`):
  `SilkEncoder::set_dtx` and `OggOpusEncoder::new_silk_dtx` implement the
  reference's DTX schedule (`silk_Encode`'s `noSpeechCounter`/`inDTX`
  pair, `NB_SPEECH_FRAMES_BEFORE_DTX` = 10 / `MAX_CONSECUTIVE_DTX` = 20,
  driven by the mid channel's activity): the first 10 inactive frames are
  still coded, and once 20 consecutive inactive frames have passed,
  every-silent packets are emitted as 1-byte packets (TOC byte only) —
  which decoders answer with comfort-noise generation (this crate's
  decoder PLC/CNG paths are already exercised by the conformance suite).
  The input pipeline (resampler, MS transform, history buffering) keeps
  running through skipped packets, so coding resumes seamlessly when
  speech returns; a packet is skipped only when EVERY frame is inactive
  (partial-DTX multi-frame packets are coded in full, since the decoder
  reads per-frame side info for any packet that carries a payload).
  Tests: speech→silence→speech streams show 1-byte packets exactly in
  the deep-silence region (none without DTX, none in the first 20
  inactive frames), exact sample-count recovery, near-silent CNG decode,
  post-gap speech energy, and determinism.
- CBR payload sizing for the SILK encoder (`tpt-av-cadence-opus`):
  `SilkEncoder::set_cbr_bytes` / `set_max_payload_bytes` and
  `OggOpusEncoder::new_silk_cbr` close the SILK CBR-sizing gap. Every
  encoding attempt is wrapped in a snapshot/retry loop: the payload is
  serialized, measured (exact byte length for standalone CBR, range-
  coder `tell()` plus the hybrid redundancy-lookahead headroom for the
  hybrid), and an oversized payload is re-encoded from a full mutable-
  state snapshot at a proportionally reduced working rate (up to 12
  attempts); an undersized payload is padded with zero bytes, which the
  SILK decoder never reads (its range coding has no end-relative raw
  bits), so padding is lossless. `new_silk_cbr` emits constant-size
  packets at the nominal `bitrate·ms/8000`; `new_hybrid` now enforces
  the SILK share with the same mechanism, so the frame-budget guard can
  no longer fire from SILK overshoot. Two subtleties pinned during
  development: the retry snapshot must include the per-channel
  RESAMPLER (its retained inter-call tail otherwise corrupts the
  re-sampled frame on every retry), and the foundation's quantizer has
  a content-dependent minimum payload (~45 B for active speech at
  16 kHz internal, ~20 B for silence) below which ExactBytes sizing
  fails cleanly and MaxBytes sizing degrades best-effort. Tests:
  constant-size mono/stereo CBR streams with decode SNR gates, CBR
  determinism, infeasible-size rejection, and all prior suites green.
- Stereo hybrid encoding (`tpt-av-cadence-opus`): `OggOpusEncoder::new_hybrid`
  now accepts 1 or 2 channels — stereo pairs code through stereo
  adaptive-mid/side SILK (per-channel rate targets split from the total)
  plus the stereo start-band-17 CELT layer on the shared range coder,
  with the TOC stereo bit set and every budget guard unchanged. The
  constructor's rate validation is now per channel (stereo totals range
  up to 2x). Found and fixed a real robustness bug while testing: the
  `finish()`/`Drop` delayed-tail flush loop spun forever when
  `emit_frame` kept failing (e.g. a budget guard error) because
  `emitted_samples` never advanced — both loops are now progress-bound
  and end the stream a few samples short instead of hanging. Documented
  measurement: stereo SILK's natural VBR payload is far larger than the
  nominal rate math (~114/140/165/290 B per 20 ms frame at 8/12/16/40
  kbps per channel — two channels' side info plus the MS predictor
  symbols dominate), so stereo hybrid budgets must be sized from the
  measured payload, which the frame-budget guard enforces by refusing to
  emit frames the decoder would misread. Tests: stereo hybrid SWB/FB
  round trips with exact length recovery, TOC stereo-bit and exact
  frame-size checks, per-channel SNR gates, and the extended validation
  matrix.
- SILK stereo (adaptive mid/side) encoding (`tpt-av-cadence-opus`):
  `SilkEncoder::new_stereo` and `OggOpusEncoder::new_silk(.., channels =
  2)` produce real stereo SILK payloads and `.opus` streams (TOC stereo
  bit set, `nChannelsInternal = 2`). Per frame after resampling, the
  left/right input is converted to adaptive mid/side by
  `stereo::lr_to_ms` — an exact fixed-point mirror of the decoder's
  `ms_to_lr` unmixing in reverse, including the one-sample-delay
  buffering and the 8 ms predictor ramp. The mid/side predictor is
  chosen by least squares over the frame's constant-weight region and
  quantized to the decoder's `STEREO_PRED_QUANT_Q13` table (the joint
  25-symbol index packs the two weights' regions); the quantized
  predictor is then removed with the decoder's own ramped arithmetic, so
  a decode of the emitted indices reconstructs the input mid/side pair.
  Mid-only side skipping is engaged when the side residual's RMS falls
  below the activity gate — the flag is present in the bitstream exactly
  when the decoder's side-VAD gate reads it — and the encoder mirrors
  the decoder's side-channel reset (zeroed synthesis memory, re-seeded
  pitch/gain state, `LastGainIndex` = 10) on the first coded side frame
  after a skipped one. Per-frame and per-channel conditional coding
  follow the decoder's rules exactly: mid independent on frame 0 and
  conditional afterwards; side offset by one frame index (independent
  for frames 0 and 1) with `CODE_INDEPENDENTLY_NO_LTP_SCALING` after a
  skipped frame; per-channel VAD/LBRR prologues and per-channel
  persistent `ec_prev`/`indices` state. All per-channel state moved into
  a `ChannelState` with the analysis chain; the mono path is
  bit-identical (pinned by the existing suite). Tests: stereo Ogg round
  trips at 8/16 kHz internal with exact length recovery, per-channel SNR
  gates and TOC stereo-bit checks; mid-only engagement (near-mono
  packets shrink ~1.7x vs true stereo while decoding both channels at
  >20 dB); stereo pre-skip pinning (mid channel at the signalled
  constant, side within the documented dispersion tolerance);
  determinism and validation.
- Hybrid SILK+CELT encoding (`tpt-av-cadence-opus`): `OggOpusEncoder::new_hybrid`
  writes real mono hybrid `.opus` streams (TOC configs 12–15, 10/20 ms,
  superwideband or fullband) — the third and last Opus mode on the encode
  side. The SILK layer (always 16 kHz internal, the wideband low band)
  writes its symbols first onto a shared range coder, the hybrid
  no-redundancy bit follows, and a `start`-band-17 CELT layer codes the
  ≈6.8 kHz+ region on the same coder, exactly as a hybrid decoder reads
  it. `SilkEncoder::encode_frame_into` exposes the shared-coder
  half (no finalization), and the CELT encoder's bitstream body was
  refactored into a band-windowed, shared-encoder core
  (`encode_frame_core`) used by both the pure-CELT and hybrid paths —
  header bits mirror the decoder's exact conditions (silence only at
  `tell() == 1`, postfilter only at `start == 0`), and every budget gate
  plus the whole allocation arithmetic keys off `8 × whole-frame bytes`,
  which the hybrid packet makes exact by fixing the frame's final byte
  count (SILK share + CELT share, padded through the proven
  raw-bit/clone-verify finalizer) so encoder-side and decoder-side
  budgets match by construction. Measured hybrid pre-skip: 67 samples
  (the SILK low band's delay dominates the composite alignment; the CELT
  high band's ~120-sample delay is the documented compromise). Speech
  round trips at 16 kbps SILK + 24 kbps CELT decode at ~37 dB SNR
  through the real hybrid `OpusDecoder` path (vs ~21 dB SILK-only).
  Tests: SWB/FB × 10/20 ms round trips with exact sample-count recovery
  and TOC/frame-size checks, pre-skip pinning, low-band consistency,
  determinism, and configuration validation. Remaining Opus encoder
  scope: SILK stereo, LBRR/FEC/DTX, CBR SILK sizing, and the SILK
  quality iterations.
- SILK-mode Opus packetization (`tpt-av-cadence-opus`): `SilkEncoder` now
  emits complete RFC 6716 SILK payloads — 10/20 ms single-frame packets
  plus 40/60 ms packets carrying two/three 20 ms frames in the reference's
  intra-packet arrangement (frame 0 coded independently, later frames
  conditionally: subframe-0 gains delta-coded against the previous frame's
  last subframe, delta pitch lags when the previous frame was voiced, no
  LTP-scale symbol, NLSF interpolation factor still transmitted but held
  at 4 = no interpolation; the encoder keeps a decoder-mirror persistent
  `SideInfoIndices` so fields a frame does not code carry across frames
  exactly as `decode_indices` reads them). `OggOpusEncoder::new_silk`
  wires the payloads into real `.opus` streams: mono, TOC configs 0–11
  (NB/MB/WB × 10/20/40/60 ms, single-frame code-0 packets), target-bitrate
  steering of the quantizer SNR (5–64 kbps, VBR packet sizes), and
  per-rate RFC 7845 pre-skip constants measured with aperiodic
  impulse-train round trips (68/65/67 samples at 8/12/16 kHz internal;
  an earlier periodic-signal measurement had produced period-shifted
  representatives of the same alignment — the dispersion caveat and the
  measurement method are documented on `silk_pre_skip`). Tests:
  multi-frame payloads bit-exact through the real `SilkDecoder` with
  voiced, unvoiced, and inactive predecessors so pitch-delta coding is
  exercised in both directions; TOC/parse-back configuration checks;
  20/40/60 ms Ogg end-to-end with exact sample-count recovery, SNR gates,
  and a pre-skip pinning test; bitrate steering; determinism; and
  invalid-configuration rejection. Remaining Opus encoder scope: hybrid
  SILK+CELT, SILK stereo, LBRR/FEC/DTX, CBR payload sizing, and the SILK
  quality iterations (noise shaping + warping, Burg LPC, delayed-decision
  NSQ, pitch lookahead).
- SILK encoder foundation (`tpt-av-cadence-opus`): `SilkEncoder` encodes
  mono 10/20 ms frames at 8/12/16 kHz internal rate into VBR SILK
  payloads, closing the first half of the last major Opus encoder gap.
  Bitstream layer is an exact port of the reference encoder
  (`silk_encode_indices` incl. the VAD/LBRR prologue,
  `silk_encode_pulses` with the min-bits rate-level search,
  `silk_gains_quant` + `silk_lin2log`, `silk_NLSF_encode` with the 4-state
  `silk_NLSF_del_dec_quant` trellis, `silk_VQ_WMat_EC` +
  `silk_quant_LTP_gains`, the `silk_control_SNR` rate tables, and an exact
  fixed-point `silk_A2NLSF`). The excitation is produced by a closed-loop
  forward NSQ that evaluates candidate quantization indices through the
  decoder's own `decode_core` arithmetic (seed-dithered excitation, LTP
  prediction over the re-whitened state, gain/LPC synthesis), so the
  encoder's simulated reconstruction and the real `SilkDecoder` output
  are bit-identical across frames — pinned by round-trip tests at all
  four bandwidth/frame-size combinations, with SNR gates on synthetic
  tonal/speech/noise material and a bitrate-control sweep. Analysis
  simplifications vs libopus are documented in the module docs: no
  noise-shaping filter or warping, autocorrelation+Schur LPC in place of
  Burg, full-resolution correlation pitch search without lookahead, no
  LBRR/DTX/FEC/stereo, and single-frame payloads (Opus/Ogg packetization
  of SILK frames and hybrid mode are the next steps).
- 960/120-sample transform support (GASpecificConfig `frameLengthFlag=1`),
  the last frame-length family used by real AAC-LC streams: the decoder now
  sizes its MDCT, KBD/sine windows, overlap-add geometry, and
  scalefactor-band tables (FFmpeg `ff_aac_num_swb_960`/`_120`,
  `ff_swb_offset_960`/`_120`) from the flag, keeping the reference layout's
  fixed 8×128 short-window coefficient stride. Explicit HE-AAC signaling
  combined with short frames downgrades to core-rate output (the reference
  drops SBR/PS there, and in-band SBR payloads are ignored), matching its
  behavior. Verified against the FATE `al04sf_48` conformance item at
  128.6 dB SNR (peak 3.6e-7) with a per-frame gate in the conformance suite.
- HE-AACv2 Parametric Stereo synthesis (QMF-domain PS stage ported from the
  reference decoder implementation, FFmpeg `aacps.c`/`aacpsdsp_template.c`):
  hybrid analysis/synthesis filterbank, transient-aware all-pass
  decorrelation, IID/ICC mixing with IPD/OPD phase smoothing, and the
  reference's end-of-frame envelope fix-ups. Streams flagged HE-AACv2
  (ASC AOT 29) now open as stereo with the doubled SBR rate instead of
  being rejected, and mono ADTS/raw-AOT-2 streams flip to stereo output on
  their first in-band SBR payload (the reference decoder's "treating HE-AAC
  mono as stereo" behavior) — until a PS header arrives the mono channel
  is duplicated, exactly like the reference. Component-level verification
  compares the Rust port against a standalone build of FFmpeg's
  `ff_ps_apply` (generated tables value-for-value, per-stage pipeline
  state, and final L/R output on four parameter scenarios; fixtures in
  `tpt-av-cadence-aac/tests/data/ps_oracle/`), and a new end-to-end
  fixture (`ps_tone.aac` encoded with a from-source libfdk-aac build)
  decodes at 98.4 dB whole-stream SNR (82-139 dB per frame) against
  FFmpeg's PS-capable decode.
- Opus CELT encoder hardening: `CeltEncoder::try_encode_frame` for
  callers that want explicit PCM validation (length, finiteness, [-1, 1]
  range), `OggOpusEncoder::encode` now rejects non-finite/out-of-range
  samples with a precise error, and encoding after `finish()` is rejected.
  `finish()` is idempotent.
- Non-publishing release preparation: `tools/release_prep.py` validates workspace
  metadata and can generate a local version/changelog patch; the manual
  `release-prep` workflow uploads that patch as an artifact and never commits,
  tags, pushes, or publishes.
- Opus CELT encoder foundation: forward MDCT wired into a working
  `CeltEncoder` (mono or stereo, fullband, CBR, all four CELT frame sizes) with a
  passing encode-then-decode round trip, now including real transient
  detection, short-block MDCT/TF-resolution encoding, an anti-collapse
  decision (verified to measurably reduce pre-echo on transient content vs.
  the previous non-transient-only path), and stereo support (independent
  per-channel band coding via `dual_stereo`, no mid/side or intensity-stereo
  coupling yet — verified end-to-end, including that hard-panned left/right
  content actually decodes distinguishably rather than collapsing to mono).
  This is the first landed piece of the planned Opus encoder (the
  user-confirmed first encoder target); hybrid SILK+CELT encoding, VBR, and
  other frame sizes are not yet implemented.
- WAV, AIFF, and headerless PCM writers (`WavEncoder`, `AiffEncoder`,
  `PcmEncoder`), matching each format's existing decoder in supported bit
  depths (8/16/24/32-bit PCM, 32/64-bit float) and channel counts, behind a
  new shared `Encoder` trait in `tpt-av-cadence-core`. Round-trip tested
  bit-exact against each crate's own decoder.
- FLAC encoder (`FlacEncoder`): fixed-blocksize, CONSTANT/VERBATIM/FIXED-
  predictor subframes with partitioned Rice coding, 1-8 channels, 4-32 bit
  depth. Verified bit-exact both against this crate's own decoder and a live
  FFmpeg decode of its output. LPC subframes and stereo decorrelation are
  not yet implemented (see `todo.md`).
- MP3 encoder (`Mp3Encoder`): MPEG-1 Layer III, fixed CBR bitrate, long
  blocks only, bit-reservoir-free framing. Bitstream validity is verified
  independently via a live FFmpeg decode of its output; active reduced-scope
  mono and independent-stereo end-to-end fidelity gates pass. Psychoacoustic
  tuning, reservoir borrowing, short blocks, and stereo coupling remain out of
  scope.

### Fixed
- `tpt-av-cadence-mp3`'s decoder applied minimp3's non-standard LSF
  intensity-stereo position ladder (integer power-of-two ratios with a 64
  position ceiling) instead of the ISO/IEC 13818-3 semantics FFmpeg
  implements (parity picks the unity side, the other side takes
  `2^(-(sh+1)*j/4)` — quarter-step ratio units — and positions >= 16 mean
  "not intensity"). The path had no fixture coverage before the encoder's
  LSF intensity gate exercised it; decoder and encoder now agree with
  FFmpeg at 121.4 dB inter-decoder.
- MP3: on MPEG-2/2.5 (LSF) material made of *repeated* click transients the
  encoder emitted bare `long -> short` window transitions, whose overlap
  handover our decoder and FFmpeg resolved differently — 22-45 dB
  inter-decoder on alternating click cycles, with and without intensity
  stereo. Cause was the window-sequence state machine's cross-frame
  lookahead disagreeing with the granule-0 detector it was predicting (it
  used the previous frame's baseline energy, and measured one channel where
  the detector measures both), which silently dropped the zero-line stop
  granule a short run must be entered through. Every granule of the repro
  now agrees at 104-122 dB, gated by the new
  `lsf_repeated_transients_agree_with_ffmpeg`.
- The SILK encoder's rate-control loop mis-bracketed its gain-multiplier
  bisection (a broken interpolation formula left over from an earlier
  abandoned integration), silently overshooting bitrate budgets by
  1.2-1.55x. Fixed and verified by direct trace; the SNR quality-gate
  floors were re-measured honestly at the now-honored budgets.
- `tpt-av-cadence-opus`'s SILK `smlawt` truncated its third operand to the
  low 16 bits like `smlawb`; the reference's `silk_SMLAWT` uses the *top*
  16 bits. The pair together decodes the reference's packed LF-shaping
  coefficient pair (`LF_AR_shp` high, `LF_MA_shp` low), so the mistake fed
  the LF feedback loop `LF_MA_shp` twice (loop gain -1.79 instead of
  -0.33) and the reconstruction diverged within one frame. `sigproc` now has
  a unit test pinning the top-half/low-half split.
- The SILK encoder's pitch analysis read out of bounds once `x_buf` grew by
  the 5 ms shaping look-ahead region: `pitch_residual`'s last window and its
  `lpc_analysis_filter` call both assumed `x_buf.len() == ltp_mem + frame`,
  which surfaced as a `STATUS_STACK_BUFFER_OVERRUN` abort in the hybrid
  tests. Both now slice the history+frame region explicitly.
- The SBR extension parser matched the wrong extension id for Parametric
  Stereo (1 instead of the normative 2), so in-band PS payloads were never
  decoded at all; every previous PS test wrote the same wrong id, so the
  bug was invisible until a real HE-AACv2 stream was decoded.
- The PS parameter Huffman decoder assigned canonical codes by symbol
  value within each code length, but the normative codebooks (and FFmpeg's
  `ff_vlc_init_from_lengths`) assign them in table order — several PS
  codebooks list same-length symbols out of numeric order, so every
  long-codeword parameter decoded incorrectly. Only the all-zero-codeword
  unit tests had exercised this path before.
- PS per-envelope delta flags (`bs_dt`) were consumed even when their
  parameter family (IID/ICC) was disabled, shifting every subsequent read
  in the payload; the reference only reads them for enabled families.
- ICC parameters rejected only values above 7 but accepted negatives; the
  reference rejects negative accumulated ICC (unsigned compare). IPD/OPD
  deltas now wrap modulo 8 (reference `MASK` 0x07) instead of failing on
  values above the old 5-bit cap, and their read no longer caps at 9 bits
  (the codebooks contain codes up to 14/17 bits — long codewords used to
  fail the whole PS payload).
- The end-of-frame reference fix-ups were missing: a fake final envelope
  is now appended whenever the parameter run ends before the last QMF
  slot (so synthesis-time interpolation covers the whole frame), and
  20/34-band mode history (`is34bands`/`is34bands_old`) is recomputed at
  end-of-frame from both the IID and ICC modes instead of only inside the
  IID header.
- Reserved SBR extension payloads used `read_bits(count)` with the full
  remaining bit count, hitting a >32-bit debug assertion (and mis-
  skipping in release builds) on any HE-AAC stream whose FIL element
  carried non-SBR extended data.
- `OggOpusEncoder::finish()` was not idempotent (a second call or drop
  path could rewrite EOS bookkeeping).
- PS IPD/OPD parameters shared a single per-envelope delta flag, but the
  normative bitstream carries a SEPARATE `bs_dt` bit for OPD after the IPD
  parameters of each envelope. Every HE-AACv2 payload carrying phase data
  therefore decoded its OPD table from misaligned bits, and payloads whose
  extension-length accounting overflowed failed validation entirely —
  disabling PS and duplicating mono until the next PS header, which
  desynchronized phase rendering for all header-less frames in between.
  This was the root cause of the `al_sbr_ps_04_new` FATE conformance item
  decoding a 12-frame region as uncorrelated garbage; that stream now
  decodes at 129.2 dB whole-stream SNR against FFmpeg (peak error 2.2e-7).
- A failed PS payload parse reset the entire `ParametricStereo` state; the
  reference (`ff_ps_read_data`'s error path) keeps the partially parsed
  header — enable flags, band modes, and envelope geometry survive — and
  only clears `start` and zeroes the parameter arrays. The full reset made
  later header-less frames consume a different number of parameter bits
  than the reference, sustaining desync until the next PS header.
- `Sbr::turnoff` reset the Parametric Stereo context; the reference's
  `sbr_turnoff` leaves PS untouched (synthesis is gated by `ps.start` at
  apply time), so SBR-level turnoffs no longer destroy PS state.
- The 20/34-band PS mode (`is34bands`) is now re-derived only when IID or
  ICC is enabled, matching the reference; a payload disabling both families
  keeps the previous synthesis band mapping.

### Known limitations (tracked in `todo.md`)
- Parametric Stereo synthesis is implemented (see Added above). HE-AACv2
  with explicit AOT 29 signaling decodes as stereo, and mono HE-AAC
  streams with in-band PS flip to stereo; a from-source libfdk-aac
  end-to-end fixture decodes at 98.4 dB whole-stream SNR against FFmpeg
  (per-frame 82-139 dB). Explicit HE-AAC AOT 5 streams whose SBR payloads
  contain PS data still keep mono output (the container explicitly said
  "no PS", matching the reference decoder's behavior of ignoring PS in
  that configuration).
- All eight ISO/IEC AAC-LC FATE multichannel conformance items (CCE/PCE
  coupling) now pass the standard conformance gates (al15 keeps a relaxed
  peak bound for float-ulp noise-fill differences at 105.8 dB). The
  HE-AAC/SBR self-generated fixtures reach roughly 117-133 dB SNR against
  FFmpeg, and the official HE-AACv2 FATE items pass: `al_sbr_ps_04_new` at
  129.2 dB (standard gate) and `al_sbr_ps_06_new` at 56.2 dB whole-stream —
  every frame ≥109 dB except one frame (~1e-2 peak, both channels, a
  transient PS-state divergence at the 20→34-band mode switch) plus
  digital-silence frames where the decoders differ only at ~1e-6. The
  960/120-frame (`frameLengthFlag=1`) transform is now supported too, and
  its FATE item (`al04sf_48`) decodes at 128.6 dB.
- The CELT-only Ogg Opus encoder's CBR storage/allocation mismatch is fixed:
  range-coded output now uses libopus-style fixed-size storage, including
  correct final carry flushing and partial raw-bit placement. Mono and stereo
  frames remain exactly on budget across multiple rates, preventing the
  decoder-side PVQ reallocation that previously caused silent corruption.
- The Ogg Opus writer now signals the measured 120-sample CELT algorithmic
  delay as RFC 7845 pre-skip, offsets all audio granules, flushes delayed tail
  frames, and sets the EOS granule to `input_samples + 120`. Wire-level header
  and granule assertions plus exact-length round trips verify gapless sample
  recovery. SILK/hybrid encoding and psychoacoustic tuning remain out of scope.
- Broader official ISO/IEC AAC and MP3 conformance vector suites are not
  obtainable/integrated; AAC and MP3 correctness currently rest on FFmpeg-
  reference comparison rather than official test vectors.
- MP3 encoder produces valid, independently-FFmpeg-decodable bitstreams. The
  analysis polyphase path matches shine's reverse circular-buffer fill;
  forward MDCT/antialias/change-sign precompensation, escape-Huffman pair
  orientation, count1 zero-width handling, MPEG-1 stereo side-info field
  order, and mono/stereo synthesis state now have regression coverage.
  Active mono and independent-stereo end-to-end fidelity gates pass within
  the reduced flat-gain/no-reservoir encoder scope. Psychoacoustic tuning,
  bit-reservoir borrowing, and full feature-set expansion remain out of scope.
- Encoders beyond the Opus CELT foundation, the WAV/AIFF/PCM writers, the
  FLAC encoder, and the MP3 encoder above (Vorbis, AAC) are not yet started;
  AAC encoding is on hold pending a patent-licensing decision.

## 2026-09-21 — CLI, CELT encoder groundwork, panic-safety audit

### Added
- `tpt-av-cadence-cli` (`cadence` binary): auto-detects format by extension
  (with content-sniffing to distinguish Ogg Vorbis from Ogg Opus), and
  provides `info` and `decode` subcommands, including transcode-to-WAV via a
  minimal hand-rolled 16-bit PCM WAV writer.
- `examples/decode.rs` added to every decoder crate (WAV, AIFF, FLAC, MP3,
  PCM) for quick per-crate usage reference.
- Initial `CeltEncoder` scaffolding (mono/fullband/non-transient/CBR/20 ms)
  and a promoted-to-production forward MDCT.

### Fixed
- Workspace-wide panic-safety audit (~230+ sites traced): fixed a
  hybrid-mode subtraction underflow in the Opus decoder on a crafted small
  packet, a missing spec-mandated clamp in the Vorbis Floor1 decode that
  could overflow on crafted amplitude deltas, and an unbounded CCE
  gain-array index in the AAC decoder reachable from a malformed stream.
- AAC/SBR decode could overflow a 1 MiB default thread stack (e.g. a plain
  Windows `main()`) because the `Sbr` struct's large fixed-size tables were
  stored inline in `AacDecoder` rather than boxed; boxed the offending
  fields, cutting the measured release-build minimum stack from ~449 KB to
  ~65 KB.
- FLAC decoder removed unconditional per-frame debug `eprintln!` calls from
  the hot decode path (violated the allocation/lock-free real-time-safety
  contract; discovered via the new CLI).
- CELT `quant_band` collapse-mask bug using a stale pre-time-divide bit
  width, closing the residual 07/08/09 PCM SNR gap left after the hybrid
  fold refactor.

## 2026-09-20 — Opus hybrid fold refactor, shared Ogg crate, real-time safety

### Added
- Shared `tpt-av-cadence-ogg` crate: the Ogg page-parsing layer moved out of
  the Vorbis crate so Opus and Vorbis (and future formats) share one
  implementation.
- `cargo-fuzz` targets and per-crate fuzz/property-test suites across the
  workspace.

### Changed
- Completed the Opus "hybrid fold" refactor: band recursion now uses
  open-ended slices into the shared per-channel norm buffer (mirroring
  libopus's raw-pointer semantics) instead of exact-width slices that
  panicked at band/split boundaries on some vectors.
- Restored allocation-free Opus decode: removed three per-frame `Vec`
  allocations from `compute_allocation`, cached debug-flag environment
  lookups once at construction instead of per band/call, and replaced a
  mode-transition crossfade's heap `to_vec()` with a stack array.

### Result
- All 12 official RFC 6716 Opus test vectors decode with 100% `final_range`
  match on every packet (vectors 02-04 bit-exact PCM; others 37-110 dB SNR
  vs. a live libopus 1.5.2 oracle).

## 2026-09-18/19 — MP3, Opus/CELT/SILK, and Vorbis reach working/conformant status

### Added
- **MP3 decoder** (`tpt-av-cadence-mp3`): Huffman decoding, polyphase
  filterbank, joint stereo, bit-reservoir handling, CRC-protected frame
  support. Ten-stream FFmpeg PCM conformance at 118.7-119.4 dB SNR
  (<=1e-5 peak error); allocation-free `decode()` verified by a
  counting-allocator test. Official ISO/IEC 11172-4 conformance vectors are
  not freely obtainable, so conformance rests on the FFmpeg-oracle
  comparison plus structural/replay/robustness/CRC test suites.
- **Vorbis decoder** (`tpt-av-cadence-vorbis`): MDCT-based Ogg Vorbis
  decode, conformance-tested against FFmpeg at 136-138 dB SNR across six
  bundled fixtures (mono/stereo, 32/44.1/48 kHz, multiple quality levels),
  sample-exact lengths, bit-identical `seek(0)` replay, and exact
  mid-stream seek rejoin.
- **Opus decoder** (`tpt-av-cadence-opus`): range coder (RFC 6716 §4.1/§5.1),
  CELT decoder, SILK decoder, and hybrid SILK+CELT integration, wired into a
  full top-level `OpusDecoder` state machine (mode transitions, redundancy
  frames, DTX/PLC). RFC 7845 Ogg Opus container support
  (`OpusHead`/`OpusTags`, pre-skip/end-trim handling, seek).
- Per-crate README/CHANGELOG documentation added across the workspace.

### Fixed
- Root-caused and fixed the CELT `final_range` entropy-desync bug: raw-bit
  reads in the range coder never advanced the total-bits counter used by
  the CELT bit allocator, silently under-budgeting every allocation
  decision downstream of a postfilter-bearing frame. Found via a byte-exact
  trace against a locally built libopus 1.5.2 oracle; all 6 CELT-containing
  test vectors went from 0-30% to 100% `final_range` match.
- Fixed a SILK multi-subframe (40/60 ms payload) decode bug; 3 of 12 RFC
  6716 vectors reached bit-exact `final_range` and PCM match.
- Fixed raw-bit accounting in the range coder's `tell()`.
- Fixed AAC windowing/overlap-add regressions surfaced while building out
  the MP3/Opus decoders in parallel.

## 2026-09-14/17 — Project bootstrap; WAV/AIFF/PCM/FLAC and AAC-LC/SBR land

### Added
- Workspace scaffolding: `tpt-av-cadence-core` (`Decoder` trait,
  `StreamInfo`, error types), `tpt-av-cadence-test-utils` (FFmpeg oracle
  harness, fuzz helpers, MD5), CI (`cargo test` matrix + `cargo-deny`
  license audit), dual MIT/Apache-2.0 licensing.
- **WAV decoder**: RIFF chunk parsing, PCM/IEEE-float, 8/16/24/32-bit,
  extensible fmt chunk, bit-exact conformance tests.
- **AIFF decoder**: big-endian IFF chunk parsing, AIFF/AIFC (NONE/twos/sowt/
  FL32/FL64/in24/ni24), 80-bit extended sample rates, bit-exact vs. FFmpeg.
- **Raw PCM decoder**: headerless int8/16/24/32 and f32/f64, both byte
  orders.
- **FLAC decoder**: bit-exact conformance vs. FFmpeg across bundled
  subset/uncommon fixtures.
- **AAC-LC decoder** (`tpt-av-cadence-aac`), built from the ISO/IEC 14496-3
  spec: full profile support including PNS, M/S stereo, TNS, PCE-based
  channel configurations (mono through 7.1), CCE coupling channels, and
  ADTS/raw framing. FFmpeg round-trip conformance >120 dB SNR across
  sample rates and channel layouts; 4 official ISO/IEC FATE-mirrored
  conformance items pass at full fidelity (121-129 dB).
- **SBR (HE-AAC) decoding**: full bitstream parsing, frequency-table
  derivation, dequantization, envelope/gain calculation, HF generation, and
  a 64-band QMF analysis/synthesis pair, verified stage-by-stage against an
  independent reference build. See Known Limitations for its residual
  fidelity gap.

### Fixed
- Three independent AAC-LC bugs found via NumPy-assisted coefficient
  bisection and FFmpeg source audit: codebooks 5/6 decoded through the
  wrong (unsigned) branch, PNS noise phase-inverted, and non-common-window
  CPEs double-parsed (corrupting subsequent elements).
- A data-stream-element count field misread as 4 bits instead of 8,
  desynchronizing any ADTS stream containing a non-trivial DSE.
- Fixed configuration 7 to carry 8 channels (7.1) instead of 7.

[Unreleased]: https://github.com/tpt-solutions/tpt-cadence
