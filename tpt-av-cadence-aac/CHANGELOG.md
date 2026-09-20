# Changelog

All notable changes to `tpt-av-cadence-aac` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **SBR (Spectral Band Replication) — HE-AAC support.** A new `sbr` module
  ports the reference decoder's enhancement pipeline: SBR header/grid/
  dtdf/invf/envelope/noise/harmonic bitstream parsing with the ten SBR
  Huffman tables, master/derived frequency-table construction (incl. patch
  construction and limiter bands), dequantization with coupled-stereo
  balance handling, envelope estimation, gain calculation with the limiter
  boost, chirp-controlled high-frequency generation, the 5-tap smoothing
  filter, sinusoid addition, and the 64-band QMF analysis/synthesis
  filterbanks (a dedicated 64-point MDCT matching the reference
  transform's semantics, f64 accumulation). Detection of an SBR fill
  element doubles the output rate (implicit signaling) and stages 2048
  samples per channel; the pipeline allocates nothing per frame. The
  filterbank and the frequency-table derivation are regression-tested
  against values produced by an independent build of the reference C
  code; the FATE HE-AAC sample (al_sbr_cm_48_2) decodes at 48 kHz with
  full-length output and ~22 dB SNR versus FFmpeg's decode (gated >=20 dB
  in the conformance test; the residual gap is documented in the README).
  Known limitations: PS data is skipped (HE-AACv2 stays mono); 5.1/7.1
  HE-AAC applies only the first element's SBR payload.

### Fixed

- The data-stream element count is EIGHT bits with a 255 escape byte
  (ISO/IEC 14496-3 Table 4.8), not four bits with a 15 escape; the
  misreading desynchronized every frame containing a non-trivial data
  stream element (a whole class of real-world streams decoded as garbage).
- The coupling element's 4-bit instance tag is consumed before its
  configuration (previously the header parse started one field early).
- Codebooks 5/6 (signed pairs) were decoded through the unsigned-pair path:
  values were read from the wrong value table (positive magnitudes instead of
  signed small values). These books carry no sign bits; the sign is part of
  the value table (`CODEBOOK_VALS_SIGNED_PAIR`).
- Perceptual Noise Substitution bands were phase-inverted: the reference's
  negative noise scalefactor folds into ITS positive MDCT scale, while this
  decoder carries the global −1 inside the IMDCT kernel, so applying the
  negative a second time inverted the noise (whole-file SNR was capped near
  59 dB; now >120 dB).
- A non-common-window channel pair (`common_window == 0`) parsed each
  channel's individual channel stream twice — the second pass consumed the
  following element's bits as spectral data, rejecting valid frames with
  "too many bitstream sections" (and every such pair now keeps its own
  per-channel window sequence for windowing).
- Raw-block framing (`from_config`) decoded only the first raw_data_block of
  a stream; blocks are now parsed successively, with a refill-and-retry path
  for blocks that straddle the frame buffer's refill boundary (a failed
  speculative parse restores the PNS LCG state and the per-channel
  window-shape flags, so the retry decodes from identical state).
- Sign-bit shifts in the spectral decode no longer shift `u32` by 32 when a
  tuple has no nonzero values (debug-build panic).

### Changed

- Removed the `AAC_DEBUG`/`AAC_DUMP`/`AAC_TRACE` environment-gated tracing
  and file-dump scaffolding from the decode path (`env::var` is not
  real-time safe); preallocated the PCM staging buffer at open so `decode()`
  performs no allocation.

### Added

- Program Config Element (PCE) support: channel configuration 0 streams are
  configured from the in-band PCE (channel plan, dynamic `StreamInfo`
  channel count), with per-element order mapping; streams without a PCE
  before their first channel element are rejected as corrupt.
- WAV channel order for the fixed multichannel configurations: bitstream
  element order is front-center first, while the WAV convention puts front
  pairs first; the per-configuration mapping (3.0 through 7.1) is verified
  against FFmpeg with distinct per-channel content. Channel configuration 7
  now correctly reports EIGHT channels (was seven).
- Conformance suite: bundled mono/stereo fixtures compared against FFmpeg
  reference PCM (>100 dB SNR, <=1e-5 peak gate, measured 123.4/123.8 dB),
  deterministic replay after `seek`, mid-stream seek replay, live FFmpeg
  encode/decode round trips at 8-48 kHz across mono/stereo AND 4.0/5.0/5.1/
  7.1 multichannel (all scalefactor-band tables exercised; 122.4-126.1 dB),
  crafted in-band-PCE streams decoded identically to the plain fixtures,
  raw `AudioSpecificConfig` framing decoded bit-identically to the ADTS
  framing (including a 64-byte-read streaming case that forces refill
  straddles), and proptest never-panic coverage over arbitrary and mutated
  streams.

## [0.1.0]

### Added

- Initial AAC-LC (ISO/IEC 14496-3) decoder scaffold.
