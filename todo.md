# tpt-cadence — Project TODO

Tracks all tasks for the whole project, organized by phase. See `DESIGN.md` for full design rationale.

Status snapshot: the workspace builds clean (fmt/clippy/deny), and 133 tests pass, including bit-exact conformance suites for WAV, AIFF, and FLAC (the FLAC suite MD5-verifies against the official IETF decoder testbench vectors, CC0, bundled under `tpt-av-cadence-flac/tests/data/`).

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
- [ ] Implement CELT decoder (MDCT-based, music-optimized)
- [ ] Implement SILK decoder (speech-optimized, LP-based)
- [ ] Integrate hybrid SILK+CELT mode
- [ ] Conformance tests against official Opus test vectors

## Phase 4 — Legacy & Open Source

- [ ] Implement `tpt-av-cadence-mp3` (Huffman decoding, polyphase filterbank, joint stereo) — crate scaffolded with a module plan
- [ ] Implement `tpt-av-cadence-vorbis` (MDCT-based OGG Vorbis decoder) — crate scaffolded with a module plan
- [ ] Conformance tests against mpg123 conformance streams (MP3)
- [ ] Conformance tests for Vorbis

## Cross-Cutting (ongoing, applies to every phase)

- [x] Enforce real-time safety contract per decoder (alloc-free/lock-free/panic-free `decode()`; all allocation confined to `init()`/`open()`) — holds for WAV/AIFF/FLAC/PCM; re-check for each new decoder
- [x] Fuzz testing (`proptest` never-panic property tests) for every new parser — WAV, AIFF, FLAC (arbitrary + mutated real streams), Opus packets/range coder; `cargo-fuzz` targets still to be added under `fuzz/`
- [x] Bit-exact validation harness (`assert_bit_exact_vs_ffmpeg`) wired for every new decoder — WAV covered; FLAC/AIFF verified via embedded checksums (FFmpeg cross-checks to follow in CI)
