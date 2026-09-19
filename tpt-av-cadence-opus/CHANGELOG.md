# Changelog

All notable changes to `tpt-av-cadence-opus` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Opus packet parser (TOC byte, frame framing) and range coder.
- Full CELT decoder (unit-tested).
- Full SILK decoder (unit-tested).
- Full top-level `OpusDecoder` (`src/decoder.rs`): a port of libopus's
  `opus_decoder.c` state machine holding one SILK and one CELT decoder
  with the cross-packet state (`mode`/`prev_mode`, `prev_redundancy`,
  `rangeFinal`) that drives SILK-only, CELT-only, and hybrid packets,
  mode-transition crossfades, the 5 ms CELT redundancy frames, hybrid
  low-band mixing, and packet-loss concealment, exactly as the reference
  orders them. `range_final()` exposes the per-packet range-coder state
  recorded in the official test vectors.
- `RangeDecoder::shrink_storage`: the `dec.storage -= n` operation used
  when a frame tail holds CELT redundancy raw bits.
- The conformance harness now decodes all 12 official RFC 6716 vectors
  end-to-end through `OpusDecoder` (no more per-mode runs), compares PCM
  against the bundled `.dec` files, and — when `OPUS_ORACLE_PCM_DIR`
  points at a live libopus 1.5.2 decode — against the reference decoder
  itself.
- Ogg Opus (RFC 7845) container support (`src/ogg_opus.rs`):
  `OpusHead` parsing (mapping families 0/1, trivial 1–2 channel
  mappings), `OpusTags` validation, granule bookkeeping with pre-skip and
  per-completing-packet end trim, Q7.8 output-gain application, and the
  `OggOpusDecoder`/`OggOpusReader` pair implementing the core
  [`Decoder`]/[`FormatReader`] traits (decode-and-discard seek; only the
  first link of a chained stream). Depends on the new shared
  `tpt-av-cadence-ogg` container crate.
- `tests/ogg_opus.rs`: self-contained container tests (pre-skip/end-trim
  counts, determinism across buffer sizes, seek-0 replay, mid-stream seek
  continuity, unseekable-source seek rejection) plus an `#[ignore]`d test
  muxing an official SILK-only vector's packets into Ogg pages and
  checking bit-exact PCM against the bundled `.dec`.

### Fixed

- `OpusDecoder::decode_frame` no longer allocates on redundancy/transition
  frames (three `Vec::to_vec` crossfade copies replaced with fixed-size
  buffers), restoring the crate's allocation-free `decode()` contract for
  the new `Decoder` trait impl.

- CELT spectral-LCG seed (`CeltDecoder::new`): the decoder was born with
  `rng = 1_000_000` where libopus's `OPUS_CLEAR`-based init starts it at
  zero, corrupting every noise-fill and folding decision on the first
  frames after init/reset.
- Hybrid band folding with `start_band == 17`: the fold-source and
  fold-output regions overlap at `i == start + 1`; the reference reads
  the source before the output is written (or routes it through the
  scratch buffer), this port now snapshots the source, fixing an
  out-of-bounds panic on testvector06.
- CELT→hybrid mode transitions: the 5 ms concealment frame decoded in the
  outgoing CELT mode (and the 2.5 ms copy + `smooth_fade` from it) was
  missing; testvector10's once-per-second hybrid packets decoded with
  their first 2.5 ms silent (31 dB → 105.6 dB SNR vs libopus).
- Energy prediction `prev[]` update now uses the reference's
  left-associated `prev = prev + q - beta*q` rounding.
- `comb_filter_const` and `renormalise_vector`'s energy now reproduce the
  reference's runtime-dispatched SSE accumulation orders (4-lane partial
  sums with the `(s0+s2)+(s1+s3)` horizontal fold, and the
  `(x + g10·x[-T]) + (g11-term + g12-term)` grouping), including the SSE
  kernel's unwritten `n % 4` tail samples.

### Conformance (all 12 official RFC 6716 vectors)

- `final_range`: **100% match on every packet of every vector**
  (16,073 packets) — the entropy decode is bit-exact against the
  encoder-recorded range-coder state.
- Vectors 02–04 (SILK-only): bit-exact PCM against the bundled `.dec`.
- Versus a live libopus 1.5.2 decode: 91–110 dB SNR on vectors
  01/05/06/10/11/12; vectors 07/08/09 measure 37–56 dB — the accepted
  float-ULP gap (libopus's `exp` and `celt_inner_prod` accumulate in a
  platform-specific SIMD order the scalar Rust port does not replicate;
  the divergence only becomes measurable on near-silent passages).
  `final_range` remains the conformance contract and is 100% everywhere.

### Known issues

- PCM SNR versus the bundled `.dec` files stays low on the hybrid-only
  vectors (05/06) and mixed vector12 — those `.dec` files were produced
  by the 2012-era RFC reference decoder and differ from modern libopus
  at the same level; the oracle comparison above is the authoritative
  signal.
