# tpt-cadence — Project TODO

Tracks all tasks for the whole project, organized by phase. See `DESIGN.md` for full design rationale.

Status snapshot (workspace totals are historical; MP3 revalidated separately): `cargo deny` currently fails against deny.toml's deprecated keys with the newest cargo-deny — config migration pending, and MP3 now passes 36 tests including the doc test and required FFmpeg 7.1 comparisons of all ten bundled streams (>100 dB SNR, <=1e-5 peak error); MP3 strict Clippy is clean; broader feature/bit-exact conformance and the full safety audit remain unresolved. Historically, ~182 tests passed, including bit-exact conformance suites for WAV, AIFF, and FLAC (the FLAC suite MD5-verifies against the official IETF decoder testbench vectors, CC0, bundled under `tpt-av-cadence-flac/tests/data/`). The Opus crate now has 139 unit tests (packet/range coder + the full CELT decoder + the new packet-to-CELT wiring + the SILK modules so far: tables, side-info indices, stereo mid/side prediction, gains dequant, NLSF decode/NLSF2A, pitch lag + LTP codebook lookup, and excitation decode with the seed-dithered reconstruction) plus an `#[ignore]`d conformance test against the official RFC 6716 test vectors, which found a real bug in the packet parser (fixed) and a residual, not-yet-root-caused bug in the CELT decoder itself (see the CELT status section below) — that decoder bug is the reason Opus isn't yet counted toward bit-exact conformance the way WAV/AIFF/FLAC are.

## Phase 0 — Project Setup & Governance

- [x] Initialize git repository
- [x] Create workspace `Cargo.toml` (resolver "2", members list, `[workspace.package]` with `license = "MIT OR Apache-2.0"`, edition 2021, rust-version 1.75)
- [x] Add `LICENSE-MIT` and `LICENSE-APACHE` (dual license)
- [x] Write `README.md` (vision, ecosystem table, quickstart)
- [x] Write `DESIGN.md` (carried over spec.txt content as the living design doc; spec.txt removed)
- [x] Create `deny.toml` (cargo-deny license allow/deny lists per spec §8)
- [x] Set up CI pipeline (build/test matrix + `cargo-deny` license audit job) — `.github/workflows/ci.yml`
- [x] Write `CONTRIBUTING.md` (dual-license contribution terms, conformance-test requirement for new decoders)

## Phase 1 — Foundation

- [x] Scaffold `tpt-av-cadence-core`: `Decoder` trait, `StreamInfo`, `Format`/`SampleFormat`/`ChannelLayout` enums, `CadenceError` enum (+ `ByteSource`/`BufferedSource` plumbing and sample-conversion helpers in core)
- [x] Scaffold `tpt-av-cadence-pcm`: raw headerless PCM reader (int8/16/24/32 + f32/f64, both byte orders)
- [x] Scaffold `tpt-av-cadence-wav`: RIFF chunk parser (`reader.rs`), PCM/IEEE Float decoder (`decoder.rs`) — 8/16/24/32-bit int + 32/64-bit float, mono/stereo/any channel count
- [x] Scaffold `tpt-av-cadence-test-utils`: FFmpeg subprocess comparison harness (`reference.rs`), fuzz helpers (`fuzz.rs`), RFC 1321 MD5 (`md5.rs`)
- [x] Bit-exact conformance tests for WAV (all bit depths, mono/stereo/multichannel, float, extensible fmt chunk, odd chunk sizes, seek, streaming sentinel)
- [x] Wire up `cargo-deny` CI enforcement for a pure MIT/Apache dependency tree

## Phase 2 — Lossless & Migration

- [x] Implement `tpt-av-cadence-aiff` (big-endian IFF chunk parser, AIFF/AIFC decode: NONE/twos/sowt/FL32/FL64/in24/ni24, 80-bit extended sample rates, SSND offsets)
- [ ] Implement `tpt-av-cadence-aac` from scratch (ISO/IEC 14496-3), replacing the kinetix migration plan — **IN PROGRESS (started this session, see status below)**
- [x] Implement `tpt-av-cadence-flac`: stream/frame parser, subframe types (Constant/Verbatim/Fixed/LPC), Rice coding, LPC prediction, top-level `Decoder` impl (stereo decorrelation, wasted bits, CRC-8/CRC-16, resync, decode-and-discard seek)

- [x] Conformance tests against the official IETF FLAC decoder testbench (subset/uncommon vectors; MD5-verified; plus an in-tree reference encoder covering mono, 4/8ch, variable blocksizes, forced Rice escapes, Rice2, 8–32 bit depths, wide UTF-8 frame numbers, wasted bits, and every stereo mode)
- [ ] Conformance tests against ITU-T AAC reference vectors (blocked on the AAC migration)

### AAC-LC from scratch — handoff status (next steps)

Progress (latest session): element order FIXED (global_gain before ics_info);
Mdct FIXED + TDAC-VERIFIED (full 2048/256-point synthesis, kernel
cos(π/(2M)(n+M/2+½)(2k+1)), scale −1/M; PR test max_err 4.8e-8); spectral
decode REWRITTEN to FFmpeg packed-idx semantics (VLC symbol →
CODEBOOK_IDX_02/_4/_6/_8/_10[book][symbol] → dims/vals/signs per
VMUL4/VMUL4S/VMUL2/VMUL2S in aacdec_float.c); sign bits MSB-aligned; infinite
loop fixed (zero-length section + overread guards); windows [half|rev(half)],
sine-half sin(π(n+½)/2M), cumulative KBD α=4 long / α=6 short.

REMAINING BUG: decoded audio still wrong (~2^15 too loud, uncorrelated with
the reference). Coefficient dump for a 1 kHz tone frame shows
|coef| ≈ 155132 = |q|^{4/3}·2^(sfo/4), sfo ≈ 69 — first verify the dequant
scale/sign convention end-to-end (my Mdct carries −1/M; my sf is +2^(sfo/4);
FFmpeg sf = −pow2sf_tab[sfo+200] with pow2sf_tab[i] = 2^((i−200)/4) and its
av_tx scale is +1/1024 — resolve which extra sign/scale differs), then
re-check the packed-idx sign application for books 3,4 / 7,8 / 11 (nnz vs
mask semantics differ per book; mirror aacdec_proc_template.c case 2/3/
default exactly). Debug findings (latest): element order and FIL/ADTS framing are NOW CORRECT —
frame 1 parses as FIL(count=15, SBR-like payload) + SCE(gg=154, seq=1
LONG_START, max_sfb=47, bands 10/11/6) which is plausible for the encoded
onset. The decode bug is narrowed to the QUANTIZED VALUES: dumped coefficient
155132.33 = 13.375·2^13.5, but the valid table value is 13.3905 (12^{4/3}) —
a 0.1% mismatch suggests the packed-idx → value path is ALMOST right but the
value table indexing or the sign/escape bits are slightly off. Also compare
with FFmpeg decode: ref frame-1 output ≈ 0 (priming) while mine is ±20000.

COEFFICIENT VALUES VERIFIED EXACT (final check): frame-1 coefficients
155132.33 = 12^{4/3}·2^{13.5} exactly — the dequant, scalefactors, and
huffman/parse are all CORRECT. The remaining audio bug is therefore in the
SYNTHESIS KERNEL CONVENTION: my kernel is cos(π/2048·(n + 512.5)·(2k+1))
with scale −1/1024; the ISO AAC IMDCT per the spec may instead use
n_start = ½ (no N/4 shift), i.e. cos(π/2048·(n + ½)·(2k+1)), or another
shift — test candidate kernels by synthesizing frame 1 in numpy (coefficients
known-correct) and comparing y[0..1024] against ffmpeg's ref[0:1024] (≈ 0,
the priming region) — the correct kernel gives ≈ 0 there. Candidates:
(a) shift 512.5 (current), (b) shift ½, (c) shift 1536.5, (d) scale −2/2048,
(e) sign flip. Also verify the window: [half_prev | rev(half_cur)] with
half = sin(π(n+½)/2048) — try the alternative full-length form
sin(π(n+½)/1024) over [0..2048) (peaks at 1.0 mid-window) if the kernel is
confirmed. Once frame 1 matches ref[0:1024] ≈ 0, the rest follows.

WINDOW SEQUENCES — definitive finding (from FFmpeg imdct_and_windowing,
which I have ported but must now verify field-by-field):
- LONG_START (cur = LONG_START after long): FFmpeg takes the LONG lap
  (vector_fmul_window with lwindow_prev over the full 1024) and its saved
  update copies buf[512..1024] UNWINDOWED (w = 1.0 over the whole second
  half of the synthesis!). So the LONG_START window = [long-left(prev
  shape) 1024 | ONES 1024].
- LONG_STOP (cur = LONG_STOP after EIGHT_SHORT): out[0..448] = saved
  passthrough (window 0 there — suppresses the frame's own synthesis),
  out[448..576] = short-shaped lap, out[576..1024] = buf UNWINDOWED (w = 1),
  saved = buf[512..1024] raw. So LONG_STOP window = [ZEROS 448 |
  short-transition 128 | ONES 448 | long-right 1024].
- EIGHT_SHORT: out[448..1024] from short-window laps (64-granularity,
  prev shape for the first lap, cur for the rest), out[0..448] = saved
  passthrough; saved = the 128-sample tail of the short lap + raw buf.
- ONLY_LONG: the standard [half_prev | rev(half_cur)] long window.

My current natural-WOLA implementation must adopt these window functions in
the natural (unfolded) domain: assemble the 2048-sample window per sequence
as above and window the synthesis once — the TDAC PR then holds because the
windows are 0/1/shape-pieced consistently with the hop-1024 overlap.

CRITICAL FINDING (final experiment this session): the parse and dequant are
verified EXACT — frame 1 of tone.aac decodes to coefficients
±155132.33 = 7^{4/3}·2^{13.5} (sf = 2^(54/4), sfo = 54 = gg 154 + delta 0
− 100) — an exact dequant value, and even ONLY_LONG blocks (4-5) remain
uncorrelated noise. This RULES OUT the parse/dequant and points at the
WINDOW SEQUENCES for transition frames (LONG_START/LONG_STOP/EIGHT_SHORT):
my long-window = [prev-half | rev(cur-half)] is likely WRONG for transition
frames — the ISO window_sequences figure defines LONG_START = [long-left |
short-envelope halves(1024)] and LONG_STOP = [short-envelope halves |
long-right], with flat (1.0) regions per the FFmpeg algorithm
(out[576..1024] = buf[64..512] UNWINDOWED copy for LONG_STOP proves the
window has a 1.0 region there). Next session: implement the window
sequences per the ISO window_sequences figure (LONG_START = [long-left(prev
shape) | short-halves(cur shape)]; LONG_STOP = [short-halves(prev shape) |
long-right(cur shape)]), keeping the natural WOLA lap. Also re-check the
VMUL2S sign order for pair books (dim1 may consume the FIRST sign bit).

LATEST SESSION FINDINGS (element order + transform now verified correct):
- Frame 1 parses as FIL(count=15) + SCE(gg=154, seq=1 LONG_START, max_sfb=47,
  bands 10/11/6) — plausible for the encoded onset. Reference frame-1 output
  ≈ 0 (encoder priming region). My output ±20000 → the quantized coefficient
  VALUES are still wrong.
- The packed-idx → value path is the suspect: dumped coef 155132.33 = 7^{4/3}
  (13.3905) × 2^13.5 — magnitude plausibly from vals10_16, but the audio
  doesn't reconstruct. Next: dump the RAW quantized values (before sf
  multiply) for frame 1 of tone.aac and cross-check against the ISO codebook
  tables by hand (book 11 symbol → idx → dims/escape), verifying: dim nibble
  order (dim0 = low nibble ✓ per VMUL2S), sign bit application order (FFmpeg
  VMUL2S: dim1 consumes the FIRST sign bit, dim0 the SECOND — note the
  `sign >> 1 << 31` vs `sign << 31` asymmetry!), and the book-11 escape
  (escape dims: ones-count unary then (ones+4) magnitude bits).
- Also verify the sf dequant exponent: my sf = 2^(sfo/4) with
  sfo = gg + delta − 100 (matches FFmpeg pow2sf_tab[sfo + 200]).
- Alignment note: FFmpeg's decoded reference has a 1024-sample encoder delay
  (skip_samples): ref[0..1024] = the priming block's output ≈ 0; my frame-N
  output corresponds to ref[(N−1)·1024 .. N·1024].

Debug harness: AAC_DEBUG=1 dumps ICS internals + spectral
coefficients; compare vs FFmpeg decode of tests/data/test.aac
(test_ref.f32); run in --release (debug O(M²) IMDCT is slow).

## Phase 3 — Modern Compressed

- [x] Implement Opus packet parser (RFC 6716 §3: TOC, codes 0–3, padding, DTX, 120 ms cap, config tables)
- [x] Implement the bit-exact range coder (RFC 6716 §4.1 decoder + §5.1 encoder: decode/update, icdf, bit_logp, raw bits, uint, tell) — groundwork shared by SILK and CELT
- [x] Implement CELT decoder (MDCT-based, music-optimized)
- [x] Wire CELT-only packets into a top-level packet decode path (`src/decoder.rs`, `decode_celt_only_packet`) — TOC → (start/end band, stream channels, frame size) mapping, multi-frame packets, DTX/PLC; not the full `Decoder` trait (still needs SILK/hybrid)
- [ ] Root-cause the CELT `final_range` desync bug — see task breakdown below
- [ ] Implement SILK decoder (speech-optimized, LP-based) — see task breakdown below
- [ ] Integrate hybrid SILK+CELT mode (blocked on SILK decoder above)
- [x] Conformance test harness against the official Opus test vectors (`tests/conformance.rs`, `#[ignore]`d — see below); found and fixed a real packet-parser bug, and found (but has not yet root-caused) a residual CELT decoder bug

### CELT decoder — status (packet mapping verified correct; decoder itself has a residual, unresolved desync bug)

The full CELT layer is ported from libopus 1.5.2 (float build semantics,
no `-ffast-math`/`FLOAT_APPROX`) into `src/celt/`, targeting bit-exact
output with the reference:

- `tables.rs` — static 48 kHz/20 ms mode tables (window, FFT bitrev +
  twiddles, MDCT twiddles, eband5ms, logN400, band_allocation, CWRS
  `cache_index50/bits50/caps50`).
- `fft.rs` / `mdct.rs` — kiss FFT (all butterflies) and the backward MDCT
  with TDAC overlap-add (verified against naive DFT and the reference's
  own analytic MDCT oracles).
- `cwrs.rs` — PVQ pulse decoding over the verbatim 1272-entry `U(N,K)`
  table (`cwrsi`/`decode_pulses`); exhaustive small (N,K) round trips
  through the range coder + random large (N,K) inside `fits_in32`.
- `math.rs` — float-build mathops (`isqrt32`, `fast_atan2f`, `celt_log2/
  exp2` via f64 `ln`/`exp`, `celt_cos_norm`, `celt_udiv`).
- `laplace.rs` — coarse-energy Laplace decode/encode round trips.
- `rate.rs` — `get_pulses`, `bits2pulses`, `pulses2bits`,
  `clt_compute_allocation` (interp bisection, skip/intensity/dual-stereo
  signaling, fine-bit assignment with rebalancing), `init_caps`.
- `quant_bands.rs` — `unquant_coarse_energy` (Laplace + prediction),
  `unquant_fine_energy`, `unquant_energy_finalise`.
- `vq.rs` — `alg_unquant`, `exp_rotation` spread rotation,
  `renormalise_vector`.
- `bands.rs` — `quant_all_bands` recursion (`compute_theta` with the
  bitexact cos/log2tan helpers, `quant_band`, `quant_partition` with
  haar/hadamard TF recombination, folding, stereo split/merge,
  `special_hybrid_folding`), `denormalise_bands`, `anti_collapse`,
  `tf_decode`.
- `pitch.rs` — pitch postfilter (`comb_filter` in place + two-buffer
  variant) and the PLC pitch search stack (downsample/xcorr/search).
- `celt_lpc.rs` — autocorr/Levinson/FIR/IIR for packet-loss concealment
  (note the reference's NEGATED LPC sign convention and the IIR's
  batch-4 accumulation order, both preserved).
- `decoder.rs` — `CeltDecoder` assembly: `celt_decode_with_ec` (silence
  flag, postfilter, transient/intra flags, loss-recovery energy safety,
  dynalloc, trim, anti-collapse bit, deemphasis, postfilter state
  machine), `celt_decode_lost` (noise-based AND pitch-based PLC),
  `celt_synthesis` (mono↔stereo up/downmix paths), `prefilter_and_fold`.
  All scratch is preallocated in the struct (alloc-free `decode`).
- `range.rs` fixes: `decode_icdf` reimplemented in libopus's algebraic
  form (0-terminated tables select the last symbol), `decode_uint`
  clamps out-of-range values like the reference, added `tell_frac`,
  `force_tell`, `rng`.

- `src/decoder.rs` — `decode_celt_only_packet`: the packet TOC → CELT
  mapping (`start_band=0`, `end_band` from bandwidth via
  `celt_end_band` 13/17/19/21, `stream_channels` from `toc.stereo`,
  `frame_size` from `toc.frame_duration()`), driving one `CeltDecoder`
  across a multi-frame packet's frame ranges (DTX frames as
  concealment), matching `opus_decode_frame`'s CELT-only slice. This
  mapping has been verified correct empirically (see conformance
  results below) — it is not the source of the residual decoder bug.
- `CeltDecoder::final_range()` (new, read-only `CELT_GET_FINAL_RANGE`
  equivalent) exposes the range coder's final state after a decode, so
  it can be cross-checked against an encoder's own reported final range
  (the official test vectors' `.bit` files carry exactly this) —
  independent of, and more precise than, a PCM comparison.

Testing: 52 tests in the crate — round trips (range coder, laplace,
cwrs, alg_unquant unit-norm invariants), table spot checks, FFT/MDCT
oracles, comb-filter formula checks, LPC recovery of an AR(2) system,
PLC pitch search finding a synthetic period, plus never-panic
smoke/fuzz integration tests over random packets, DTX runs through both
PLC paths, cold-start concealment, and the new packet→CELT mapping unit
tests (SILK/Hybrid rejection, end-band table, a basic CELT-only decode).
`cargo clippy --all-targets` and `cargo fmt` clean.

### CELT `final_range` desync bug — session update (root cause still NOT found; extensive elimination done)

**No fix landed this session.** Conformance numbers are unchanged from
before (confirmed by re-running the suite at the end of the session):
testvector01 0/2147, testvector07 286/4186, testvector08 91/1242,
testvector09 405/1332, testvector10 174/1598, testvector11 0/553 range
matches (testvector02-06/12 have no CELT-only segments). This session did
NOT touch the decode logic in a way that changes behavior — all edits were
temporary debug instrumentation, added and then fully reverted (verified via
`git diff` showing no residual changes beyond the prior session's
`final_range()` accessor).

**What this session ruled out, definitively (not just "looks right on
inspection" — see method below):**

- Every static data table used by the CELT decode path was extracted
  *programmatically* from the actual libopus 1.5.2 source (downloaded fresh
  from `github.com/xiph/opus` tag `v1.5.2`: `celt/{bands,rate,cwrs,vq,
  quant_bands,laplace,celt_decoder,celt,entcode,entdec,mathops}.c` +
  matching headers + `celt/static_modes_float.h` + `celt/modes.c`) and
  diffed byte-for-byte against this crate's Rust tables via small throwaway
  `examples/diff_*.rs` binaries (parse the C initializer list including its
  `#if defined(CUSTOM_MODES)` conditional regions, compare element-by-element)
  — not eyeballing. Zero mismatches found in: `BAND_ALLOCATION` (231 u8),
  `CACHE_INDEX50`/`CACHE_BITS50`/`CACHE_CAPS50` (105/392/168), the full
  `CELT_PVQ_U_DATA` table (1272 u32 — the one previously only "spot-checked"),
  and `e_prob_model` (336 u8, the Laplace coarse-energy probability model).
  `EBAND5MS`/`LOGN400`/`E_MEANS`/`PRED_COEF`/`BETA_COEF` were also
  hand-diffed and match.
- Re-derived (not just line-diffed) the entropy coder primitives
  algebraically against `entdec.c`/`entcode.c`/`mfrngcod.h`/`entcode.h` to
  *prove* equivalence rather than assume it: `decode()`/`update()` (the
  general `ec_decode`/`ec_dec_update`), `decode_bit_logp` (shown
  algebraically equivalent to `ec_dec_bit_logp`'s direct fast-path, since
  this crate routes it through the generic `decode`/`update` pair instead of
  duplicating a fast path — the two are mathematically identical, confirmed
  by expanding both), `decode_uint` (both the `ftb<=8` and `ftb>8`/raw-bits
  branches), `decode_icdf`, `tell()`/`tell_frac()` (including the
  8-entry correction-table shortcut), `read_raw_bits` (proved bit-for-bit
  equivalent to `ec_dec_bits`'s window-based algorithm by re-deriving the
  overall bit order both produce), and `RangeDecoder::new`'s init constants
  (`EC_CODE_BITS`/`EC_SYM_BITS`/`EC_CODE_EXTRA`/`EC_CODE_TOP` arithmetic).
- Line-by-line re-verification (this time to the point of manually
  expanding every macro under float-build semantics, e.g. confirming
  `PSHR32`/`SHL32`/`MULT16_16` collapse to plain float ops so `SHL32(q,7)`
  really is just `q` in the float build, not `q*128`) of: `unquant_coarse_energy`,
  `unquant_fine_energy`, `unquant_energy_finalise`, `laplace_decode`,
  `tf_decode` (including the `tf_select_rsv`/`budget` bit accounting and the
  `TF_SELECT_TABLE` indexing), `compute_allocation`/`interp_bits2pulses`
  (the outer allocation-vector bisection, the skip-band backwards loop, the
  intensity/dual-stereo reservation and decode, the fine-bit assignment with
  rebalancing), `init_caps`, `bits2pulses`/`pulses2bits`/`get_pulses`,
  `compute_qn`, `bitexact_cos`/`bitexact_log2tan`/`frac_mul16`/`ec_ilog`,
  `compute_theta` (both the triangular and uniform-pdf branches, the
  `qn==1`/stereo-inv-flag branch, the `imid`/`iside`/`delta` derivation),
  `quant_partition`'s split-vs-leaf decision and both split sub-calls
  (`mbits`/`sbits`/rebalance bookkeeping), `quant_band`'s recombine/
  time-divide/Hadamard-(de)interleave pre/post-processing (confirmed the
  decoder path in libopus only transforms `X` in the *post* (resynth) half,
  never before `quant_partition`, matching this port), `quant_band_stereo`'s
  N=2 special case, `alg_unquant`/`exp_rotation`/`extract_collapse_mask`,
  `cwrsi`/`decode_pulses` (the "lots of pulses" vs "lots of dimensions"
  branches, the N=2/N=1 tail), and `isqrt32` (re-derived against
  `mathops.c`'s azillionmonkeys algorithm; the Rust port's extra `.max(1)`
  guard on `ec_ilog(val)` is inert for every value this codebase actually
  passes it, since `decode_pulses`'s triangular-pdf callers of `isqrt32`
  never pass 0).
- All of the above traced live against a real failing case
  (`testvector07` packet 0: TOC config=31, mono, code 0, single 20 ms CELT
  frame, `got=0x031bb500 want=0x2c33ee00`) using a temporary
  `CELT_DEBUG=1`-gated `eprintln!` trace (per-band `i`/`n`/`b`/`tell`,
  per-leaf `q`/pulses, and the frame header's `silence`/`is_transient`/
  `intra_ener`/`spread`/`alloc_trim`/`tf_res`/`offsets`/`intensity`/
  `dual_stereo`/`coded_bands`/`balance`/`pulses[]`) — all fully reverted,
  see the git-diff check above.

**New empirical finding that overturns the previous session's leading
hypotheses** ("transient/Hadamard path" and "`quant_partition` splits"):
scanning every CELT-only packet in `testvector07` with the trace above and
cross-referencing against `final_range` match/mismatch shows the bug is
*not* confined to either of those code paths:
  - Packet 1 (config 31, **`is_transient=false`**, i.e. long blocks, no
    Hadamard recombine/deinterleave at all) still desyncs — ruling out the
    transient-only theory the correlation with `coded_bands` had suggested.
  - Packets 453 (mismatch) and 454 (match) — back to back in the same file,
    same TOC config (29, 5 ms fullband), extremely similar band structure
    (`coded_bands` 14 vs 15, same `is_transient=false`, nearly identical
    per-band pulse counts) — **both decode every band as a single leaf**
    (`quant_partition` never splits: `n<=2` or insufficient bits in every
    band), yet one matches and the other doesn't. So the bug isn't gated on
    ever reaching the split recursion either.

  Together these say the residual bug is **not a code path that's simply
  never been exercised** (every plausible "this branch is untested" theory
  now has a counter-example either passing or failing on both sides), but
  something that depends on the *specific decoded values* hitting an edge
  case — a numeric edge case in a clamp/rounding/threshold shared by both
  transient and non-transient, split and non-split bands. Given the
  exhaustive audit above found no such edge case by inspection, it's likely
  either (a) extremely subtle (a single off-by-one in a rarely-hit branch
  of one of the audited functions that inspection still missed), or (b) in
  a piece of the pipeline not yet exhaustively re-audited this session:
  `celt_lpc.rs`/`pitch.rs` (postfilter decode reads bits once per frame —
  low call frequency but never re-verified this session), or `mdct.rs`/
  `fft.rs` (these can't affect `final_range` directly, so are out of scope
  for *this* bug specifically, per the reasoning that a range mismatch
  requires a different sequence of `ft` arguments to the entropy coder, not
  just wrong resynthesis values).

**Recommended next step**: since exhaustive line-by-line/table-diff/
algebraic-equivalence auditing (this session) and the prior session's own
pass both failed to find it, the highest-leverage next move is a byte-exact
*oracle trace* rather than more reading — either (a) get user approval to
install a C toolchain (gcc/clang) so libopus 1.5.2 can actually be built and
instrumented (its own `celt_decode_with_ec` can be patched with the same
kind of per-band trace this session used, then diffed line-for-line against
this crate's trace for the same packet — this would find the exact
divergence point in minutes instead of more manual reading), or (b) find/
build a second independent Opus decoder implementation (e.g. a WASM build
of libopus runnable without installing a system toolchain, or a pure-Rust/
JS reimplementation) to use as the oracle instead. Both were out of scope
this session per the "no new system software without asking" constraint,
and no such WASM/alternative build was readily available via `WebFetch`
without more investigation than the remaining session budget allowed.

Reference C sources for diffing (this session's copy, not committed):
`C:\Users\Phillip\AppData\Local\Temp\claude\...\scratchpad\opus_src\celt\`
— re-downloadable from `github.com/xiph/opus` tag `v1.5.2` if that temp
directory is gone; it is NOT the same path as the prior session's
`C:\Users\Phillip\AppData\Local\Temp\opencode\opus-1.5.2\celt\`, which no
longer exists on this machine (opencode's temp dir was cleaned since).

### SILK decoder — task breakdown (parallelizable)

New module tree under `tpt-av-cadence-opus/src/silk/`, mirroring the
`celt/` layout. Ported from libopus 1.5.2 `silk/` + RFC 6716 §4.2.
Dependency structure below determines what can start immediately in
parallel vs. what must wait on an interface (not a full implementation)
from an earlier task.

**Tier 1 — start immediately, no dependencies on each other:**

- [x] `silk/tables.rs` — static tables: NLSF stage-1/stage-2 codebooks,
  NLSF cosine table, pitch lag/contour codebooks, gain quantization
  tables, shell-code pulse-count tables, LTP codebooks, stereo/other
  tables, pitch-estimation tables (ported from libopus 1.5.2
  `silk/tables_*.c`, `silk/table_LSF_cos.c`, `silk/pitch_est_tables.c`).
  **Verified programmatically** (not by eye): a throwaway Python
  extractor parsed all 77 tables out of the actual 1.5.2 C sources and
  diffed them element-by-element AND shape-wise against the Rust
  statics — all 72 data tables, 5 pointer tables (member order), and
  both `silk_NLSF_CB_struct` initializers (scalars incl. resolved
  `SILK_FIX_CONST`s, field wiring, order) match exactly; zero
  transcription errors found. Pinned by 6 in-file unit tests, the key
  one being FNV-1a content hashes over every table computed from the
  C-extracted values (any future divergence from libopus fails
  `table_content_matches_libopus`), plus iCDF monotonicity, NLSF CB
  slice-length/pointer-wiring, LSF-cos antisymmetry, and shell-table
  structure invariants.
- [x] `silk/decode_indices.rs` — per-frame side-info bitstream layout:
  VAD/LBRR flags, frame-type + quantization-offset index, then the
  index fields consumed by gains/NLSF/pitch (this task only needs to
  *define the struct* of decoded indices early so Tier 2 tasks can code
  against it — flag the struct as provisional in a doc comment so Tier 2
  isn't blocked waiting for it to be finalized). Done: `SideInfoIndices`
  (provisional) + `decode_indices`/`nlsf_unpack` + VAD/LBRR flag
  helpers, round-trip tested; Tier 2 can code against the struct now.
- [x] `silk/stereo.rs` — mid/side predictor index decode + unmix to L/R.
  Done: `silk_stereo_decode_pred` (joint stage-1 index + per-weight
  uniform3/uniform5 pairs, Q13 dequant with the 6554 sub-step weight,
  combined `w0-w1` output), `silk_stereo_decode_mid_only`, and
  `silk_stereo_MS_to_LR` (8 ms predictor interpolation ramp,
  3-tap-lowpass + raw mid side prediction, LR sum/diff conversion) over
  a new shared `silk/sigproc.rs` (SMULBB/SMULWB/SMLAWB/SMLABB/
  RSHIFT_ROUND/SAT16 fixed-point helpers mirroring the reference's
  generic macros, incl. the i16 truncation of the combined predictor
  and its wrap-on-store into `pred_prev_Q13`). Algorithm cross-verified
  against both the 1.5.2 C sources and RFC 6716 §4.2.7.1–§4.2.8
  (bitstream order, PDFs→iCDFs, and formulas agree); the stereo tables
  in `tables.rs` were also re-diffed against `tables_other.c`. Tests:
  exhaustive decode round trip over all 5625 index combinations vs an
  independent RFC-formula dequant, mid-only round trip, zero-predictor
  sum/diff identity, ramp overshoot phase-boundary check, cross-frame
  history carry-over, saturation, a synthetic AR(2) mid + shaped side
  end-to-end recovery (±1 LSB residual-granularity bound), and
  never-panic fuzz for both decode and unmix paths.
- [x] `silk/resampler.rs` — internal-rate (8/12/16 kHz) → output-rate
  resampler, ported bit-exactly from `silk/resampler*.c`: all four
  kernels (copy, 2x allpass upsample `up2_hq`, IIR+FIR fractional
  upscale, AR2+FIR fractional downscale) plus the standalone `down2` /
  `down2_3` helpers, the Q16 ratio computation with its round-up loop,
  and the decoder/encoder delay-compensation matrices. Contract mirrors
  the reference: whole-millisecond chunking, state persists across
  calls (bit-identical output for any chunking), alloc/lock/panic-free;
  reuses the shared `silk::sigproc` helpers (keeps only `SMULWW`
  locally for the ratio loop). 17 unit tests: coefficient spot-checks,
  delay-matrix and invRatio-Q16 values, exact output lengths for all 21
  supported rate pairs, chunked-vs-monolithic bit-exactness, DC/step
  settle, sine-sweep passband gain, out-of-band rejection, and image
  suppression. Note: the reference filters are not DC-exact (worst
  ~+0.6% on 12→8) and the fractional-FIR phases ripple ±few LSB on a
  constant — the tests assert those measured properties rather than
  ideal ones. (`down2_3` is chunking-invariant only across 3-aligned
  boundaries, as in the reference's usage.)

**Tier 2 — depends only on the `decode_indices` struct shape (not its full
bitstream correctness), so can start in parallel once that struct exists:**

- [x] `silk/nlsf.rs` — NLSF stage-1/2 decode, backward-prediction
  reconstruction, stabilization, inter-frame interpolation, NLSF→LPC
  (`NLSF2A`) conversion, LPC bandwidth expansion. Done: full port of
  `silk/NLSF_decode.c` (`nlsf_residual_dequant` + `nlsf_decode`:
  `NLSF_unpack` reuse, stage-2 backward-prediction residual dequant
  with the `NLSF_QUANT_LEVEL_ADJ` offset, stage-1 codebook add with
  inverse-square-root weights, `silk_LIMIT` clamp), `silk/NLSF_stabilize.c`
  (`nlsf_stabilize`: both the 20-loop minimum-Euclidean-distance
  move-apart/center-frequency path and the insertion-sort fallback),
  the `decode_parameters.c` interpolation formula (`nlsf_interpolate`),
  `silk/NLSF2A.c` (`nlsf2a`: ordering16/ordering10 reordering, LSF-cos
  linear interpolation, even/odd polynomial convolution, `LPC_fit`
  with the chirp/clip branches, and the 16-iteration bandwidth-expansion
  stability loop), `silk/LPC_inv_pred_gain.c` (full `silk_INVERSE32_varQ`
  refinement port incl. the `i32::MIN` wrap edge case at
  `rc_mult1 = 2^30`), `silk/bwexpander.c`/`bwexpander_32.c`. Also added
  the missing `SigProc_FIX.h`/`macros.h`/`Inlines.h`/`sort.c` helpers to
  `sigproc.rs` (SMULL/SMULWW/SMLAWW/SMMUL/RSHIFT_ROUND64/DIV32(_16)/
  ADD_LSHIFT32/ADD_SAT16/SUB_SAT32/insertion sort). Verification: an
  independent Python transcription of the C sources (tables parsed from
  the C files, not the Rust statics) generated 10 end-to-end golden
  vectors (decode→NLSF2A→bwexpander), 38 stabilize vectors covering both
  termination paths, 14 inverse-prediction-gain vectors, plus a 500-case
  FNV-1a differential hash over the full pipeline — all match the Rust
  bit-for-bit; fuzz/property tests pin the stabilize spacing/border
  invariants and panic-freedom. Two real port bugs caught by this
  harness: a `j as usize + 1` overflow in the insertion sort (j == -1)
  and `(1 << 15)` inferring `i16` in the stabilize border writes.
- [x] `silk/gains.rs` — gain index decode, delta coding, dequantization,
  inter-frame smoothing. Done: `gains_dequant` (port of the decoder half
  of `silk/gain_quant.c`, driven from `decode_parameters.c`) — delta
  accumulation into the persistent `LastGainIndex` state with the
  double-step mapping for large increases, the absolute path's
  inter-frame 16-step-down clamp (~21.8 dB), the `silk_log2lin` Q16
  conversion (`silk/log2lin.c`, both reference branch structures
  preserved), and the `LAST_GAIN_INDEX_ON_PACKET_LOSS = 10` state
  contract from `dec_API.c` (clamp disabled on packet loss and
  side-channel restart; init/reset value 0 makes it inert, per RFC 6716
  §4.2.4's note). Verified against the RFC's independently-stated
  formulas: exhaustive comparison of both coding paths over all
  (prev_state, symbol) pairs (64×41 delta, 64×64 absolute) against the
  RFC's absolute `log_gain` formulation and its own `silk_log2lin`
  expression, plus the RFC's stated Q16 bounds 81920/1686110208 as
  hand-traced literal endpoints. Tests: monotonicity + <1% accuracy of
  `log2lin` across its domain, hand-traced double-step/threshold
  boundary vectors, path-dependence of the clamp, multi-subframe
  chaining and nb_subfr scoping, total no-panic sweep over all corrupt
  i8 symbols × {0,10,63} states, and a bit-exact quant→dequant round
  trip via a test-side port of the encoder's `silk_gains_quant` +
  `silk_lin2log` over random gains/modes/frame sizes. Note: the
  bitstream gain-index symbols were already decoded by
  `decode_indices.rs`; this module owns the state threading and Q16
  dequantization. The intra-frame gain ramp (`prev_gain_Q16`,
  `decode_core.c`) and CNG smoothing stay with Tier 3's
  `synthesis.rs`/`plc.rs`.
- [x] `silk/pitch.rs` — pitch lag decode (absolute + relative/contour
  coding), LTP filter index → coefficient codebook lookup. Done: the
  *bitstream* side (absolute `lag_high·fs/2 + lag_low` / delta-coded
  primary lag index, contour/periodicity/LTP-index symbols) was already
  entropy-decoded by `decode_indices.rs`; this module owns the
  reconstruction — `decode_pitch` (port of `silk/decode_pitch.c`:
  per-subframe lags = primary lag + contour VQ offset
  (`CB_LAGS_STAGE2[_10_MS]` for NB, `CB_LAGS_STAGE3[_10_MS]` for
  MB/WB), clamped to the 2–18 ms·fs_kHz search range), `ltp_coefs_q14`
  (the voiced branch of `silk/decode_parameters.c`: per-subframe 5-tap
  LTP filter index → `LTP_VQ_PTRS_Q7` codebook lookup widened Q7→Q14 by
  `<< 7`), and `ltp_scale_q14` (RFC §4.2.7.6.3's 15565/12288/8192).
  Tests: exhaustive comparison against an oracle built straight from
  RFC 6716 §4.2.7.6.1 with Tables 33–36 transcribed from the RFC text
  (independent of `tables.rs`) over all 4 (fs, nb_subfr) configs ×
  every codebook entry × the full reachable primary-lag range plus
  out-of-range lags only relative coding can legally produce (the RFC
  leaves the primary lag unclamped; only subframe lags clamp);
  RFC-transcribed Tables 39–41 pinned exhaustively through
  `ltp_coefs_q14`; search-range saturation at 2·fs/18·fs; 10 ms
  partial-write semantics for both outputs (caller-array tails
  untouched, matching the reference's in-place `psDecCtrl` arrays);
  hand-traced NB chain (high/low bits → lag → contour); unsupported
  fs_kHz rejected like `decode_indices`' rate selectors.
  Note: while re-running the suite, fixed two wrong test *expectations*
  in `sigproc.rs` (`rshift_round64` wide-value case: reference formula
  `((a >> 32) + 1) >> 1` gives 128, not the test's 190; `add_lshift32`
  wrapping case: `i32::MAX + 1` wraps to `i32::MIN`) — implementations
  re-verified against the 1.5.2 `SigProc_FIX.h` macros first; opus lib
  tests now 126/126.
- [x] `silk/excitation.rs` — shell-code pulse-count decode, pulse
  position/LSB/sign decode, seed-based dither reconstruction. Done:
  `decode_pulses` (port of `silk/decode_pulses.c`: rate-level symbol,
  per-block pulse counts with the `SILK_MAX_PULSES + 1` LSB-shift
  escape loop — reading the last rate level's table offset by one entry
  from the 10th shift on, which removes the escape symbol — shell
  decoding per 16-sample block, LSB refinement, sign decode),
  `shell_decoder` (port of `silk/shell_coder.c`'s binary pulse-count
  tree, `silk_shell_code_table_offsets[p]` row selection, reference
  split order preserved since it fixes the entropy-coder symbol
  order), `decode_signs` (port of `silk/code_signs.c`: one sign symbol
  per nonzero magnitude in blocks whose marked count
  `sum_pulses | nLS << 5` is positive, table row
  `7·(quantOffsetType + 2·signalType)`, probability entry
  `min(p & 0x1F, 6)`), and `reconstruct_excitation` (the "Decode
  excitation" block of `silk/decode_core.c`: `q << 14`, the ±
  `QUANT_LEVEL_ADJUST_Q10` (=80) pull toward zero, the
  `QUANTIZATION_OFFSETS_Q10` offset, and the per-sample sign dither
  from the LCG `196314165·seed + 907633515` with wraparound; LCG state
  updated before the sign test, pulse accumulated after — order
  preserved; wrapping ops throughout for the reference's
  `ovflw` semantics). Cross-verified against RFC 6716 §4.2.7.4/§4.2.7.7/
  §4.2.7.8.2–5: identical LCG constants and sign-test order, and the
  RFC's adjust:offset ratios (20:25, 20:60, 20:8) match 80:100, 80:240,
  80:32 exactly. Tests: bit-exact round trips against a test-side port
  of the reference *encoder* (`silk_encode_pulses` incl. the
  `max_pulses_table` halving loop, `silk_shell_encoder`,
  `silk_encode_signs`; explicit rate level instead of the minSumBits
  search) over all frame lengths 80/120/160/240/320 (incl. the 10 ms @
  12 kHz partial-block padding) × all 6 (signalType, quantOffsetType)
  combos × sparse/dense/LSB-heavy vectors, with encoder/decoder
  `tell()` equality asserting identical symbol counts; exhaustive
  shell round trips for every 16-sample pattern up to 4 total pulses
  plus random up to 16; targeted pinning of the 10-shift offset-table
  read, the escape-then-zero block (LSB bits consumed, no signs), and
  zero-count block memset; dither verified against an independent
  u32/MSB formulation of the RFC's LCG over 40 random vectors plus
  hand-computed literals; all no-panic (corrupt streams bounded by
  construction: `nLshifts ≤ 10`, magnitudes ≤ 17407). Rate levels are
  encoded 0–8 only (level 9 exists solely as the escape table row),
  matching the reference's `k < N_RATE_LEVELS - 1` search bound.

**Tier 3 — depends on Tier 2 outputs (LPC coefficients, gains, LTP
coefficients, excitation signal):**

- [ ] `silk/synthesis.rs` — short-term (LPC) synthesis filter +
  long-term (LTP) prediction for voiced subframes, noise-shaping
  quantization inverse.
- [ ] `silk/plc.rs` — SILK-side packet loss concealment.

**Tier 4 — final assembly, depends on everything above:**

- [ ] `silk/decoder.rs` — `SilkDecoder` top-level: per-20ms-frame decode
  loop over 5ms subframes, LBRR (low-bitrate redundancy) handling,
  multi-frame packet support, wiring Tiers 1-3 together.

Testing note: each Tier 1/2 module should get its own round-trip/table
spot-check unit tests (mirroring how `celt/laplace.rs`, `celt/cwrs.rs`,
etc. were tested in isolation) so correctness is established before
Tier 4 assembly — this is what let multiple people work on `celt/` files
concurrently without waiting on a working end-to-end decoder.

### Conformance testing against the official RFC 6716 test vectors

`tests/conformance.rs` (`#[ignore]`d by default — the vectors are ~63MB
extracted, too large to bundle like the FLAC suite's) walks every
packet in a `.bit` file, decodes contiguous runs of CELT-only packets
through `decode_celt_only_packet` with one persistent `CeltDecoder`,
and compares against the reference `.dec` PCM plus (independently and
more precisely) the encoder's recorded final range-coder state per
packet. SILK/Hybrid packets are skipped (this crate can't decode them
yet) while still advancing the expected sample offset.

How to run it: download `opus_testvectors.tar.gz` from
<https://opus-codec.org/static/testvectors/opus_testvectors.tar.gz>
(cited in RFC 6716 §6.1/Appendix A.4), extract it, then:
`OPUS_TESTVECTORS_DIR=<path> cargo test -p tpt-av-cadence-opus --release -- --ignored`.

Running it against all 12 official vectors:
- Confirmed the packet-to-CELT mapping itself is correct: while writing
  this test, found and fixed a real bug in `src/packet.rs` — the code-3
  frame-count byte's `v`/`p`/`M` bit layout had the MSB-first convention
  backwards (was reading `M` from the top 6 bits and `v`/`p` from the
  bottom 2, RFC 6716 §3.2.5 Figure 5 has it the other way around,
  consistent with how the TOC byte itself is laid out). This was
  invisible to the crate's own synthetic unit tests (self-consistent
  either way) and only surfaced against real bitstreams, where it
  produced nonsensical frame counts (e.g. 32 frames in a 6-frame-max
  20ms packet).
- 6 of 12 vectors (02–06, 12) contain no CELT-only segments at all (not
  a failure — just SILK/Hybrid content this crate can't touch yet).
- The other 6 (01, 07–11) do contain CELT-only runs, and a meaningful
  fraction of their packets decode bit-exactly (`final_range` matches
  the encoder's recorded value: e.g. testvector09 405/1332, testvector07
  286/4186) — proving the TOC→CELT mapping (start/end band, channel
  count, frame sizing, multi-frame/DTX handling) is right. But most
  packets *don't* match, and the PCM comparison fails the strict
  thresholds (max abs diff <=2 int16 units / SNR >=90dB) on all 6.
- The mismatch is a real, unresolved bug in `CeltDecoder` itself, not in
  the packet mapping: a `final_range` mismatch means the decoder read
  the wrong number of bits from the entropy stream, which only
  functions that call into the range decoder can cause. Its rate
  correlates strongly with `coded_bands` (near 0% at 1-2 coded bands,
  >80% at 15+) and not with `is_transient`, stereo/dual-stereo, or
  postfilter state, which were all checked and ruled out — pointing at
  something in the per-band recursion in `celt/bands.rs`
  (`quant_partition`'s split path, called roughly once per coded band)
  or `celt/rate.rs` (`bits2pulses`, ditto), rather than a single missing
  feature. Extensive line-by-line comparison against libopus 1.5.2's
  `celt/bands.c`/`rate.c`/`cwrs.c`/`vq.c` (compute_theta, the
  interp_bits2pulses skip loop, bits2pulses's cache bisection, cwrsi's
  U(N,K) walk) turned up no discrepancy, so the exact root cause is
  still open.
- **Re-audit (this session): `celt/rate.rs`'s cache bisection re-verified
  a third time, still no discrepancy found.** Re-downloaded libopus 1.5.2
  `celt/rate.c`, `celt/rate.h`, `celt/celt.c`, and
  `celt/static_modes_float.h` fresh from `github.com/xiph/opus` tag
  `v1.5.2` and diffed directly (not from memory): `CACHE_INDEX50` (105),
  `CACHE_BITS50` (392), and `CACHE_CAPS50` (168) are byte-for-byte
  identical to `cache_index50`/`cache_bits50`/`cache_caps50`; `bits2pulses`/
  `pulses2bits`/`cache_index` match `rate.h`'s inline versions exactly,
  including the `LM++`-then-index convention (`lm + 1` in the Rust port) and
  the tie-break comparison (`bits - (lo==0 ? -1 : cache[lo]) <= cache[hi] -
  bits`); `interp_bits2pulses`/`compute_allocation` (the outer
  allocation-vector bisection, the skip-band backwards loop including the
  `psum -= bits[j] + intensity_rsv` reclaim, the fine-bit assignment) and
  `init_caps` also match `rate.c`/`celt.c` line-for-line. No off-by-one in
  the bisection bounds, at any band count. This rules out `rate.rs` as the
  source of the `coded_bands`-correlated desync with the same rigor as the
  prior sessions' audits of `bands.rs`/`cwrs.c` — the bug is not here
  either; the byte-exact oracle trace (real libopus build or an
  alternative decoder) recommended above remains the highest-leverage next
  step.

Deferred / next:
- Root-cause the `final_range` desync — see the "CELT `final_range` desync
  bug — session update" subsection above (under the CELT decoder status
  section) for the current, up-to-date state: still unresolved after two
  sessions of line-by-line/table-diff auditing; next step is a byte-exact
  oracle trace (real libopus build, or an alternative decoder
  implementation), not more manual reading.
- Bit-exact conformance vs libopus binaries (rather than just the
  recorded final range in the test vectors) still blocked on a
  gcc/ffmpeg toolchain (none on this machine).

## Phase 4 — Legacy & Open Source

- [ ] Implement `tpt-av-cadence-mp3` (Huffman decoding, polyphase filterbank, joint stereo) — pipeline implemented; PCM conformance remains unresolved
  - [x] Fix synthesis unsigned-index underflow reproduced by `probe_128k`.
  - [x] Handle short-stream probing, truncated-frame EOF, and probe-window boundary retention; track the first-frame seek offset across discarded windows.
  - [x] Preserve the in-band scalefactor when `big_values` ends mid-band and count1 takes over (isolated by a unit test; first-granule probe went from 15.9 dB to 104 dB).
  - [x] Use nine scalefactor bands for region 0 of pure short blocks (verified with a side-info parse test against frame 0; first granules went from 3 dB to ~118 dB).
  - [x] Keep `ist_pos` across granules so MPEG-1 scfsi sharing reads granule 0's values (matches `L3_read_scalefactors`); changed metrics negligibly on this stream but kept as correct.
  - [x] Add deterministic streaming regressions and assert the bundled 128 kbps stream's format, 133632-frame count, reference length, and finite output.
  - [x] Gate MPEG-1 granule-1 scfsi flags on that granule's own block type (short → none, long/start/stop → stream nibble), matching the C reference's running-shift semantics. This was the final root cause: frame 114's start-block granule lost sharing, corrupting its overlap into frame 115.
  - [x] Result on the bundled 128 kbps reference stream: **119.3 dB whole-file SNR, max sample diff 4.6e-7** (was 15.8 dB / 0.377 max at session start); this is a tolerance-based comparison, NOT bit-exact equality (earlier rounded per-frame diagnostics were misinterpreted). `probe_128k` asserts a >100 dB SNR gate. All 36 MP3 tests pass, including the ten-stream FFmpeg comparison.
  - [x] Independent FFmpeg 7.1 PCM gate across all ten bundled streams using the shared subprocess harness: >100 dB SNR and <=1e-5 peak error, equal lengths, no trimming, shift, or gain adjustment. Observed 118.74–119.37 dB, worst peak error 7.451e-7. Set `CADENCE_REQUIRE_FFMPEG=1` with `ffmpeg` on PATH to prevent a skip.
  - [x] Fix MPEG-2 scalefactor-band table selection: `!lsf` duplicated the MPEG-1 flag instead of testing the header's not-MPEG-2.5 bit. New nine-rate unit test and external PCM test failed first. MPEG-2 16/22.05/24 kHz improved from -2.07/21.26/11.14 dB to 118.76/118.89/118.74 dB.
  - [x] Remove decoder debug environment checks, diagnostic output, and file writes (including their unwraps); full safety is not implied.
  - [x] Fix LSF partition lookup (block-type row base plus byte offset) and MPEG-2/2.5 private-bit counts (mono 1, stereo 2). The 16 kHz fixture now decodes all 86 frames / 49536 samples per channel instead of 5760.
  - [x] Structural/replay tests for all ten fixtures: pinned counts independently derived from complete frame headers, finite output, exact seek-to-zero replay, plus mid-stream seek on the 128 kbps fixture. These are NOT external PCM conformance tests.
  - [x] Add bounded proptest checks for arbitrary bytes and mutated/truncated MPEG-1/MPEG-2 fixtures, 128 cases each. No-panic results are not a proof for all inputs.
  - [x] Fix CRC-protected frame handling: select the correct MPEG-1/LSF side-info size and exclude the stored CRC from checksum coverage. Add a frame-size guard and three fail-before/pass-after regressions covering MPEG-1/2/2.5 mono/stereo, protected-bit rejection/recovery, and unprotected ancillary bytes. Full suite: 36 tests pass with FFmpeg required; strict Clippy and formatting pass.
  - [ ] Complete real-time safety and malformed-input audits, including reservoir bounds and mixed-block processing; error paths still allocate. CRC coverage now has synthetic regressions, but broader protected-stream testing remains open. Independent PCM checks now run with FFmpeg 7.1 bundled in `C:\Users\Phillip\AppData\Roaming\Python\Python313\site-packages\imageio_ffmpeg\binaries\ffmpeg-win-x86_64-v7.1.exe` (not on PATH; exposed under the name `ffmpeg.exe` in a temporary PATH directory for validation). Wider feature coverage and official bit-exact conformance remain open.
  - [x] Clear strict Clippy failures: `cargo clippy -p tpt-av-cadence-mp3 --all-targets -- -D warnings` passes. Fixed 23 library diagnostics and two test diagnostics without lint suppressions; coefficient spelling changes preserve f32 bits. All ten FFmpeg comparisons retain their measured SNR/peak errors after cleanup.
- [ ] Implement `tpt-av-cadence-vorbis` (MDCT-based OGG Vorbis decoder) — crate scaffolded with a module plan
- [ ] Conformance tests against mpg123 conformance streams (MP3)
- [ ] Conformance tests for Vorbis

## Cross-Cutting (ongoing, applies to every phase)

- [x] Enforce real-time safety contract per decoder (alloc-free/lock-free/panic-free `decode()`; all allocation confined to `init()`/`open()`) — holds for WAV/AIFF/FLAC/PCM; re-check for each new decoder
- [x] Fuzz testing (`proptest` never-panic property tests) for every new parser — WAV, AIFF, FLAC (arbitrary + mutated real streams), Opus packets/range coder; `cargo-fuzz` targets still to be added under `fuzz/`
- [x] Bit-exact validation harness (`assert_bit_exact_vs_ffmpeg`) wired for every new decoder — WAV covered; FLAC/AIFF verified via embedded checksums (FFmpeg cross-checks to follow in CI)
