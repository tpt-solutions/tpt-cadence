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
