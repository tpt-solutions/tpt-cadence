# tpt-cadence — Project TODO

Tracks all tasks for the whole project, organized by phase. See `DESIGN.md` for full design rationale.

Status snapshot (workspace totals are historical; MP3 revalidated separately): `cargo deny check licenses` now passes clean — `deny.toml` migrated off the removed `deny` key (an allow-list is sufficient for the newest cargo-deny; everything not on `allow` is denied by default). MP3 passes 36 tests including the doc test and required FFmpeg 7.1 comparisons of all ten bundled streams (>100 dB SNR, <=1e-5 peak error); MP3 strict Clippy is clean; broader feature/bit-exact conformance and the full safety audit remain unresolved. Historically, ~182 tests passed, including bit-exact conformance suites for WAV, AIFF, and FLAC (the FLAC suite MD5-verifies against the official IETF decoder testbench vectors, CC0, bundled under `tpt-av-cadence-flac/tests/data/`). The Opus crate now has 172 unit tests (packet/range coder + the full CELT decoder + the packet-to-CELT wiring + the full SILK decoder: tables, side-info indices, stereo mid/side prediction, gains dequant, NLSF decode/NLSF2A, pitch lag + LTP codebook lookup, excitation decode with the seed-dithered reconstruction, the inverse-NSQ/LTP/LPC synthesis core (`silk/decode_core.c`), PLC + CNG (`silk/PLC.c`/`silk/CNG.c`, bit-exact against an independent Python oracle transcription), and the Tier 4 `SilkDecoder` top-level assembly (`silk/dec_API.c`/`decode_frame.c`/`decode_parameters.c`/`decoder_set_fs.c`) wiring all of the above into a per-payload decode loop with LBRR, mono/stereo, and resampling) plus an `#[ignore]`d conformance test against the official RFC 6716 test vectors, which found a real bug in the packet parser (fixed). This session root-caused and fixed the long-standing CELT `final_range` desync bug (a missing `nbits_total` update on raw-bit reads in `RangeDecoder`/`RangeEncoder`, found by building a real libopus 1.5.2 oracle with the already-installed VS Build Tools and diffing an instrumented trace) — all 6 CELT-containing test vectors now hit 100% range-coder match (was 0-30%); PCM SNR is still below the 90dB gate on all of them (a separate, smaller float-reconstruction issue) — see the CELT status section below. A new `decode_silk_only_packet` top-level entry point (mirroring `decode_celt_only_packet`) and a SILK arm in `tests/conformance.rs` were also run against the official RFC 6716 test vectors this session for the first time: found and fixed a real bug (only the first 20ms internal subframe of any 40/60ms SILK payload was being decoded, the rest left as silence), after which 3 of 12 vectors (pure SILK-only content) decode fully bit-exact (`final_range` matches 100% of packets, infinite PCM SNR); a smaller, still-unresolved transition glitch remains at internal-bandwidth (fs_kHz) switches — see the "SILK conformance — session update" section under Phase 3. Hybrid packets (SILK+CELT) are still rejected.

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
- [x] Root-cause the CELT `final_range` desync bug — found and fixed this session by building a real libopus 1.5.2 oracle (MSVC/CMake/Ninja, already installed as VS Build Tools — no new system software needed) and diffing an instrumented trace against it; see "CELT `final_range` desync — ROOT CAUSE FOUND AND FIXED" below. All 6 CELT-containing test vectors now hit 100% range-coder match (was 0-30%). Residual PCM SNR gaps (24-98dB, below the 90dB gate) remain — a separate, much smaller float-reconstruction issue, not an entropy desync.
- [x] Implement SILK decoder (speech-optimized, LP-based) — see task breakdown below; wired into `decode_silk_only_packet`. This session ran it against the official RFC 6716 test vectors for the first time, found and fixed a real multi-subframe (40/60 ms payload) decode bug, and got 3 of 12 vectors to bit-exact `final_range` match — see the "SILK conformance — session update" subsection under Phase 3 below
- [ ] Integrate hybrid SILK+CELT mode (SILK decoder is done; still needs the low-band CELT merge for hybrid packets)
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

### CELT `final_range` desync — ROOT CAUSE FOUND AND FIXED this session

After two prior sessions of exhaustive line-by-line/table-diff/algebraic
audits found nothing (see the historical record kept below this box), this
session took the recommended next step — a byte-exact oracle trace against
a real libopus build — and found the bug within a couple of hours.

**Building the oracle**: no new system software was needed. This machine
already has Visual Studio 2022 Build Tools installed
(`C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools`), which
bundles both CMake and Ninja
(`Common7\IDE\CommonExtensions\Microsoft\CMake\{CMake,Ninja}\`). Downloaded
`opus-1.5.2.tar.gz` from the official GitHub release, loaded the MSVC
environment from `VC\Auxiliary\Build\vcvars64.bat`, and built `opus_demo`
via `cmake -G Ninja -DOPUS_BUILD_PROGRAMS=ON` (default float build: OPUS_
FIXED_POINT/OPUS_FLOAT_APPROX/OPUS_FAST_MATH all OFF, matching this port's
target semantics). `opus_demo -d 48000 2 <in.bit> <out.pcm>` directly
consumes the RFC test vectors' `.bit` format and independently confirmed
`enc_final_range == dec_final_range` for every packet — i.e. libopus 1.5.2
itself is a valid bit-exact oracle for this exact format.

**Method**: added matching `getenv("CELT_C_..._DEBUG")`-gated `fprintf`
checkpoints to a local copy of `celt/celt_decoder.c` and `celt/rate.c`
(counting decode calls with a `static int` counter so a single packet index
could be isolated, e.g. `CELT_C_DEBUG_PKT=0`), and mirrored each checkpoint
with an `eprintln!` behind the same env var name in the Rust decoder
(`src/celt/decoder.rs`, `src/celt/rate.rs`). Rebuilding after each new
checkpoint pair and diffing the two traces bisected the divergence in about
six rounds: header flags (`silence`/`transient`/`intra_ener`/`spread`/
`alloc_trim`/`tf_res`) all matched, but the fractional bit-position
(`ec_tell`) at each checkpoint was *already* off by a constant amount before
even reaching the allocation code — traced backward through coarse-energy
decode and tf_decode (bit consumption there matched, so the constant offset
was already present *before* them) to the postfilter block, and finally to
the exact two calls: `octave = ec_dec_uint(dec, 6)` and
`postfilter_pitch = (16<<octave) + ec_dec_bits(dec, 4+octave) - 1` /
`qg = ec_dec_bits(dec, 3)`. The *decoded values* (octave, pitch, qg) matched
exactly between C and Rust — only the bit-position bookkeeping afterward
diverged (by exactly the number of raw bits read: 7 in the traced case).

**Root cause**: `RangeDecoder::read_raw_bits` (`src/range.rs`) never updated
`self.nbits_total`. Every other decode primitive (`decode`/`update`,
`decode_bit_logp`, the `ftb<=8` branch of `decode_uint`) advances
`nbits_total` (directly or via `normalize()`'s byte-refill), but the raw-bits
path — used for CELT's postfilter pitch/gain fields and the high-order bits
of `decode_uint`'s `ftb>8` branch — silently exempted itself. Real libopus's
`ec_dec_bits` (`entdec.c`) explicitly does `_this->nbits_total += _bits;`.
Since `tell()`/`tell_frac()` (`ec_tell`/`ec_tell_frac`) are the *only* way
the CELT allocator knows how many bits remain, every budget-dependent
decision downstream of any raw-bits read — the dynalloc per-band boost
loop's bit-availability check, the `alloc_trim` icdf gate, and ultimately
the entire `bits`/`total` value fed into `clt_compute_allocation` — was
computed against an under-counted budget, producing a different (but
plausible-looking) allocation and, with it, a different sequence of
subsequent entropy reads: exactly the "looks locally consistent but globally
wrong" signature that made this invisible to per-function unit tests (none
of which exercised a bitstream that both used postfilter *and* checked
`tell()` against an external oracle) and to code-reading audits (the bug is
an *omission*, not a wrong formula, so nothing to spot by reading the
formula that IS there). This also explains the previously-noted correlation
with `coded_bands`: postfilter is far more likely to be enabled on frames
complex enough to code many bands.

**Fix**: added the missing `self.nbits_total += count;` to
`RangeDecoder::read_raw_bits`, and the equivalent `self.nbits_total += bits;`
to `RangeEncoder::write_raw_bits` (same omission, same fix, matching
`ec_enc_bits` in `entenc.c` — this side wasn't the cause of the decoder
conformance failures, since encode+decode round-trip tests used the same
buggy accounting symmetrically and so never caught it, but was equally
wrong and is now consistent).

**Result**: re-ran the full official-vector conformance suite after the
fix. Every CELT-containing vector now matches **100% of packets'
`final_range`** (was 0-30%):

| vector | range match (before → after) | PCM SNR (before → after) |
|---|---|---|
| 01 | 0/2147 → **2147/2147** | -2.3dB → 73.4dB |
| 07 | 286/4186 → **4186/4186** | -1.5dB → 49.4dB |
| 08 | 91/1242 → **1242/1242** | -1.7dB → 37.5dB |
| 09 | 405/1332 → **1332/1332** | 0.2dB → 55.7dB |
| 10 | 174/1598 → **1598/1598** | 0.8dB → 24.3dB |
| 11 | 0/553 → **553/553** | -2.4dB → 98.3dB |

All 172+5+1 existing opus unit/fuzz tests still pass; `cargo clippy
--all-targets -- -D warnings` and `cargo fmt` are clean.

**What's left**: PCM SNR is still below the 90dB gate on every vector
(only testvector11 comes close, at 98.3dB — that one alone would already
pass). Since the entropy decode is now proven bit-exact (100% `final_range`
match means the decoder read *exactly* the same bits as the encoder
intended), the remaining gap is purely in the float DSP reconstruction
(MDCT synthesis, deemphasis, postfilter comb-filter application, or
resampling) — a different, almost certainly much smaller class of bug than
the entropy desync that's now fixed. testvector10's 24.3dB is the worst
outlier and the best next place to look; worth checking whether it uses
postfilter more heavily than the others, given postfilter code was exactly
where the just-fixed bug lived (though the *fix* itself only changed bit
accounting, not the DSP values, so this would be a separate, coincidental
issue if postfilter-related at all).

**Oracle build artifacts are not committed** — they live under
`C:\Users\Phillip\AppData\Local\Temp\claude\opus_src\` (`opus-1.5.2/` source
with temporary debug `fprintf`s added to `celt_decoder.c`/`rate.c`, and
`build/opus_demo.exe`), separate from this repo. Re-buildable in a few
minutes from a clean `opus-1.5.2.tar.gz` if needed again (see the "Building
the oracle" paragraph above for the exact CMake invocation — no source
changes are required to rebuild `opus_demo` itself; the debug `fprintf`s are
only needed if bisecting a *new* divergence the same way).

<details>
<summary>Historical record: two prior sessions' audit trail (kept for
reference; superseded by the root cause above)</summary>

**What those sessions ruled out, definitively (not just "looks right on
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

</details>

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

- [x] `silk/synthesis.rs` — short-term (LPC) synthesis filter +
  long-term (LTP) prediction for voiced subframes, noise-shaping
  quantization inverse. Done: `decode_core` (port of
  `silk/decode_core.c`: the seed-dithered excitation reconstruction
  delegated to `excitation::reconstruct_excitation`; per-subframe
  `inv_gain_Q31` via `silk_INVERSE32_varQ(·, 47)` and the
  `silk_DIV32_varQ` gain-change factor that rescales the sLPC_Q14
  (and, on non-rewhitened voiced subframes, the sLTP_Q15) states;
  voiced-branch re-whitening at subframe 0 and — when NLSF
  interpolation is active — again at subframe 2 (which first copies
  xq[0..2·subfr] into outBuf), with the k=0 `LTP_scale_Q14`
  downscale; the 5-tap LTP prediction over sLTP_Q15 with the +2 bias
  and the residual feedback into the state; the 10th/16th-order LPC
  synthesis with `ADD_SAT32`/`LSHIFT_SAT32` and the Q0 output
  quantization), plus the "voiced PLC → unvoiced" transition fix-up
  (center-tap-only LTP filter + `pitchL[k] = lagPrev` for k < 2 —
  mutating `pitchL` is observable because `silk_decode_frame` feeds
  `pitchL[nb_subfr-1]` back into `lagPrev`), the
  `silk_decoder_state` subset as `SynthesisState` (sLPC_Q14_buf /
  outBuf incl. the end-of-frame slide-and-append from
  `silk_decode_frame` / prev_gain_Q16, reset to 65536 per
  `silk_reset_decoder`), frame geometry per `silk_decoder_set_fs`
  (`FrameInfo`), and new shared helpers `silk_DIV32_varQ` and
  `silk_LPC_analysis_filter` (generic non-`USE_CELT_FIR` form) in
  `sigproc.rs`. Verification: 48-case bit-exact FNV-1a hash suite
  over whole 3-frame decode runs (xq + post-frame pitchL + final
  sLPC/outBuf/prevGain) against an independent Python transcription
  of decode_core.c (covering all 6 fs×nb_subfr geometries, NLSF
  interpolation on/off, gain changes incl. the state-rescale path,
  all signal types, loss sequences with the transition fix-up, and
  both realistic ±8k and full-range i16 coefficient distributions);
  structural tests (unvoiced zero-LPC closed form, voiced
  center-tap LTP hand trace incl. the k=0 scale participation,
  pitchL-override truth table, outBuf slide, reset-state values,
  geometry table); `div32_varq` endpoint literals checked against
  the same transcription; no-panic fuzz over full-range garbage in
  every geometry. Also fixed four never-compiled expectations in
  `sigproc.rs`'s prior-session test block (`rand` LCG literal type,
  `0x8765_4321` literal overflow, `smultt`/`sub_lshift32`/
  `lshift_sat32` semantics — the LIMIT-then-shift macro tops out at
  `32767 << 16`, not `i32::MAX`), and pinned `sum_sqr_shift`'s
  reference fixed-point bias for saturated inputs `(306764653, 3)`
  from a C transcription (the port is bit-exact; the old exact-sum
  expectation was wrong even for libopus), plus the AR(1) analysis
  test's coefficient (0.5 in Q12 is 2048, not 8192) and the
  d==len all-zero case. Opus lib tests now 154/154.
- [x] `silk/plc.rs` — SILK-side packet loss concealment. Done: full port
  of `silk/PLC.c` (`plc`/`plc_conceal`/`plc_update`/`plc_glue_frames`,
  the voiced pitch-based and unvoiced LPC-noise concealment paths, the
  cross-fade glue after a loss run) and `silk/CNG.c` (`cng`: NLSF
  smoothing, per-subframe gain tracking, LPC-shaped comfort noise added
  into the output during silence/loss). Verification: a 32-case
  bit-exact FNV-1a hash suite over multi-frame (loss/normal-mixed) runs
  of `plc`+`cng`+`plc_glue_frames` in `silk_decode_frame`'s exact call
  order, against an independent Python transcription of `PLC.c`/`CNG.c`;
  plus `plc_reset`/`cng_reset` literal checks, an fs-change reset-path
  test, and a never-panic sweep over hostile state/coefficient
  combinations.
- [x] `silk/decoder.rs` — `SilkDecoder` top-level. Done: full port of
  `silk/dec_API.c`'s `silk_Decode` (first-frame-of-payload bookkeeping,
  VAD/LBRR flag decode, the LBRR skip/decode pass, mid/side predictor
  decode and side-channel skip logic, per-frame `LostFlag` dispatch,
  mid/side → left/right conversion, internal→API resampling, mono→stereo
  duplication, `prevPitchLag`/`LastGainIndex` cross-packet updates),
  `silk_decode_frame` (`ChannelState::decode_frame`: side-info +
  excitation + parameter + core decode, outBuf slide, PLC update/
  concealment, CNG, frame gluing), `silk_decode_parameters` (gain
  dequant, NLSF decode + NLSF2A + interframe interpolation, post-loss
  BWE, voiced pitch/LTP lookups), and `silk_decoder_set_fs` (geometry,
  NLSF codebook selection, resampler reinit, reset-on-rate-change
  semantics). All scratch lives in the state structs (alloc-free
  `decode`). Wired into a new top-level `decode_silk_only_packet`
  (`src/decoder.rs`, mirroring `decode_celt_only_packet`): TOC config →
  internal rate/payload duration, per-frame range decoder, DTX frames as
  concealment. Tests: `set_fs` geometry table for all 6 (fs, nb_subfr)
  pairs, resampler reinit on rate changes, invalid-rate rejection,
  packet-loss plumbing (geometry/output-length math/loss counting/gain-
  clamp removal), loss-run determinism + pitch-lag drift bounds, mono→
  stereo duplication, two-channel internal loss bookkeeping, and
  `decode_parameters`' unvoiced-zeroing/interpolation-gate/post-loss-BWE
  branches. Not yet validated against the official RFC 6716 test vectors
  or FFmpeg (see the Opus conformance section below) — the SILK
  bitstream-to-PCM path is untested against real encoded streams this
  session, only against independent-oracle unit tests of each stage.

**Tier 4 — final assembly, depends on everything above:** done (see
`silk/decoder.rs` above).

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
packet. As of this session, SILK-only packets are decoded too, through
the new `decode_silk_only_packet` with one persistent `SilkDecoder`
(reset whenever the previous packet was CELT-only, matching libopus);
Hybrid packets are still skipped (this crate can't decode them yet)
while still advancing the expected sample offset. This session did not
have the official test vectors available locally to actually run the
suite, so the SILK path's real-world pass rate against RFC 6716 vectors
is not yet known — only its independent-oracle unit tests (see the
task breakdown above) have been verified.

How to run it: download `opus_testvectors.tar.gz` from
<https://opus-codec.org/static/testvectors/opus_testvectors.tar.gz>
(cited in RFC 6716 §6.1/Appendix A.4), extract it, then:
`OPUS_TESTVECTORS_DIR=<path> cargo test -p tpt-av-cadence-opus --release -- --ignored`.

**Update (this session): the CELT desync bug referenced throughout the
section below is now fixed** — see "CELT `final_range` desync — ROOT CAUSE
FOUND AND FIXED this session" earlier in this file. All 6 CELT-containing
vectors (01, 07-11) now hit 100% `final_range` match; PCM SNR is still below
the 90dB gate on all of them (a separate float-reconstruction issue) — see
that section for current numbers and next steps. The bullets immediately
below are the historical run that first characterized the bug, kept for
context.

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

### SILK conformance — session update: multi-subframe bug found and fixed; three vectors now bit-exact

This session downloaded `opus_testvectors.tar.gz` and ran `tests/conformance.rs`
against all 12 official vectors for the first time. Also added SILK
SNR/range-match reporting to the test's output (previously computed but never
printed — see `run_vector`'s `silk_stats`/`silk_packet_count` fields).

**Found and fixed a real bug**: `decode_silk_only_packet` (`src/decoder.rs`)
called `SilkDecoder::decode` exactly once per *Opus* frame, but
`SilkDecoder::decode` only ever produces one *internal* SILK subframe (always
20 ms, or 10 ms in the rare case) per call — a 40/60 ms Opus/SILK frame needs
2 or 3 internal frames, decoded from the same range decoder in a loop with
`new_packet_flag` true only on the first (mirroring libopus's
`while (nSamplesOut < FrameSize)` loop in `opus_decoder.c`). The old code
wrote a single 20 ms internal frame's output into a buffer sized for the
whole 40/60 ms Opus frame, leaving the remainder at its zero-initialized
value. This was invisible to every existing unit/oracle test (all synthetic,
none exercised a real multi-subframe payload end-to-end) and only surfaced
against real bitstreams, where roughly 30% of decoded SILK-only audio was
exact silence in vectors using >20 ms payloads. Fixed by looping
`SilkDecoder::frames_per_packet(payload_size_ms)`'s frame count per Opus
frame, sub-slicing the output buffer per internal frame
(`sub_frame_size = frame_size / n_frames_per_payload`), and sharing one
`RangeDecoder` across the internal frames of one Opus/SILK frame — exposed
`SilkDecoder::frames_per_packet` as `pub(crate)` for this. Also deleted a
dead, never-used `payload_size_ms` computation left over from an earlier
refactor (`decoder.rs`, shadowed by the one now derived from `frame_size`).

**Result**: testvector02/03/04 (pure SILK-only, no CELT/hybrid content) now
decode **bit-exact** — `max |diff| = 0`, `SNR = ∞ dB`, and every packet's
`final_range` matches the encoder's recorded value (1185/1185, 998/998,
1265/1265). This is the first bit-exact conformance result for Opus at all
(CELT's `final_range` bug, below, still blocks CELT-only content).

Full per-vector numbers after the fix (`OPUS_TESTVECTORS_DIR=<path> cargo
test -p tpt-av-cadence-opus --release -- --ignored --nocapture`):

| vector | CELT SNR (dB) | CELT range match | SILK SNR (dB) | SILK packets |
|---|---|---|---|---|
| 01 | -2.3 | 0/2147 | (no SILK) | — |
| 02 | (no CELT) | — | **inf** | 1185 |
| 03 | (no CELT) | — | **inf** | 998 |
| 04 | (no CELT) | — | **inf** | 1265 |
| 05, 06 | (no CELT) | — | (no SILK) | — |
| 07 | -1.5 | 286/4186 | (no SILK) | — |
| 08 | -1.7 | 91/1242 | 0.1 | 5 |
| 09 | 0.2 | 405/1332 | 3.0 | 5 |
| 10 | 0.8 | 174/1598 | (no SILK) | — |
| 11 | -2.4 | 0/553 | (no SILK) | — |
| 12 | (no CELT) | — | 23.6 | 1068 |

**SILK transition-glitch investigation — session update (using the same
oracle-trace method as the CELT fix; root cause NOT yet found, but the
original "state-scaling bug" hypothesis is now RULED OUT with strong
evidence, and a new, more surprising lead was found).**

Extended `silk/decoder_set_fs.c`, `silk/decode_core.c`, and `silk/dec_API.c`
in the same libopus 1.5.2 oracle build (see the CELT section above) with
`SILK_C_FS_DEBUG`-gated dumps of every field the "state-scaling" hypothesis
named as a suspect: `fs_kHz`, `nb_subfr`, `signalType`, `NLSFInterpCoef_Q2`,
`lagPrev`, `LastGainIndex`, `prevGain_Q16`, all four `Gains_Q16`, all four
`pitchL`, and the first 4 `PredCoef_Q12` taps — mirrored in Rust behind the
same env var (`src/silk/synthesis.rs`'s `decode_core`, `src/silk/decoder.rs`'s
`set_fs`/resample call site). Ran both across testvector12's first 7
fs-changes (0→8→12→16→16→12→8→12→16 kHz).

**Result: every single one of these values matches bit-for-bit between C and
Rust, at every transition** — gains, LPC coefficients, pitch, signal type,
NLSF-interpolation flag, all identical. This conclusively rules out
`decode_core`/`decode_parameters`/`set_fs` as the source of any value-level
error at fs-change frames. Went one step further and dumped the resampler's
input (`PRERESAMP`, the raw internal-rate `xq[]` samples from `decode_core`)
and output (`POSTRESAMP`, the 48 kHz samples after `silk_resampler`) at
`dec_API.c`'s resample call site — **also bit-for-bit identical** between C
and Rust for the exact fs=12→16 transition frame at `testvector12` sample
offset 205440 (the same frame flagged with `max_diff=915` against the RFC
`.dec` file). So the full pipeline — decode_core through the resampler — is
proven bit-exact against real libopus 1.5.2 for this frame.

**The genuinely puzzling part**: comparing libopus's own *full-file* decode
(`opus_demo -d testvector12.bit`) against `testvector12.dec` at this exact
sample position shows them agreeing (`(186, 230, 273, 312, ...)`, a clean
ramp) — but the `SILK_C_FS_DEBUG` trace's `POSTRESAMP` dump for what looks
like the very same decode call (immediately preceding, in program order,
the point where this packet's samples would need to be produced) shows a
completely different signal (`(0, 0, 0, ..., 1, 2, 5, 15, 34, 63, 98, 127,
138, 119, 71, 6, -57, ...)`), which is what our own decoder ALSO produces
(bit-exact with C's own trace of the same call, per above). Since both
decoders internally compute the identical "wrong-looking" values at what
appears to be this call site, yet libopus's *final* file output at this
position matches the clean reference, the most likely explanation is that
this particular `silk_Decode` call is not the one whose output ends up at
that timeline position — e.g. an LBRR/redundancy decode pass, or some
other libopus-internal call ordering (`dec_API.c` can call `silk_decode_frame`
more than once per packet) that this crate's much simpler
`decode_silk_only_packet` (one `silk.decode()` call per packet, no LBRR
handling) doesn't replicate. If so, the bug would be in **this crate's
packet-to-SILK-call wiring** (missing an LBRR-aware decode dispatch), not in
`decode_core`/`set_fs`/the resampler, which are now proven correct. This is
a materially different, narrower hypothesis than the "state-scaling" one
this session started with, and directly falsifies it.

Next step for whoever picks this up: instrument `dec_API.c`'s `silk_Decode`
entry (not just `decode_core`/`decode_parameters`) to log every call
(including LBRR-flag decodes and any `condCoding`/`lost_flag` branch) for
the packets around `testvector12` offset 205440, to find how many actual
`silk_decode_frame` invocations libopus makes per packet there and which
one's output is kept — then check whether `decode_silk_only_packet`
(`src/decoder.rs`) needs an LBRR-aware call sequence to match. The oracle
build (`C:\Users\Phillip\AppData\Local\Temp\claude\opus_src\`) has
`SILK_C_FS_DEBUG`-gated traces already wired into `decoder_set_fs.c`,
`decode_core.c`, and `dec_API.c` from this session; the Rust side's mirror
traces are also still in place (`src/silk/decoder.rs`, `src/silk/synthesis.rs`,
gated behind the same `SILK_C_FS_DEBUG` env var) since they're zero-cost
when unset and directly reusable for this exact next step.

**Also discovered this session, independent of the above (and relevant to
interpreting ALL PCM-level conformance numbers, not just SILK's): the RFC
test vectors' `.dec` reference files have drifted from libopus 1.5.2's own
output.** Running `opus_demo -d` (this session's real libopus 1.5.2 build)
and diffing its output against `testvectorNN.dec` directly:
- `testvector07` (CELT-only): libopus's own decode already only reaches
  **82.99 dB** SNR against the RFC reference `.dec` (3905/2170080 samples
  differ, by exactly ±1 unit each) — a real reference decoder does NOT
  reproduce these specific `.dec` files exactly.
- `testvector12` (mixed SILK/hybrid): first divergence between libopus's own
  decode and the `.dec` file is at interleaved sample index 741160 (~15.4s
  in); 134208/2557440 samples differ overall.

This makes sense given the RFC 6716 test vectors were generated by a much
older reference decoder (circa the RFC's 2012 publication) than libopus
1.5.2, which has since accumulated floating-point/algorithmic refinements
that don't change the bitstream format or `final_range` (still bit-exact,
by design — that's the actual, durable conformance contract) but do shift
PCM output at the ~1-LSB level. **This means the 90 dB PCM SNR gate this
project's own test uses is stricter than even libopus 1.5.2 itself can
satisfy against these specific files**, and PCM SNR comparisons against the
bundled `.dec` files should be treated as a coarse sanity check, not a
pass/fail gate — `final_range` remains the correct, version-independent
bit-exactness signal. (Directly comparing this crate's own CELT-only output
against libopus's own decode, rather than the `.dec` file, for
`testvector07` gives the same ~49 dB SNR as the `.dec` comparison, though —
so CELT's residual gap is NOT explained by version drift and is a real,
still-open bug, distinct from the drift explanation that plausibly covers
part of SILK's residual gap.)

Deferred / next:
- Root-cause the remaining CELT PCM SNR gap. Confirmed this session it is
  real (not explained by the `.dec`-file version drift documented above —
  diffing this crate's own CELT output directly against a live libopus 1.5.2
  build gives the same ~49dB for testvector07 as diffing against the `.dec`
  file), and is a float-reconstruction issue (MDCT synthesis, deemphasis, or
  postfilter), not an entropy desync, since `final_range` is 100% bit-exact.
  testvector10 (24.3dB) is the worst outlier and the best starting point;
  the now-working libopus oracle build (see the CELT root-cause section) can
  be reused for a per-sample PCM diff trace the same way it was used for the
  bit-position trace (e.g. dump `out_syn` pre/post `comb_filter`, pre/post
  `deemphasis`, on a known-bad packet from both implementations and diff).
- Root-cause the SILK LBRR/call-ordering hypothesis described just above —
  the state-scaling theory is now ruled out; the new lead is that
  `decode_silk_only_packet` may need an LBRR-aware multi-call sequence per
  packet, matching `dec_API.c`'s `silk_Decode`, rather than one
  `silk.decode()` call per packet.
- When comparing any Opus PCM output against the bundled `.dec` files going
  forward, keep in mind they don't even match libopus 1.5.2's own decode
  (see the version-drift finding above) — `final_range` is the reliable
  bit-exactness signal; treat PCM SNR as a coarse sanity check only, and
  prefer diffing against a live libopus build when chasing a specific PCM
  discrepancy.
- Bit-exact conformance vs libopus binaries (rather than just the
  recorded final range in the test vectors) no longer needs a new
  toolchain — this session found Visual Studio 2022 Build Tools (with
  bundled CMake+Ninja) already installed and used it to build libopus
  1.5.2 as a real oracle; see the CELT root-cause section for the exact
  build steps. Reuse that approach directly instead of treating this as
  blocked.
- Implement the hybrid SILK+CELT merge (low-band SILK + high-band CELT)
  now that both sub-decoders exist independently — note testvector08/09's
  tiny SILK-only segments (5 packets each, poor SNR) are likely SILK-only
  runs sandwiched between hybrid packets this crate still skips, so their
  state may be legitimately discontinuous until hybrid is wired up; revisit
  their numbers once hybrid decoding exists rather than treating them as a
  separate bug.

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
