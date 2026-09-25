# AAC test data

- `tone.aac` — 1 s mono 44.1 kHz AAC-LC ADTS file, encoded with FFmpeg's
  native encoder from a deterministic 1 kHz sine WAV.
- `tone_ref.f32` — FFmpeg's decode of `tone.aac` as raw interleaved f32le
  (reference implementation output for the comparison harness).
- `test.aac` — 1 s stereo 44.1 kHz AAC-LC ADTS file, encoded with FFmpeg's
  native encoder from a deterministic two-tone test WAV (generated in-test by
  the conformance suite).
- `test_ref.f32` — FFmpeg's decode of `test.aac` as raw interleaved f32le
  (reference implementation output for the comparison harness).

The conformance suite (`tests/conformance.rs`) requires >100 dB whole-stream
SNR and <=1e-5 peak error against these references — a tolerance-based
float comparison, not bit-exact equality.

- `he_aac_sbr_overread_regression.aac` — 2.1 s stereo 48 kHz HE-AAC (SBR)
  ADTS stream, encoded with `libfdk-aac` (`fdkaac -p 5 -b 128000 -f 2`,
  built from upstream source — the distro-packaged `libfdk-aac`/`fdkaac`
  reject HE-AAC encode profiles) from deterministic pseudorandom
  (`random.seed(42)`) stereo noise. This crate has no HE-AAC encoder of its
  own and the official ISO/FATE HE-AAC conformance samples aren't
  redistributable, so this is a from-scratch repro built specifically to
  catch a real bug: `BitReader::set_pos` used to leave a stale `overread`
  flag set after correcting a deliberate rounded-up-to-a-byte read back to
  a valid position (used only by the FIL/SBR extension payload capture in
  `decoder.rs`), which rejected this stream's 48th ADTS frame — and every
  frame after it — as corrupt even though nothing was wrong. Noise-like
  content (rather than a tone) was needed to reproduce it: it fills SBR
  envelope/noise payloads enough to leave little trailing padding in the
  frame, which is what triggers the rounded-up capture actually touching
  the buffer's true end. See `tests/conformance.rs`'s
  `he_aac_sbr_stream_with_frame_end_aligned_fil_element_decodes_without_error`.

- `sbr_fidelity_tone.aac` / `sbr_fidelity_tone_ref.f32` — 1 s stereo 48 kHz
  HE-AAC (SBR), same `libfdk-aac` toolchain as above, from a deterministic
  two-tone WAV (440/660 Hz). Locks in the fix for the real root cause of
  this project's long-standing ~18-23 dB HE-AAC/SBR fidelity gap: two of
  `SBR_QMF_WINDOW_US`'s 640 entries (indices 384 and 512) had the wrong
  sign. `sbr_fidelity_tone_ref.f32` is FFmpeg n7.1's decode of the `.aac`
  file as raw interleaved f32le (same reference-generation convention as
  `tone_ref.f32`/`test_ref.f32` above). See
  `tests/conformance.rs`'s `he_aac_sbr_fidelity_matches_reference_at_high_snr`
  (gated at >80 dB; the fix itself measured ~117-126 dB on this and other
  self-generated fixtures, vs. ~18-23 dB before) and `todo.md`'s AAC SBR
  session log for how this was found — tracing this exact fixture's QMF
  analysis output frame-by-frame against a live FFmpeg build.

- `sbr_multichannel_5_1.aac` / `sbr_multichannel_5_1_ref.f32` — 1 s 5.1
  (6-channel) 48 kHz HE-AAC (SBR), same `libfdk-aac` toolchain, from a
  deterministic six-tone WAV (one tone per channel, 220 Hz in 110 Hz
  steps). Locks in the fix for a real multichannel correctness bug: the
  decoder used to keep exactly one shared `Sbr` context for the whole
  stream, so in a frame with more than one SBR-carrying channel element
  (this stream's front pair, center, and side pair each carry their own
  SBR payload; only the LFE doesn't), only the *last* element processed
  kept valid state — every earlier element's upper-half (samples
  1024-2047) output was whatever the last element's QMF synthesis had
  left there, i.e. uncorrelated noise (~-2 dB SNR vs FFmpeg), not simply
  "SBR disabled". See `tests/conformance.rs`'s
  `multichannel_he_aac_sbr_matches_reference_on_every_channel` (gated at
  >80 dB per channel; measured ~117-133 dB post-fix) and `todo.md`'s AAC
  SBR session log.

- `ps_tone.aac` / `ps_tone_ref.f32` — 1 s HE-AACv2 (Parametric Stereo)
  ADTS stream, 32 kbps, encoded with a from-source `libfdk-aac` v2.0.2
  build (mingw-w64 GCC, same toolchain convention as the SBR fixtures
  above) from a deterministic stereo two-tone (440/660 Hz, 0.8 L/R level
  tilt) at the 24 kHz core rate; generator source in
  `tools/ps_fixture_gen.c`. The ADTS header carries channel
  configuration 1 (mono SCE) and a 12 kHz core rate — PS presence is only
  discoverable in-band, which is exactly the case this fixture locks in:
  the decoder must flip to stereo on the first in-band SBR payload and
  synthesize the PS stereo image. `ps_tone_ref.f32` is FFmpeg's decode of
  the same file as raw interleaved f32le (2023-12-28 gyan.dev full
  build). See `tests/conformance.rs`'s
  `heaacv2_ps_stream_matches_reference_decode` (gated at >80 dB;
  measured 98.4 dB whole-stream, 82-139 dB per frame).

- `ps_oracle/` — reference dumps from a standalone build of FFmpeg
  n7.1's Parametric Stereo synthesis (`ff_ps_apply`) and parameter
  parser (`ff_ps_read_data`), used to verify the Rust port at the
  component level without needing an encoder:
  - `table_*.f32` — every generated PS table (allpass fractions, phase
    rotation, HA/HB mixing LUTs, pd smoothing, hybrid filter prototypes),
    dumped from `ps_tableinit()` and compared value-for-value against the
    Rust table generator (exact except HB, which differs by ≤1 f32 ulp
    between libms).
  - `a_20band_ipd_last.f32`, `b_34band_ipd_last.f32`,
    `c_10band_baseline_last.f32`, `d_modeswitch_last.f32` — last-frame
    L/R QMF output for four parameter scenarios (20-band fine with
    ipd/opd, 34-band, 10-band coarse baseline without ipd/opd, and a
    live 20→34 band-mode switch), each 8 frames of deterministic
    pseudo-random QMF input. Compared per-element (≤2e-4; most values
    bit-exact).
  - `stages_case_a.bin` — per-stage pipeline state (after hybrid
    analysis, after decorrelation, after stereo processing, per frame)
    for case A, used to localize any future regression to a single
    synthesis stage.
  Build sources (stub headers + instrumented `aacps.c` copy + harnesses)
  in `tools/ps_oracle_build/`, `tools/ps_harness.c`,
  `tools/ps_read_oracle.c`.
