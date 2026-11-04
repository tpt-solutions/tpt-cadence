# Changelog

All notable changes to `tpt-av-cadence-aac` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **Parametric stereo (HE-AACv2) synthesis** (`sbr/ps.rs`, `sbr/ps_synth.rs`):
  IID/ICC/IPD/OPD parsing and QMF-domain synthesis. Explicit AOT 29 streams and
  implicitly signaled mono-core streams (first in-band PS payload) output stereo.
  Validated against FFmpeg with `al_sbr_ps_04_new` at 129.2 dB whole-stream and
  `al_sbr_ps_06_new` at a reduced gate (about 56 dB; one residual frame, see
  `todo.md`).
- 960/120-sample frames (`frameLengthFlag`): the ASC's GASpecificConfig flag
  selects the short transform and scalefactor-band tables. Like the reference,
  960-frame streams carry no SBR/PS. Conformance item `al04sf_48` now decodes.
- Explicit HE-AAC `AudioSpecificConfig` signaling: AOT 5 hierarchical configs
  now preserve the AAC-LC core rate, expose the SBR extension/output rate,
  validate `sbrPresentFlag` and reserved frequency indices, and activate the
  existing SBR synthesis path from the first raw block. AOT 29 metadata is
  parsed and identifies PS signaling; PS synthesis runs for those streams.

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
  full-length output. The sign fix listed under Fixed closed its original
  ~22 dB SNR gap.
  Known limitation (since resolved in part, see parametric stereo above):
  the PS path is synthesized, but `al_sbr_ps_06_new` still has one residual
  frame.

### Fixed

- SBR grid border underflow panic on malformed VARVAR/FIXVAR frames: trailing
  `t_env` borders now use `checked_sub` and return an error. Fuzz regression
  in `tests/fuzz_regressions.rs`.
- Channel elements are now processed per element (BEFORE_TNS coupling, TNS on
  both channels, BETWEEN_TNS coupling, both IMDCTs, AFTER_IMDCT coupling). A
  CPE's coupling no longer spills onto the next element or the LFE. Conformance
  item `al07` now passes its gate.
- Refill-retried block parses are idempotent: noise LCG, channel and CCE-scratch
  window-shape state, coupled flags, and `window_seq_prev` are snapshotted and
  restored per attempt, so a CCE is no longer applied twice. `al06`, `al15`,
  and `al22` now pass their gates.
- The CCE scratch channel no longer swaps with, and corrupts, real channel 0's
  overlap-add state. PCE SIDE/BACK positions are split into separate rows.
- CCE coupling targets are matched by PCE declaration-order index rather than
  the raw element instance tag, matching the reference for PCEs whose element
  order differs from output order.
- Implicit PS also engages for stereo-signaled (channel config 2) streams whose
  blocks carry a single SCE. Staging under `Order::Element` emits the PS-expanded
  second channel, so the caller's interleaving stays aligned.
- `BitReader::set_pos` no longer leaves a stale `overread` flag after correcting
  a rounded-up read. Previously this rejected valid frames as corrupt.
- SBR window-table sign bug: fixing it closed the ~22 dB fidelity gap on the
  FATE HE-AAC sample.
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
