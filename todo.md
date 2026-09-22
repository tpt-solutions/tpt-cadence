# tpt-cadence — Project TODO

Tracks all tasks for the whole project, organized by phase. See `DESIGN.md` for full design rationale.

Status snapshot: the Opus crate now decodes **all 12 official RFC 6716 test vectors end-to-end with a 100% `final_range` match on every packet (16,073 packets)** through a new full top-level `OpusDecoder` (a port of libopus's `opus_decoder.c` state machine: SILK-only/CELT-only/hybrid packets, mode-transition crossfades, 5 ms CELT redundancy frames, hybrid low-band mixing, DTX/PLC). Hybrid mode is implemented; the SILK fs-change transition glitch and the LBRR/call-ordering hypothesis are resolved (the unified `opus_decode_frame` port decodes testvector12's mixed stream at 110 dB SNR vs a live libopus 1.5.2 build). Three real decoder bugs were found and fixed this session: (1) the CELT spectral-LCG seed initialized to 1,000,000 instead of libopus's 0, (2) hybrid band folding at `start_band == 17` reading past the norm buffer (the reference's overlapping fold-source/fold-output regions), (3) the missing CELT→hybrid transition concealment frame (testvector10's once-per-second hybrid packets decoded with a silent first 2.5 ms; 31 dB → 105.6 dB SNR vs libopus). Versus a live libopus 1.5.2 build the decoder now reaches 73–110 dB SNR on every vector (vectors 02–04 bit-exact); the residual is float-ULP noise (`exp`/`renormalise` accumulation order), the same class as the documented libopus-vs-`.dec` drift. The conformance gate is `final_range` (100% required); PCM comparisons are reported per vector, including an oracle comparison when `OPUS_ORACLE_PCM_DIR` points at a libopus decode. Workspace totals are historical; MP3 revalidated separately; `cargo deny check licenses` passes clean. The Opus crate additionally ships the RFC 7845 Ogg container layer (`src/ogg_opus.rs`: `OpusHead`, `OggOpusReader: FormatReader`, `OggOpusDecoder: Decoder` with pre-skip/end-trim granule handling and linear seek), bit-exact end-to-end for a SILK-only vector muxed through Ogg pages.

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
- [x] Implement `tpt-av-cadence-aac` from scratch (ISO/IEC 14496-3), replacing the kinetix migration plan — **COMPLETE (AAC-LC)**: whole-stream PCM conformance vs FFmpeg >100 dB SNR / <=1e-5 peak on both bundled fixtures and on live FFmpeg round trips. See "AAC-LC — root causes found and fixed" below.
- [x] Conformance tests against ITU-T AAC reference vectors — covered by FFmpeg-reference conformance instead: bundled fixtures + live FFmpeg encode/decode round trips at >100 dB SNR / <=1e-5 peak (`tpt-av-cadence-aac/tests/conformance.rs`), the same external-oracle standard the MP3 suite uses. Official ISO/IEC ITU reference-vector playback remains open as a future hardening task (the FFmpeg gate already exercises ADTS framing, PNS, M/S stereo, EIGHT_SHORT/LONG_START/LONG_STOP transitions, and non-common-window CPEs).

### AAC-LC — root causes found and fixed (session summary)

After prior sessions had verified the element order, the direct O(M^2) IMDCT
(TDAC-verified), the FFmpeg packed-idx Huffman semantics, and the window
sequences, the remaining ~59 dB-whole-file error came down to THREE
independent bugs, isolated by writing a NumPy forward synthesis from the
decoder's own coefficient dumps (proving synthesis self-consistent at
112 dB, i.e. the divergence had to be in the coefficients) and by reading
the actual FFmpeg 6.1/7.1 sources line-by-line (`aacdec_proc_template.c`,
`aacdec_dsp_template.c`, `aactab.c`, `kbdwin.c`):

1. **Codebooks 5/6 decoded through the wrong branch.** ISO codebooks 5/6
   are SIGNED pairs (sign baked into a 9-entry value table
   {-6.35, -4.33, -2.52, -1, 0, 1, 2.52, 4.33, 6.35}, reference
   `codebook_vector4_vals` + `VMUL2`, NO sign bits in the bitstream). The
   decoder instead ran them through the unsigned-pairs-with-sign-bits
   branch and looked the values up in the 7-10 value table. The packed
   index table's nnz nibble is 0 for these books, so no phantom sign bits
   were consumed (no desync — which is why the corruption was invisible:
   plausible magnitudes, wrong values). Symptom: error concentrated in the
   17-20 kHz bins wherever books 5/6 appeared.
2. **PNS noise was phase-inverted.** FFmpeg's `sf` is negative for ALL
   bands (normal AND noise) because its av_tx MDCT scale is positive — the
   sign is a global convention, not noise-specific. This decoder carries
   the global −1 inside the IMDCT kernel instead, so the correct noise
   scale is POSITIVE 2^(sfo/4); the port copied the reference's negative
   sign anyway, double-negating the noise. Symptom: a least-squares
   inversion of the frame-0 output error into coefficient space showed
   noise bands with relative error ~2.0 (the exact signature of a sign
   flip) while every Huffman band sat at ~1e-4.
3. **Non-common-window CPEs parsed each channel twice.** For
   `common_window == 0`, each channel's ICS (with its own ics_info) must be
   parsed exactly once; the code decoded both channels and then ran a
   second `decode_ics` pass over each, consuming the NEXT element's bits as
   section/scalefactor/spectral data — eventually rejecting valid frames
   with "too many bitstream sections" (the 43-frame crash on `test.aac`).
   Common-window pairs (the common case) skipped the double parse, which is
   why most frames decoded fine.

Method notes worth keeping: the FFmpeg-vs-us diff chain was (a) numpy
forward synthesis from own dumps to bisect coefficients-vs-synthesis, (b)
window-weighted least-squares inversion of the PCM error into coefficient
space to identify WHICH bands diverge, (c) `-aac_pns 0` re-encodes to rule
PNS in/out of the steady-state error, (d) direct source reads of every
ambiguous FFmpeg macro (`VMUL2S`'s `sign >> 1 << 31` consumes the SECOND
read bit for dim0; `SHOW_UBITS(nnz) << (cb_idx >> 12)` positions the single
pair sign bit by the "only second value non-zero" table flag; the KBD window
alpha^2 IS 4*(alpha*pi/n)^2 with the sqrt inside `bessel_i0`). Result:
`tone.aac` 123.77 dB / 1.76e-6 peak, `test.aac` 123.39 dB / 2.09e-6 peak,
fresh FFmpeg round trips 124.08/124.61 dB; 16 crate tests + full workspace
suite green; strict Clippy and fmt clean.

Continuation work (same session): the `from_config` raw-block path now
decodes successive raw_data_blocks instead of stopping after the first —
with a refill-and-retry path for blocks straddling the 16 KiB frame buffer
refill, where a failed speculative parse restores the PNS LCG state and the
per-channel window-shape flags so the retry decodes from identical state
(raw framing is verified bit-identical to ADTS framing on a 3x stream fed
in 64-byte reads). The FFmpeg round-trip conformance now covers 44.1/48/
32/24/16/8 kHz in mono and stereo (123.6-125.5 dB), exercising every
scalefactor-band offset table. 18 crate tests total; strict Clippy and fmt
clean.

Second continuation: PCE (channel configuration 0) support — in-band
program_config_element parsing, an ordered channel plan, dynamic StreamInfo
channel count, and element-order-to-channel mapping (verified by crafting
ADTS streams with a PCE prepended to real fixtures: output matches the
plain decode exactly for both a mono SCE plan and a stereo CPE plan). Fixed
configuration 7 to carry EIGHT channels (7.1) — it was misread as seven.
Multichannel output for the fixed configurations 3-7 now follows the WAV
channel order (the bitstream is front-center-first); the per-configuration
mappings were derived and verified empirically against FFmpeg using
distinct per-channel tones, and the FFmpeg round-trip conformance now
covers 4.0/5.0/5.1/7.1 at 124.9-126.1 dB SNR. 19 crate tests; strict
Clippy and fmt clean.

Third continuation — real-world conformance: the official ISO/IEC AAC-LC
conformance items mirrored in FFmpeg's FATE suite now run as an opt-in
test (`AAC_FATE_SAMPLES_DIR`), demuxed from their MP4 containers in-test
and fed through the raw/`esds` entry point. Four items pass at full
fidelity (al04 128.3 dB, al05 125.7, al17 129.5, al18 121.7); the
multichannel CCE/PCE items (al06/al07/al15/al22) decode with correct
structure and correlation at partial fidelity (2-53 dB), limited by
residual coupling/PCE-interaction differences, including an FFmpeg
reference quirk (al06's duplicate PCE tags make FFmpeg itself drop the
front-center channel). This work also fixed a REAL parse bug: the data
stream element count is EIGHT bits with a 255 escape (not four bits with
a 15 escape) — the misreading desynchronized every frame with a
non-trivial DSE, which is what blocked all these streams. CCE (coupling
channel) support is now implemented: target lists, per-band gains, and
application at all three reference coupling points. PCE-configured
streams additionally output in the sniffed WAV channel order (reference
`sniff_channel_order` semantics: class positions, stable sort).

SBR (HE-AAC) is now implemented — the fourth major effort of this crate.
A new `sbr` module ports the reference decoder line-for-line: bitstream
parsing (header/grid/dtdf/invf/envelope/noise/harmonics, ten Huffman
tables), frequency-table derivation (master/derived tables, patch
construction, limiter bands), dequantization with coupled-stereo balance,
envelope estimation, gain calculation with limiter boost, chirp inverse
filtering, HF generation/assembly with smoothing and sinusoid addition,
and the 64-band QMF analysis/synthesis pair on a dedicated 64-point MDCT
matching the reference transform's exact semantics (f64 accumulation).
Detecting an SBR fill element doubles the output rate and stages 2048
samples per channel; decode() stays allocation-free. Verification went
deeper than the usual SNR loops: the QMF filterbank and the full
frequency-table derivation were checked against an INDEPENDENT BUILD of
the reference C implementation (av_tx + aacsbr + sbrdsp compiled
standalone), matching coefficient-for-coefficient, and those values are
pinned as unit tests. Four real port bugs fell out of the line-by-line
audit: the QMF analysis result was never committed to the per-channel
history (the low band was built from two-frames-old data), the sinusoid
addition branch used wrong band indexing and a wrong sign derivation, the
noise-floor index was double-incremented per envelope, and the patch
construction inner loop read the next master-band entry before testing
the loop condition (the C tests the PREVIOUS sb first) — which failed
patch construction on every reset and silently collapsed HE-AAC to pure
upsampling. Whole-file SNR against FFmpeg's HE-AAC decode went from 0 dB
(pre-fix) to ~22 dB, and that fidelity is gated (>20 dB) in the FATE
conformance test alongside structural checks for the 5.1 and full-rate
SBR samples.

Remaining SBR gap: the ~22 dB residual is uniform across frames and
bands (coherence ~0.999 in the lowest core bands, degrading with
frequency; ~0.88 in the enhancement band). PS (HE-AACv2) remains
unimplemented; 5.1/7.1 HE-AAC applies only the first element's SBR
payload (one SBR context per decoder, not per channel element).

**Session update — thorough line-by-line audit against real reference
source; root cause narrowed but not conclusively identified.** Baseline
re-measured fresh before touching anything:
`AAC_FATE_SAMPLES_DIR=<dir containing al_sbr_cm_48_2.mp4> cargo test -p
tpt-av-cadence-aac --test conformance fate_he_aac_sbr_sample` →
**SNR=21.86 dB** (matches the previously recorded ~22 dB; the sample
itself is fetchable straight from `https://fate-suite.ffmpeg.org/aac/`,
which is reachable from this environment — no need to hunt for a cached
copy). This session did not have a working standalone-C-oracle build
left over from a prior session (its temp dir, like the Opus one, was
gone), and rebuilding FFmpeg itself (not just `opus_demo`-style CMake
target) from source on Windows is a materially bigger lift than the
Opus oracle build was, so instead of an instrumented trace diff, this
session pulled the **actual current FFmpeg reference source** directly
(`aacsbr_template.c`, `aacsbr.c`, `sbrdsp.c`, `sbrdsp_template.c`,
`aacsbrdata.h` from `raw.githubusercontent.com/FFmpeg/FFmpeg/master/
libavcodec/`) and did a line-by-line formula diff against every stage
of the Rust port, which is a strictly stronger check than a numeric
trace diff for catching logic bugs (it catches anything that would
*ever* produce a different formula, not just bugs that happen to fire
on this one frame). Every one of the following was checked expression-
by-expression against the current reference and found to match exactly
(not "close" — identical formulas, identical index arithmetic,
identical operand order where operand order matters for float
semantics): `sbr_dequant`, `sbr_lf_gen`, `sbr_hf_inverse_filter`
(including the `dk` cancellation-prone division), `sbr_chirp`,
`sbr_hf_gen`'s patch/`g`-index loop (the exact loop whose off-by-one
was one of the four bugs fixed when SBR was first brought up),
`sbr_x_gen`, `sbr_mapping`, `sbr_env_estimate`, `sbr_gain_calc` (the
limiter/envelope-interaction code previously flagged as the leading
suspect — it matches the reference's `sbr_gain_calc` in
`aacsbr_fixed.c`'s float sibling line for line, including the
asymmetric `g_temp[i+h_SL]`/`q_temp[i]` indexing in the smoothing
branch), `sbr_hf_assemble` (including the reset/smoothing history
memcpy branches and the sinusoid-addition `A`/`B` sign derivation,
verified against the bit-twiddling reference form `B = (A^(-idx)) +
idx`), and every `sbrdsp.c` kernel (`sum_square`, `sum64x5`,
`neg_odd_64`, `qmf_pre_shuffle`, `qmf_post_shuffle`, `qmf_deint_bfly`,
`autocorrelate`, `hf_gen`, `hf_g_filt`, `hf_apply_noise` incl. the four
`phi_sign` variants). The QMF analysis/synthesis windowing (`sbr_qmf_
window_ds` vs. the Rust port's `SBR_QMF_WINDOW_US[2*j]` decimation) was
independently spot-checked against the actual reference table values
fetched from `aacsbrdata.h` and confirmed exact (`ds[j] == us[2*j]` for
every checked index) — not just "previously verified" as the prior
note said, but re-verified this session against literal reference
constants. **This rules out every previously-open candidate in the
"known open gaps" list except (c), the accumulation-order/precision
difference.**

Runtime instrumentation (temporary, `SBR_DEBUG`/`SBR_DEBUG2` env-gated
`eprintln!`s in `apply()` and `hf_inverse_filter`, added and then fully
reverted this session — no trace left in the tree) on the actual
`al_sbr_cm_48_2` fixture found: `alpha0`/`alpha1` magnitudes stay well
inside the `>=16.0` stability cutoff (peak ~2.0, vs. the 4.0 magnitude
bound), ruling out the filter hitting its explicit instability guard.
But `hf_inverse_filter`'s `dk = phi[2][1][0]*phi[1][0][0] -
(phi[1][1][0]^2+phi[1][1][1]^2)/1.000001` denominator does show real
(not catastrophic) cancellation on this stream — 10-50% of `term_a`
cancels against `term_b`, i.e. `dk` is systematically 2-10x smaller
than either operand, which is inherent to the *reference's own*
algorithm (the C source carries its own "Warning: This routine does
not seem numerically stable" comment on this exact function) rather
than a port defect — cancellation of this magnitude exists in the
reference's arithmetic too. The mechanism that plausibly turns a small
(~1e-5 relative, per the existing QMF unit test's documented tolerance)
upstream precision difference into the observed ~20 dB gap: (1) this
port's 64-point inverse MDCT (`Mdct64::inverse`, `qmf.rs`) accumulates
in `f64` via direct O(N^2) summation, while the reference's `av_tx
AV_TX_FLOAT_MDCT` is an actual FFT (different operation *order*, not
just different precision — an FFT's rounding pattern cannot be
reproduced by a direct-summation reimplementation even at matching
precision), so `X_low` differs from the reference by irreducible
float-rounding noise at the ~1e-5 relative level; (2) that noise feeds
`hf_inverse_filter`'s cancellation-prone division, amplifying it
further into `alpha0`/`alpha1`; (3) critically, `bw_array` (the chirp
bandwidth) is **persistent per-band state carried frame-to-frame**
(`sbr_chirp` exponentially smooths each frame's derived bandwidth
against the previous frame's, 75/25 or 90.625/9.375 blend), so this
isn't a single-frame perturbation that washes out — small per-frame
divergences compound across the chirp's smoothing recursion. This
fully explains the coherence *shape* from the earlier session's
measurement: the passthrough low/core band (`x_low` copied linearly
into `X`, no division, no recursion) stays at 0.999 coherence, while
every band that passes through `hf_inverse_filter` → chirp → `hf_gen`
→ `gain_calc`'s nonlinear (sqrt, per-band limiter clamp, gain-boost
clamp) correction accumulates the compounding error, landing at 0.88.

**This is a plausible, mechanistically-grounded explanation, not a
confirmed root cause** — it was not validated against a real
byte-exact C oracle trace (no standalone FFmpeg build was attempted
this session; doing so on Windows, unlike the CMake-based `opus_demo`
oracle, needs a full MSYS2/mingw or WSL toolchain, which is a
materially larger lift than remaining session budget allowed). Per
this project's own established discipline (see the CELT `final_range`
historical record below, and the Opus CELT `freq * 2.0` empirical-
fix rejection), **no fix was applied**: swapping `Mdct64` to `f32`
accumulation, or any other precision tweak, would be exactly the kind
of unexplained-until-verified change that section warns against —
without a real oracle trace confirming it narrows the `X_low` delta
*and* that the narrower delta propagates to a measurably smaller `dk`
divergence and a higher final SNR, changing the MDCT's precision is a
guess, not a fix, however well-motivated the mechanism above sounds.

**Ruled out this session** (in addition to the frequency tables/QMF
filterbank/kernels already pinned as unit tests): every DSP/control-
flow formula in the SBR pipeline from bitstream-parsed parameters
through to the synthesis QMF's windowed overlap-add, checked against
the live reference source, not just memory of it.

**Next forensic step, more concretely scoped than before:** build a
real FFmpeg (not just `opus_demo`-style single-target CMake — needs
MSYS2/mingw-w64 or WSL on this machine) with `getenv`-gated `fprintf`
checkpoints in `sbr_hf_inverse_filter` (dump `phi`, `dk`, `alpha0`,
`alpha1` for the same `k`/frame index used here) and mirror them behind
`SBR_DEBUG2`-style env vars in the Rust port (the instrumentation
pattern is already proven out and reverted cleanly this session, so
re-adding it is fast); diff the two traces for the *same* input frame
to get a real, not hypothesized, number for how far `X_low`/`alpha`
diverge and whether that divergence's magnitude and growth-over-frames
shape is consistent with the compounding-`bw_array` mechanism above. If
confirmed, the actual fix is almost certainly replacing `Mdct64`'s
direct-summation kernel with a real radix FFT matching `av_tx`'s
algorithm (not just flipping the accumulator to `f32`), since operation
*order* is what needs to match, not just precision.

Other remaining gaps (hardening, not blockers): the four multichannel
FATE items' residual (al06/al07/al15/al22, 2-53 dB): frame-level
forensics narrowed it to the coupling/intensity interaction in PCE
multichannel streams — the failing channels are exactly the coupling
targets (e.g. al07 frame 189+: FL and the back pair degrade while the
uncoupled FR/


## Phase 3 — Modern Compressed

- [x] Implement Opus packet parser (RFC 6716 §3: TOC, codes 0–3, padding, DTX, 120 ms cap, config tables)
- [x] Implement the bit-exact range coder (RFC 6716 §4.1 decoder + §5.1 encoder: decode/update, icdf, bit_logp, raw bits, uint, tell) — groundwork shared by SILK and CELT
- [x] Implement CELT decoder (MDCT-based, music-optimized)
- [x] Wire CELT-only packets into a top-level packet decode path (`src/decoder.rs`, `decode_celt_only_packet`) — TOC → (start/end band, stream channels, frame size) mapping, multi-frame packets, DTX/PLC; not the full `Decoder` trait (still needs SILK/hybrid)
- [x] Root-cause the CELT `final_range` desync bug — found and fixed this session by building a real libopus 1.5.2 oracle (MSVC/CMake/Ninja, already installed as VS Build Tools — no new system software needed) and diffing an instrumented trace against it; see "CELT `final_range` desync — ROOT CAUSE FOUND AND FIXED" below. All 6 CELT-containing test vectors now hit 100% range-coder match (was 0-30%). Residual PCM SNR gaps (24-98dB, below the 90dB gate) remain — a separate, much smaller float-reconstruction issue, not an entropy desync.
- [x] Implement SILK decoder (speech-optimized, LP-based) — see task breakdown below; wired into `decode_silk_only_packet`. This session ran it against the official RFC 6716 test vectors for the first time, found and fixed a real multi-subframe (40/60 ms payload) decode bug, and got 3 of 12 vectors to bit-exact `final_range` match — see the "SILK conformance — session update" subsection under Phase 3 below
- [x] Integrate hybrid SILK+CELT mode — done as part of the full top-level `OpusDecoder` (`src/decoder.rs`): SILK decodes at 16 kHz internally, CELT decodes bands 17+ from the same range decoder, outputs are summed (`pcm += pcm_silk/32768`), and hybrid redundancy/transition handling mirrors `opus_decode_frame`
- [x] Conformance test harness against the official Opus test vectors (`tests/conformance.rs`, `#[ignore]`d — see below); found and fixed a real packet-parser bug, and found (but has not yet root-caused) a residual CELT decoder bug
- [x] Top-level `Decoder`/`FormatReader` trait impls via an Ogg Opus (RFC 7845) container — `src/ogg_opus.rs` provides `OpusHead` parsing (mapping families 0/1, trivial 1–2 channel mappings), `OpusTags` validation, pre-skip + per-completing-packet end-trim granule bookkeeping, Q7.8 output gain, and `OggOpusDecoder`/`OggOpusReader` implementing the core traits over a `.opus`/`.ogg` byte source (decode-and-discard seek; first chained link only). The Ogg page layer itself moved from the Vorbis crate into the new shared `tpt-av-cadence-ogg` crate. `tests/ogg_opus.rs` covers pre-skip/end-trim counts, determinism, seek-0 replay, mid-stream seek continuity (bit-identical continuation), unseekable-source seek rejection, and an `#[ignore]`d test that muxes an official SILK-only vector into Ogg pages and checks bit-exact PCM against the bundled `.dec` (passing). Also fixed `OpusDecoder::decode_frame`'s three `Vec::to_vec` crossfade allocations so `decode()` upholds the real-time safety contract. Design note: the end trim is applied per completing packet (opusfile's model) — an EOS page carrying no packets cannot trim audio already emitted, so the reader relies on muxers setting the EOS flag on the final audio page.

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

Deferred / next (updated after the session that ported the full
`OpusDecoder`, implemented hybrid mode, and fixed the three decoder bugs
below):
- RESOLVED: hybrid SILK+CELT merge — implemented in the top-level
  `OpusDecoder` (`src/decoder.rs`); vectors 05/06 (hybrid-only) decode
  with 100% `final_range` and 91+ dB SNR vs live libopus.
- RESOLVED: the SILK fs-change/LBRR call-ordering hypothesis — the unified
  `opus_decode_frame` port (single persistent decoder state, correct
  `silk_ResetDecoder`/payload bookkeeping across mode switches) decodes
  testvector12's mixed stream at 110 dB SNR vs live libopus; the old
  per-mode decode path with its manual state bookkeeping was the problem.
- RESOLVED: CELT PCM SNR gap's GROSS component — three real bugs found via
  a stage-by-stage oracle trace (dump `fq`/`syn`/`post`/`pcm` per frame
  from both implementations and diff hashes):
  1. `CeltDecoder::new` seeded the spectral LCG with `rng = 1_000_000`;
     libopus's init `OPUS_CLEAR`s the whole decoder state (and
     `DECODER_RESET_START` is `rng`, so resets clear it too). Fix: `rng: 0`.
  2. Hybrid folding (`start_band == 17`) at `i == start + 1`: the fold
     source (`norm + effective_lowband`) and the fold output
     (`norm + M*eBands[i] - norm_offset`) OVERLAP (eff=0, n=12M, out at
     8M); the reference reads the source before the output is written (or
     routes it through `lowband_scratch`). The port's disjoint-slice
     helper panicked; `fold_buffers` in `celt/bands.rs` now snapshots the
     source into the scratch buffer when the regions overlap.
  3. CELT→hybrid transitions: `opus_decode_frame` decodes a 5 ms PLC frame
     in the outgoing CELT mode into `pcm_transition` when
     `transition && mode != MODE_CELT_ONLY && !redundancy`, copies its
     first 2.5 ms over the output and `smooth_fade`s the next 2.5 ms. The
     port only had the CELT-side transition; testvector10's once-per-
     second hybrid packets started with 2.5 ms of silence (31 dB →
     105.6 dB SNR vs libopus after the fix).
- RESOLVED (session 2026-09-21): the "accepted float-fidelity gap" on
  vectors 07/08/09 (37-56 dB SNR vs live libopus) was NOT float-ULP/SIMD
  noise — it was a real, findable bug, found by rebuilding the libopus
  1.5.2 oracle from scratch (the prior oracle's temp dir was gone; this
  session installed the VC++/CMake/Ninja workload via the VS installer,
  since the BuildTools install here had only the base shell) and
  stage-hash-diffing per-frame `X`/`syn`/`postpf`/`pcm` and, once that
  narrowed it to a `cm` (collapse-mask) corruption in one specific band,
  a per-call-site trace of `quant_band`'s recombine/time-divide bit
  bookkeeping. First falsified hypothesis: disabling libopus's SIMD
  kernels entirely (`-DOPUS_DISABLE_INTRINSICS=ON`) reproduced the exact
  same 37-56 dB gap byte-for-byte, ruling out the documented
  SSE-rounding-order theory outright (and 37-56 dB is far too large an
  error to be ULP noise regardless — that argument was wrong on its
  face). **Root cause**: `quant_band` (`celt/bands.rs`)'s post-recursion
  `cm` mask used the pre-time-divide-undo `b0` (`let b_final = b0 <<
  recombine`) instead of the loop-mutated `b_blocks` the undo loop left
  behind. The reference (`celt/bands.c`) reuses one `B` variable across
  both the time-divide-undo loop and the final `B<<=recombine`, so it
  naturally sees the post-undo value; the port's shadowed `b_blocks` in
  the undo loop never fed back into `b_final`. After 3 rounds of
  time-divide (common on transient content within an otherwise
  non-transient long frame, via a very negative per-band `tf_res`), the
  mask went from the correct 1 bit wide to a stale 8 bits wide, leaking
  spurious high bits of `cm` into the fold-mask (`fill`) computation for
  *later* bands — corrupting their noise/fold decisions and hence their
  decoded spectrum, despite `final_range` staying 100% bit-exact
  throughout (this bookkeeping never touches the entropy coder). Fixed
  by masking with the loop's own `b_blocks` post-loop value. Result:
  01 73.4→106.7 dB, 07 49.4→85.2 dB, 08 37.5→101.8 dB, 09 55.8→101.8 dB,
  10 105.7→105.7 dB (unchanged), 11 99.0→108.4 dB vs live oracle; 05/06/12
  (hybrid/SILK-heavy vectors) unchanged, consistent with the bug being
  CELT-only. All 172 crate unit tests + full RFC 6716 conformance green;
  strict Clippy and `cargo fmt` clean. The oracle build is NOT committed
  (lives under `C:\Users\phill\AppData\Local\Temp\claude\opus_src\`,
  rebuildable per the recipe in the CELT root-cause section above; CMake/
  Ninja obtained via `pip install cmake ninja` rather than the VS CMake
  component this time, since that component wasn't present either).
- When comparing any Opus PCM output against the bundled `.dec` files,
  prefer the live-oracle comparison: set `OPUS_ORACLE_PCM_DIR` to a
  directory of `testvectorNN.pcm` files produced by
  `opus_demo -d 48000 2 testvectorNN.bit out.pcm` from a float build of
  libopus 1.5.2 (VS 2022 Build Tools + CMake/Ninja, recipe in the CELT
  root-cause section above; the .bit/.dec vectors are re-downloadable from
  opus-codec.org). The conformance test prints a per-vector "SNR vs
  oracle" column when the directory is set.

### Opus — remaining open tasks (tracked explicitly)

- [x] Re-run the full RFC 6716 conformance suite with `OPUS_ORACLE_PCM_DIR`
  set (2026-09-21, oracle/test-vector dirs from the prior session still on
  disk at `%TEMP%\claude\opus_testvectors\opus_testvectors` and
  `%TEMP%\claude\opus_oracle_pcm`): **all 12 vectors still hit 100%
  `final_range` match** (the real, version-independent bit-exactness
  contract) and the suite passes. SNR vs oracle: 01 106.7, 05 92.0,
  06 91.3, 07 85.2, 08 101.8, 09 101.8, 10 105.7, 11 108.4, 12 110.2 dB;
  02-04 bit-exact (inf dB). Vector 07 sits under a naive 90 dB reading but
  the test's actual gate is `final_range` + a generous sanity floor (see
  `tests/conformance.rs` doc comment: even libopus 1.5.2 itself only
  reaches ~83 dB against the 2012 reference `.dec` files), so this is not
  a failing gate — no further CELT bug-hunting is required to stay green.
  Sample-level check on testvector07's two worst packets (its lowest,
  24.3/49.8 dB) confirms this is cosmetic, not a bug: max |diff| is only
  3 int16 units against the oracle, occurring during a near-silent onset
  (signal magnitude single digits to ~50 out of 32768) — the low dB
  reading is denominator-starvation (tiny `pkt_sd` energy), not a
  reconstruction error. No further CELT work is warranted here; the
  stage-hash-diff method against the still-present libopus 1.5.2 oracle
  build (`%TEMP%\claude\opus_src\build\opus_demo.exe`) remains available
  if a future session wants to chase the last few LSBs anyway.
- [x] Confirm `tpt-av-cadence-aac` currently compiles as part of the
  workspace — verified 2026-09-21 with `cargo build --workspace`: builds
  clean, no errors. The ~44-error state noted after the hybrid-fold-
  refactor commit was resolved by the later SBR/AAC-test-suite commit.
- [x] Clean up or gitignore AAC investigation scratch artifacts
  (`tpt-av-cadence-aac/blocks.bin`, `my_stripped.f32`, `spans.txt`,
  `stripped.adts`, `stripped_core.f32`) — deleted 2026-09-21 (unreferenced
  by any test/build); the `.gitignore` update excluding this class of file
  is committed alongside.

## Phase 4 — Legacy & Open Source

- [x] Implement `tpt-av-cadence-mp3` (Huffman decoding, polyphase filterbank, joint stereo) — complete: ten-stream FFmpeg PCM conformance at 118.7–119.4 dB SNR / <=1e-5 peak, structural/replay/robustness/CRC suites, allocation-free `decode()` verified by test
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
  - [x] Complete real-time safety and malformed-input audits, including reservoir bounds and mixed-block processing; error paths still allocate. CRC coverage now has synthetic regressions, but broader protected-stream testing remains open. Independent PCM checks now run with FFmpeg 7.1 bundled in `C:\Users\Phillip\AppData\Roaming\Python\Python313\site-packages\imageio_ffmpeg\binaries\ffmpeg-win-x86_64-v7.1.exe` (not on PATH; exposed under the name `ffmpeg.exe` in a temporary PATH directory for validation). Wider feature coverage and official bit-exact conformance remain open.
 Final verification added: `tests/rt_safety.rs` uses a counting global allocator to prove successful `decode()` calls perform ZERO allocations across all ten bundled fixtures (error-path formatting remains the accepted exception).
  - [x] Clear strict Clippy failures: `cargo clippy -p tpt-av-cadence-mp3 --all-targets -- -D warnings` passes. Fixed 23 library diagnostics and two test diagnostics without lint suppressions; coefficient spelling changes preserve f32 bits. All ten FFmpeg comparisons retain their measured SNR/peak errors after cleanup.
  - [x] Real-time safety audit completed (2026-09-19): the decode path is allocation-free (all granule/synthesis scratch preallocated at open; the only allocations are the probe at open and error construction), the reservoir is clamped to 511 bytes with side-info overdraw dropping the frame; the stale "audit pending" doc comment on `Mp3Decoder` was replaced with the audited contract. Added targeted malformed-input regressions: `tests/robustness.rs::reservoir_overdraw_drops_one_frame_and_resynchronizes` (a 9-bit-max `main_data_begin` drops exactly the starved frame with bounded 1–4-frame error propagation through the shared reservoir, never a cascade) and `tests/crc_streams.rs` (CRC-16 protection verified against REAL encoder output: each fixture's first frame is rewritten as CRC-protected — fresh streams' first frames draw no reservoir, so its decode must be bit-identical — and a corrupted protected side-info byte must reject exactly that frame and resynchronize with bounded loss). Documented finding: whole-stream protected rewrites are impossible to synthesize without a CRC-capable encoder — LAME runs the bit reservoir at exactly each frame's `main_data_begin` (essentially zero ancillary slack), so any per-frame 2-byte loss starves `main_data_begin` and correctly drops frames; the per-layout synthetic matrix in `streaming.rs` remains the acceptance/rejection gate.
- [x] Implement `tpt-av-cadence-vorbis` (MDCT-based OGG Vorbis decoder) — **COMPLETE and conformance-tested (2026-09-19)**. Decode path fixed against real libvorbis streams with FOUR root causes, each isolated by building an instrumented libvorbis 1.3.7 oracle (MinGW) plus a Python spec transcription and diffing per-stage bit positions: (1) the codebook header consumed a nonexistent 16-bit version field (spec layout is sync/dim/entries with no version), rejecting every real stream; (2) residue setup read the cascade vectors and book numbers interleaved — the spec (§8.2) reads ALL cascades first, then the books; (3) long-window audio packets carry exactly TWO flag bits (previous/next window) — a third "window" bit was consumed, corrupting every long-block packet; (4) residue decoding ADDS into the return vectors, which the spec says to zero per packet — only the upper half of the buffer was cleared, so mono (type-1) streams accumulated stale spectral values (13 dB SNR; stereo's type-2 path zeroed its own scratch and masked the bug). After the fixes: bundled-fixture conformance vs FFmpeg references passes at **136–138 dB SNR with sample-exact lengths** on six fixtures (mono/stereo, 32/44.1/48 kHz, quality −1…4, impulse-train block switching), `seek(0)` replays bit-identically, mid-stream seek rejoins the uninterrupted decode sample-exactly (seek re-parses headers, resumes only at true audio pages, and tracks per-page stream positions), and malformed inputs never panic. Stale CHANGELOG/README replaced; VORBIS_TRACE/debug scaffolding removed; crate clippy `-D warnings` clean.
- [x] Conformance tests against mpg123 conformance streams (MP3) — **closed as gated-harness + documented finding (2026-09-19):** the ISO/IEC 11172-4 conformance set is not freely downloadable (mpg123 SVN `test/` dir only; mirrors omit it), so `tests/iso_conformance.rs` provides the ready harness (runs every `*.mp3` in `MP3_CONS_DIR` against the FFmpeg decode at the suite's external gate) and is ignored until the streams are supplied. The external-oracle standard (FFmpeg subprocess, >100 dB SNR / <=1e-5 peak across all ten bundled streams) already provides the same fidelity gate. mpg123 ships no conformance bitstreams; the ISO/IEC 11172-4 conformance set is no longer freely downloadable, and the FFmpeg samples mirror (`samples.ffmpeg.org/A-codecs/MP3/`) is a crasher/stress collection, not the conformance set. The external-oracle standard (FFmpeg subprocess, >100 dB SNR / <=1e-5 peak across all ten bundled streams) already provides the same fidelity gate; revisit if the ISO set resurfaces or a CRC-capable MP3 encoder becomes available.
- [x] Conformance tests for Vorbis — `tests/conformance.rs` passes: 136–138 dB SNR vs FFmpeg on six bundled fixtures with sample-exact lengths, seek(0) bit-identical replay, mid-stream seek exact rejoin, malformed-input no-panic sweep.

## Opus hybrid fold refactor — COMPLETE (2026-09-20)

The in-flight refactor from 2026-09-19 evening is finished and the full
conformance gate is met: **all 12 official RFC 6716 vectors decode with
100% `final_range` match on every packet**, and the SNR-vs-oracle numbers
equal the pre-refactor values (01: 73.5, 05: 92.0, 06: 91.3, 07: 49.4,
08: 37.5, 09: 55.8, 10: 105.7, 11: 99.0, 12: 110.2 dB; vectors 02–04
bit-exact PCM). What closed it:

1. **Open-ended lowband threading.** The band recursion now receives the
   fold source and `lowband_out` as open-ended slices into the shared
   per-channel norm buffer (`norm0[eff..]`, `norm0[o..]`), mirroring
   libopus's raw-pointer semantics (`norm + effective_lowband`,
   `norm + M*eBands[i] - norm_offset`). The previous exact-width slices
   truncated at band/split boundaries, so the pre/post transforms
   (`haar1`/`deinterleave_hadamard` in `quant_band`) indexed past them —
   the `bands.rs:230` len-80/index-80 panic on vector06. The four
   per-band match arms in `quant_all_bands` (dual-stereo ×2, stereo,
   mono) now share one shape: a disjoint fold source/output pair is one
   `split_at_mut(o)`; the hybrid overlap (band `start+1`) snapshots the
   source into the scratch buffer first (reads precede the end-of-band
   output writes exactly as in the reference), and the snapshot doubles
   as the band's transform scratch, so no separate scratch is passed
   down for that band. `two_mut` (the error-on-overlap placeholder) is
   deleted; `quant_band`'s `need_copy` copies the full band width again
   (guaranteed available now).
2. **The three lost SSE-order rounding experiments were NOT re-applied**:
   the gate is met exactly without them. The deterministic SIMD orders
   that matter are already ported (`comb_filter_const` lane order in
   `celt/pitch.rs`, `celt_inner_prod` fold in `celt/vq.rs`), and the
   residual 07/08/09 gap was later found (2026-09-21, see Phase 3) to be
   a real `quant_band` collapse-mask bookkeeping bug, not float-ULP noise.
3. **Clippy**: the concurrent session's `compute_theta` triangular-pdf
   region was restructured to if-expression initializations
   (`needless_late_init` ×2) and the mono fold arm drops its redundant
   `as_deref_mut` (`needless_option_as_deref`); `tests/rt_safety.rs`'
   unused trait import removed. `tpt-av-cadence-vorbis`'s rt_safety
   harness had one `collapsible_if`.

**Real-time safety restored for Opus.** The new counting-allocator
`tests/rt_safety.rs` (zero allocations per successful `decode_packet`)
exposed three per-frame allocation sources, all fixed:

1. `celt/rate.rs`'s `compute_allocation` allocated three `Vec`s per
   frame; `AllocationResult` now carries `[i32; NB_EBANDS]` arrays (the
   `CeltDecoder` consumer already stored fixed arrays).
2. `std::env::var_os` debug-trace checks in the decode path
   (`CELT_BAND_TRACE` in `celt/bands.rs` — evaluated per band and per
   recursion leaf — plus `SILK_C_FS_DEBUG`/`SILK_DBG` in `silk/`)
   allocate on EVERY call on Windows even when unset. New
   `src/debug.rs` resolves all flags once at construction (every public
   decoder constructor calls `debug::init()`), and the hot path reads a
   plain bool out of a `OnceLock`. `CELT_BAND_TRACE=1` output is
   unchanged.
3. The mode-transition cross-fade's `pcm[n..2*n].to_vec()` became a
   stack array (matching its two neighbouring branches).

Verified: zero allocations per decode call in the release build, 172
crate unit tests + full RFC 6716 conformance green, strict Clippy and
`cargo fmt` clean. The `tpt-av-cadence-aac` crate is EXCLUDED from the
workspace-green claim — it currently does not compile (mid-flight edits,
~44 errors) and belongs to the AAC workstream.

## Cross-Cutting (ongoing, applies to every phase)

- [x] Enforce real-time safety contract per decoder (alloc-free/lock-free/panic-free `decode()`; all allocation confined to `init()`/`open()`) — WAV/AIFF/FLAC/PCM audited; MP3 verified by test (`tests/rt_safety.rs`, counting allocator, zero allocations on successful decodes across all ten fixtures; error-path formatting remains the accepted exception); Vorbis/Opus/AAC decoders preallocate all scratch at open and return `Result` everywhere
- [x] Fuzz testing (`proptest` never-panic property tests) for every new parser — WAV, AIFF, FLAC (arbitrary + mutated real streams), Opus packets/range coder, Vorbis packets, MP3/MPEG-1/2 fixtures — plus `cargo-fuzz` targets under `fuzz/` (`wav_decode`, `aiff_decode`, `flac_decode`, `mp3_decode`, `opus_packet`, `opus_decode` full-decoder, `opus_ogg_decode` — also asserting seek(0) replay determinism — `vorbis_stream`, `ogg_pages`, `aac_adts`). Run from `fuzz/`: `cargo +nightly fuzz run <target>`; the targets also build and run their inputs on stable/MSVC.
- [x] Bit-exact validation harness (`assert_bit_exact_vs_ffmpeg`) wired for every new decoder — WAV covered; FLAC (all bundled subset/uncommon fixtures) and AIFF (synthetic 8/16/24/32-bit mono/stereo) now cross-checked BIT-EXACT against FFmpeg in `tests/ffmpeg_crosscheck.rs` (skips without FFmpeg; `CADENCE_REQUIRE_FFMPEG=1` fails instead)

## Phase 5 — Platform Review Follow-Up (bugs/gaps/adoption, 2026-09-21)

Full review notes: `C:\Users\phill\.claude\plans\review-platform-for-bugs-compiled-ullman.md`. Tracked here so items don't get lost back into prose.

### Known open correctness gaps (carried over from earlier sections, re-flagged for visibility)
- [ ] AAC SBR fidelity gap: ~22 dB SNR residual vs reference (re-measured this session: 21.86 dB on `al_sbr_cm_48_2`). Every DSP/control-flow stage (dequant, hf_inverse_filter, chirp, hf_gen, mapping, env_estimate, gain_calc, hf_assemble, QMF analysis/synthesis, all sbrdsp kernels) line-by-line audited against live FFmpeg reference source this session and found to match exactly — ruling out a logic bug in any of those stages. Leading remaining candidate: the 64-point inverse MDCT's direct-summation (f64) implementation vs. the reference's actual FFT (`av_tx`) produces small (~1e-5 relative) `X_low` differences that compound through `hf_inverse_filter`'s cancellation-prone division and `bw_array`'s frame-to-frame exponential smoothing — plausible and mechanistically grounded, but NOT validated against a byte-exact C oracle trace, so not applied as a fix. See "Remaining SBR gap" above for the full account and the concrete next step (real FFmpeg build + instrumented trace diff).
- [ ] PS / HE-AACv2 unimplemented (PS payload parsed but skipped; decodes as mono)
- [ ] Multichannel HE-AAC (5.1/7.1): only the first channel element's SBR payload is applied (one SBR context instead of one per channel element)
- [ ] AAC multichannel CCE/PCE fidelity: 4 FATE vectors (al06/al07/al15/al22) at 2-53 dB, coupling/PCE interaction bugs not root-caused
- [ ] Broader ISO/IEC AAC conformance suite playback (only 4 FATE-mirrored vectors currently pass)
- [ ] MP3 ISO/IEC 11172-4 official conformance vectors still "not obtainable" — MP3 correctness rests solely on FFmpeg-oracle comparison
- [x] Audit the 8 files containing `panic!(` workspace-wide to confirm none are reachable from untrusted decode() input paths (real-time-safety contract requires decode() to never panic) — see "Workspace-wide panic-safety audit (2026-09-22)" below
- [x] Close out or remove the windowing/CCE-PCE TODO comment at `tpt-av-cadence-aac/src/decoder.rs:37` — the comment was stale (claimed "CCE/PCE rejected at parse time," but both have been fully implemented, with dedicated `decode_cce`/`decode_pce` handlers, since earlier AAC-LC sessions); replaced with a one-line note pointing at the real handlers

### Workspace-wide panic-safety audit (2026-09-22)

Widened beyond the original "8 files containing literal `panic!(`" scope (all 8 turned out to be in `tests/`/`examples/` or `#[cfg(test)] mod tests` blocks — never reachable from production `decode()`) to also cover `.unwrap()`/`.expect(`/`unreachable!(` and untrusted-length-driven indexing/arithmetic across every crate's `src/`, since those are equally real panic sources the literal-text scope would have missed. ~230+ sites traced for reachability from bitstream-attacker-controlled input. Three genuine, previously-unknown panics were found and fixed, all confirmed via the crate's own test/clippy/fmt gates plus (where available) the existing malformed-input/never-panic fuzz-style tests:

1. **Opus** (`tpt-av-cadence-opus/src/decoder.rs`, `decode_frame`): Hybrid-mode `redundancy_bytes` (bitstream `decode_uint(256)? as usize + 2`, range 2..=257) was subtracted from the remaining payload `len` with a plain `usize` subtraction and no bound against `len` — a crafted small Hybrid packet claiming a large redundancy byte count could underflow and panic in debug builds. Fixed with `saturating_sub`, matching the existing convention used two lines below (`RangeDecoder::shrink_storage`). 19 other `.unwrap()`/`.expect()`/`unreachable!()` sites across `celt/` and `silk/` were traced and confirmed genuinely safe (icdf/codebook-bounded indices, invariant-guarded unwraps such as `dual_stereo⇒stereo`, or encoder-only/non-bitstream code paths) — left unchanged.
2. **Vorbis** (`tpt-av-cadence-vorbis/src/floor.rs`, Floor1 `render_point`-equivalent decode): the spec (§7.2.3) clamps the linear-prediction `predicted` value to `[0, range)` *before* deriving `lowroom`/`highroom`; this crate's port was missing that clamp. With a small `adx` (two adjacent setup-time `x` values 1 apart) and large packet-controlled amplitude deltas, `predicted` could land far outside `[0, range)`, and the subsequent `as u32` cast + `* 2` overflowed — reachable on every floor1-using packet. Fixed by adding the spec's clamp. Several other sites got `debug_assert!`s documenting invariants that live in a different file (`header::parse_setup`'s setup-time validation) rather than being locally obvious. Two non-panic issues were flagged but left unfixed as out of scope: a potential infinite-loop DoS in `residue.rs::decode_type01` if a codebook declares `dimensions == 0` (never explicitly rejected at setup), and leftover debug `eprintln!`s in `header.rs::parse_setup`'s production path.
3. **AAC** (`tpt-av-cadence-aac/src/decoder.rs`, `apply_coupling_method`): CCE (coupling channel element) parsing lets a malformed stream declare up to 8 coupling targets, several of which can carry `ch_select == 3` and each bump a running gain-set `index`/counter. The *writer* side (`decode_cce`) already bounds-checked its gain-array writes against this overflowing past 7, but the *reader* side (`apply_coupling`/`apply_coupling_method`) indexed/sliced the same fixed-size `gain` array with no bounds check — reachable straight from `AacDecoder::decode()` on a crafted CCE. Fixed by adding the same guard pattern already used on the writer side (bounds check + early return, no signature change needed since the function returns `()`). A close read of the rest of the crate (adts/audio_specific/huffman/tns/pns/stereo/imdct/all of `sbr/`) found no other reachable panics — `sbr/`'s many `.unwrap()`s on Huffman-table construction operate on the module's own fixed compile-time `(bits, codes)` data, never bitstream values, and `sbr/freq.rs`'s frequency-table derivation already has explicit `return false`/`turnoff()` guards on every stage that could otherwise overflow a fixed-size table.
4. **pcm/wav/aiff/flac/ogg/mp3**: zero reachable panics found across all six crates, including a deep manual trace of MP3's Huffman/bit-reservoir/scalefactor code (the highest-risk crate structurally, given variable-length codes and a bit reservoir spanning frames) — every dynamic index there is either explicitly bounds-checked or algebraically provable in-range from header-validation invariants established earlier in the same decode. Three `.expect()`/`.unwrap()` sites in `wav/src/decoder.rs`, `aiff/src/decoder.rs`, and `ogg/src/lib.rs` got explanatory comments (no behavior change) since their safety depends on a same-function `Err`-before-flag-set ordering that isn't obvious without reading the whole match arm.

Verification: full workspace `cargo test --workspace` (every crate, 0 failed), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo fmt --check` (clean) — re-run and confirmed independently after reconciling all four crate-group audits' diffs together (they ran concurrently as separate sessions against the same working tree with no file overlap, so no merge conflicts arose).

### Newly discovered while building the CLI (2026-09-21) — not previously tracked
- [x] **AAC/SBR decode uses enough stack to overflow a 1 MiB default main-thread stack (Windows).** Confirmed via `AacDecoder::open` + `decode()` on the bundled `tpt-av-cadence-aac/tests/data/test.aac` fixture (a real, valid ADTS file — `cargo test`'s conformance suite decodes it fine at 123 dB SNR, because the test harness runs on a thread with a larger default stack than `main`). Running the exact same decode from a plain `fn main()` binary (the existing `aac_dump` example, and the new `cadence` CLI before its workaround) reliably overflows the stack and aborts the process. **Root cause, actually isolated this time** (via `llvm-readobj --unwind` on a release build, correlating per-function stack-frame sizes back to symbols with `llvm-symbolizer` — Windows x64's `.pdata`/`UNWIND_INFO` records each function's frame allocation, which turned out to be a far better signal than eyeballing source for big arrays): the SBR-locals hypothesis in the paragraph above was only half right and not the dominant term. The real culprit was `AacDecoder`'s `sbr: Option<sbr::Sbr>` field — `Sbr` embeds two `Mdct64`s (`[[f64; 64]; 32]` cosine tables, 16 KB *each*, 64 KB total) and two `SbrChannel`s (`g_temp`/`q_temp` alone are `[[f32; 48]; 42]`, 8 KB each) **by value**, making `Sbr` itself >100 KB and, because `Option<T>` doesn't box, making that size part of `AacDecoder`'s own layout — paid by every stack frame holding an `AacDecoder`, whether or not the stream ever uses SBR. Measured with a throwaway `stack_size`-binary-searching probe (spawns a thread with a caller-chosen stack size, decodes `test.aac` fully, checked as a child process per size since a Windows stack overflow is an uncatchable SEH abort): **release minimum was ~449 KB before, ~65 KB after** (debug: ~1.3 MB before, <40 KB after). Fix (`tpt-av-cadence-aac/src/decoder.rs`, `src/sbr/mod.rs`, `src/sbr/qmf.rs`): boxed `AacDecoder::sbr` (`Option<Box<sbr::Sbr>>`); boxed `Mdct64`'s two cosine tables; and, as a secondary cleanup, hoisted `Sbr::apply()`'s own per-call locals (`w`/`z`/`y1`/`out`, ~40 KB, the thing the original hypothesis was about) into struct-resident scratch fields (`qmf_analysis_z`, `y1_scratch`) or wrote straight into their already-struct-resident destinations, all zero-allocation (`Option::take`/`Some` pointer swaps, not `Box::new` per call) to preserve the crate's alloc-free `decode()` contract. Verified: `cargo test -p tpt-av-cadence-aac` green (unit + conformance + robustness + `rt_safety`'s counting-allocator zero-allocation check, all release), `cargo clippy -p tpt-av-cadence-aac --all-targets -- -D warnings` and `cargo fmt --check` clean. The `qmf_analysis`/`qmf_synthesis` bit-exact-vs-reference unit tests (which exercise the boxed `Mdct64` directly) still pass; full HE-AAC SBR FATE-sample conformance couldn't be re-verified end-to-end in this environment (`AAC_FATE_SAMPLES_DIR` unset here, so that suite's SBR cases skip rather than run) — the `Sbr::apply()` change is a pure data-relocation (same values, different storage), not a logic change. `tpt-av-cadence-cli`'s 32 MiB decode-thread `stack_size` was deliberately left untouched: it's shared across every codec the CLI can decode, not just AAC, so this crate's much smaller number isn't evidence it's safe to shrink workspace-wide.
- [x] **FLAC decoder had unconditional per-frame debug `eprintln!` calls in the hot decode path** (`tpt-av-cadence-flac/src/decoder.rs`, 4 call sites: header-reject, subframe-fail, CRC mismatch, and a "success" print firing on *every single frame*). This meant any normal, error-free FLAC decode spammed stderr — thousands of lines for a multi-second file — and performed unconditional stdio I/O inside `decode()`, contradicting the crate's stated allocation/lock-free real-time-safety contract (discovered because the new `cadence decode` CLI surfaced the noise immediately on a bundled fixture). Fixed: all 4 removed; full `tpt-av-cadence-flac` test suite (27 conformance tests + unit + FFmpeg crosscheck + doctest) still green after removal.

### Quick wins (in progress this session)
- [x] Add `examples/decode.rs` to `tpt-av-cadence-wav`
- [x] Add `examples/decode.rs` to `tpt-av-cadence-aiff`
- [x] Add `examples/decode.rs` to `tpt-av-cadence-flac`
- [x] Add `examples/decode.rs` to `tpt-av-cadence-mp3`
- [x] Add `examples/decode.rs` to `tpt-av-cadence-pcm` (CLI-driven format selection since raw PCM carries no header)
- [x] Fix stale `"(under construction)"` description in `tpt-av-cadence-mp3/Cargo.toml:3`
- [x] Fix stale `"(under construction)"` description in `tpt-av-cadence-vorbis/Cargo.toml:3`
- [x] Wire the 9 existing `fuzz/fuzz_targets/` into a scheduled (nightly, 03:00 UTC + manual `workflow_dispatch`) CI job — `.github/workflows/ci.yml` `fuzz` job, matrix over all 9 targets, 5-minute budget each, corpus caching, crash-artifact upload on failure

### Adoption / usability (from review §5)
- [x] Top-level "quickstart per format" table in root README linking to each crate's example — added decode-example and encode-example tables, verified against actual `examples/` filenames
- [x] `cargo generate` template or documented starter snippet for "decode any supported format to PCM" — did the documented-snippet option (cargo-check-verified), pointing to `tpt-av-cadence-cli/src/main.rs` as the canonical full implementation; no scaffold template added
- [x] Ecosystem comparison table vs. `symphonia`/`hound`/`minimp3-rs` (why this crate suite vs. the incumbents) — added, plus a shorter note on `claxon`/`lewton`/`audiopus` for format-specific comparisons
- [x] CHANGELOG.md — added at repo root, dated-milestone format derived from git log + todo.md session logs
- [ ] Revisit CONTRIBUTING.md's no-PRs policy — at minimum consider carving out example/doc PRs

### Automation / CI (from review §3)
- [ ] Benchmark tracking (`criterion` + `benches/` + perf regression detection in CI)
- [ ] Release automation (version bump/tag/changelog, e.g. `cargo-release` or `release-plz`) — deferred since crates.io publishing is explicitly out of scope for now
- [ ] Coverage reporting (`cargo-llvm-cov` or `cargo-tarpaulin`) + badge

### Innovation candidates (from review §4)
- [x] Unified CLI tool (auto-detect format, decode/inspect/transcode-to-WAV) — `tpt-av-cadence-cli` (`cadence` binary), `info`/`decode` subcommands, extension-based detection with content-sniffing for Ogg (Vorbis vs Opus); ships a minimal hand-rolled 16-bit PCM WAV writer since no encoder crate exists yet
- [ ] WASM build feasibility spike (`wasm32-unknown-unknown` + minimal JS demo)
- [ ] Per-format Cargo feature flags (opt into only needed codecs, smaller binary size)
- [ ] Machine-readable per-crate capability matrix (e.g. `capabilities.json`)
- [ ] Conformance dashboard generated from the existing SNR/bit-exactness test harness output

### Encoders (from review §2 — patent/royalty-screened; user-confirmed order)
- [ ] **Opus encoder** (user-confirmed first target — hybrid SILK/CELT encoding, bitrate control, psychoacoustic tuning; reuses existing range coder/CELT/SILK decode infrastructure). **In progress** — CELT encoder now supports mono *or stereo*, fullband, CBR, 20 ms frames (independent-per-channel stereo, no M/S or intensity coupling). See "Opus CELT encoder — foundation (2026-09-21)" below for the mono foundation and "Session log (2026-09-22, continued): stereo support" for the stereo work.
- [x] WAV/AIFF/PCM writers (near-trivial, no compression, zero patent surface) — see "WAV/AIFF/PCM writers (2026-09-22)" below
- [x] FLAC encoder (royalty-free by design, well-specified reference encoder to port/adapt) — see "FLAC encoder (2026-09-22)" below
- [ ] Vorbis encoder (royalty-free by design, higher effort — psychoacoustic model)
- [ ] AAC encoder — **on hold**: Fraunhofer/VIA-LA patent pool primarily targets encoders; needs a licensing decision from the user before any implementation work
- [ ] MP3 encoder — core patents expired worldwide by 2017 (broadly considered safe), but confirm before shipping if there's commercial distribution. **Partial progress (2026-09-22):** `tpt-av-cadence-mp3::Mp3Encoder` lands a real, spec-compliant, bit-reservoir-free CBR encoder — bitstream validity is independently verified (FFmpeg decodes its output cleanly), but audio fidelity is currently poor due to an unresolved analysis-filter/synthesis-filter mismatch. See "MP3 encoder (2026-09-22)" below for what's confirmed-correct vs. the specific open problem and what was tried. Leaving this unchecked until the fidelity gap closes.

### WAV/AIFF/PCM writers (2026-09-22)

Added a shared `Encoder` trait (`tpt-av-cadence-core/src/encoder.rs`) mirroring
`Decoder` on the write side: `encode(&mut self, samples: &[f32]) -> Result<usize>`
takes interleaved `f32` input and returns frames consumed; `finish(&mut self)`
flushes/finalizes (e.g. patching header size fields once the total is known)
and is idempotent. Added `f32_to_int` to `tpt-av-cadence-core/src/sample.rs`
as the exact inverse of the existing `int_to_f32` (round-to-nearest with
clamping to the depth's representable range, total/panic-free on NaN/Infinity
input) so every writer shares one scaling implementation with every decoder.

- **`tpt-av-cadence-wav::WavEncoder`**: RIFF/WAVE writer, matching the
  decoder's full format range (8-bit unsigned PCM, 16/24/32-bit signed PCM,
  32/64-bit IEEE float, any channel count). Always emits the classic 16-byte
  `fmt ` chunk (not `WAVE_FORMAT_EXTENSIBLE` — unnecessary since this crate's
  own decoder, and every other reader tested, accepts the classic chunk for
  any channel count/bit depth). `finish()` seeks back to patch the RIFF and
  `data` chunk sizes once the total byte count is known; a `Drop` impl
  best-effort-finalizes if the caller forgets.
- **`tpt-av-cadence-aiff::AiffEncoder`**: AIFF-C writer (`FORM 'AIFC'`),
  same format range as the WAV writer (8/16/24/32-bit signed big-endian PCM
  as compression type `NONE`, 32/64-bit float as `FL32`/`FL64`). Always uses
  the AIFF-C chunk layout (COMM with a compression-type + empty Pascal-string
  name, FVER chunk) even for integer PCM, since classic `FORM 'AIFF'` COMM
  chunks have no compression-type field at all and thus can't represent float
  output — and this crate's own `AiffDecoder` (which doesn't actually
  branch on the FORM type, only on COMM's declared size and compression tag)
  accepts the AIFF-C layout for both. Reuses `ext_float::f64_to_extended`
  (already implemented for the decoder) for the sample-rate field. `finish()`
  patches the FORM size, COMM's `numSampleFrames`, and SSND's chunk size.
- **`tpt-av-cadence-pcm::PcmEncoder`**: headerless writer — no container at
  all, so `new()` needs only `Write` (no `Seek`, unlike WAV/AIFF) and
  `finish()` is just a flush. Supports the same `PcmFormat` (sample format ×
  byte order × channels × rate) the decoder already takes.

All three are round-trip tested (encode through the writer, decode through
that same crate's own already-conformance-tested decoder, assert bit-exact
reconstruction for every integer depth and float width) rather than tested
in isolation — the strongest correctness check available for uncompressed
formats. `examples/{wav,aiff,pcm}_encode.rs` added to each crate (mirroring
the existing `examples/{wav,aiff,pcm}_decode.rs`), each writing a one-second
440 Hz sine tone. Full workspace `cargo test --workspace` (0 failed),
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`,
and `cargo deny check licenses` (no new dependencies) all clean.

One test-writing lesson worth keeping: an initial `f32_to_int` round-trip
test asserted exact round-tripping through `int_to_f32` for arbitrary 32-bit
integers, which is actually impossible — f32's 24-bit mantissa can't
represent every 31-bit integer magnitude exactly, so `int_to_f32` itself is
already lossy at that depth for values that aren't power-of-two-aligned (the
pre-existing `int32_scaling` test already only spot-checked
power-of-two-friendly values for the same reason, which was easy to miss
when writing the new inverse-function test). Fixed by restricting the 32-bit
case to power-of-two-aligned test values, matching that existing convention,
rather than by weakening `f32_to_int` itself (which is correct).

### MP3 encoder (2026-09-22)

Added `tpt-av-cadence-mp3::Mp3Encoder` (`src/encoder.rs`), implementing the
shared `Encoder` trait: MPEG-1 Layer III, fixed CBR bitrate, long blocks
only, independent (LR) stereo. This is the hardest of the encoders shipped
so far because MP3's *bitstream syntax* is fully specified (headers, side
info, Huffman tables, bit reservoir) but the *transform pipeline* an
encoder must invert is not directly available: this crate's decoder keeps
its synthesis filter's coefficients pre-permuted/optimized for its own fast
algorithm rather than in plain per-tap form, so an encoder can't just "run
the decoder backward" the way FLAC's fixed predictors can.

**Confirmed correct (independently verified, not just self-consistent):**
- **Huffman encode tables** (`build_huff_table`/`walk`): mechanically
  derived by walking the decoder's own `HUFF_TABS` two-level lookup
  automaton forward, rather than transcribing a second copy of the ISO code
  tables — guarantees the encode and decode tables can never disagree.
  Verified in `huffman_encode_table_round_trips_through_decoder_tables` by
  literally replaying the decode automaton on every emitted codeword.
- **Forward 36-point MDCT** (`Mdct36Basis`): derived *algebraically*, not
  guessed. Two closed-form kernel candidates already sitting in
  `imdct.rs`'s (unasserted — it only prints, never asserts) debug test
  both turned out to be wrong when actually checked with assertions — a
  useful reminder that an unasserted "verification" test isn't one. Instead
  this reads `imdct36`'s source directly: it splits into a `sum` (this
  call's own read-out contribution) and `ownov` (this call's contribution
  to the *next* call's read-out) via an invertible 18x18 linear map from
  the spectral input (confirmed via `imdct::probe_dct3_9` that the
  underlying `dct3_9` is exactly the textbook DCT-III). Given that map's
  inverse, the correct forward-MDCT matrices come out as simple column
  scalings (worked out via the per-index 2x2 rotation system the sine
  window induces). Verified against the real production
  `crate::imdct::imdct_gr` end-to-end
  (`forward_mdct36_inverts_decoder_imdct_after_overlap_add`, <1e-3 error on
  random data) plus two isolating sanity checks
  (`mdct_window_is_normalized`, `mdct_basis_l_round_trips`).
- **`antialias`/`change_sign` pre-compensation**: the decoder applies both
  unconditionally (antialias to the spectral data before IMDCT, a fixed
  per-band-boundary rotation; change_sign to the IMDCT's time output,
  negating odd samples of odd bands) — missing either was an actual bug
  found this session (see below). Both are now pre-compensated on the
  encoder side (antialias via its rotation's exact inverse/transpose,
  change_sign via a self-inverse pre-negation) and verified exact —
  `debug_all_bands_two_granules` (removed before landing, but reproducible
  from this description) checked reconstruction through the *entire*
  antialias+IMDCT+change_sign chain for 7 representative bands including
  odd ones, at ~1e-7 relative error (float32 precision), against directly
  recomputed ground truth.
- **Bit-reservoir-free CBR framing**: every frame is self-contained
  (`main_data_begin = 0`); this is spec-legal (a decoder never reads past
  `part2_3_length` bits per granule, so unused trailing frame bytes are
  simply never touched) and sidesteps the reservoir accounting entirely.
  `big_values` is trimmed to the last non-all-zero pair (free bit saving on
  quiet content — the decoder already zero-fills anything past
  `big_values`), and `trim_to_budget` provides a hard backstop that
  guarantees the CBR byte budget is never exceeded even when the coarsest
  representable `global_gain` (255) still doesn't fit — found and fixed a
  real bug here this session: `global_gain` is *increasing* in
  dequantization gain (I initially had the direction backwards, causing
  `choose_global_gain`'s binary search to pick the wrong end and overshoot
  the frame budget by ~10x before the fix).
- **Bitstream validity independent of this crate's own decoder**: FFmpeg
  (`tests/encoder_ffmpeg_crosscheck.rs`) decodes this encoder's output
  cleanly across mono/stereo, three sample rates, and two content types —
  the strongest correctness signal available for a lossy bitstream, since
  it rules out the failure mode where this crate's own decoder happens to
  accept something subtly malformed because it shares a bug with the
  encoder.

**Known, unresolved limitation — audio fidelity is currently poor:**
The subband analysis filter (`analyze_block_polyphase`) was originally a
generic Hann-windowed-sinc approximation (see the superseded three-fix
account below); this was replaced in a follow-up session with a more
principled attempt, described here, which narrowed the problem
significantly but still did not close the gap.

**Follow-up session (2026-09-22, continued): isolated the bug to the
analysis filter specifically, verified the implementation against the
actual reference source (not memory), still doesn't reconstruct.**

1. **Isolation test** (`analysis_filter_alone_round_trips_through_synth`,
   `#[ignore]`d, kept as the regression target): feeds
   `analyze_block_polyphase`'s raw subband output directly into
   `crate::synth::dct_ii`+`synth_granule`, skipping `forward_mdct36`/
   `imdct_gr`/quantization/Huffman entirely (all independently verified
   elsewhere — see above). This isolates the question to exactly two
   functions: `analyze_block_polyphase` vs. `crate::synth`. Result: still
   poor correlation (~0.24 for a 1 kHz tone, best delay ≈480-490 samples —
   suspiciously close to the polyphase filterbank's well-known ≈481-sample
   textbook delay, so the *timing* is roughly right but the *shape* is not).
   Confirms the bug is in this pair specifically, not in the MDCT/quant/
   Huffman chain (which the earlier session's own diagnosis already
   suspected but hadn't isolated with a dedicated test).
2. **Transcribed the real ISO/shine `Ci`/`enwindow` 512-tap table**
   (`tables::ANALYSIS_WINDOW`) and re-implemented `analyze_block_polyphase`
   to match the actual reference algorithm, rather than a generic window —
   this is the "empirically extract the divide-out-modulation approach was
   too fragile" alternative the previous session's account recommended
   trying. Verified the *implementation itself* against the live `shine`
   encoder source (`github.com/savonet/shine`, `src/lib/l3subband.c`,
   fetched and read directly, not recalled from memory) line by line:
   - Sample fill order: shine's `for(i=32;i--;) x[off+i]=*ptr++` writes the
     32 new samples in *reverse* (x[off+31]=newest-read-first,
     ..., x[off+0]=last-read) — tried both this reversed order and the
     naive forward order; **made no measurable difference to the
     correlation** (0.2435 either way on the isolation test with a pure
     sine probe). This is a real but inconclusive experiment: a sine wave
     is time-symmetric, so it cannot actually distinguish the two
     orderings — a mistake in the experiment design, caught by re-running
     with an impulse instead (see below), not by the sine result itself.
   - Fold formula (`y[i] = sum_{m=0..7} x[off+i+64m] * window[i+64m]`):
     matches shine's `s_value` accumulation exactly.
   - Offset update (`off = (off+480) mod 512`): matches shine's
     `off = (off+480) & (HAN_SIZE-1)` exactly.
   - Matrixing (`out[k] = sum_i y[i] * cos((2k+1)*(i-16)*pi/64)`):
     algebraically identical to shine's
     `fl[i][j] = cos((2i+1)*(16-j)*pi/64)` (cosine is even, so `(i-16)` and
     `(16-j)`-with-swapped-role give the same values).
   So the transcribed implementation is a faithful port of shine's
   algorithm, verified against real reference source — yet the isolated
   round trip still doesn't reconstruct well.
3. **Probed `crate::synth`'s own true per-band impulse response directly**
   (`synth_single_band_impulse_response_diagnostic`, an unasserted
   diagnostic test kept for whoever continues this): set one subband's one
   time-sample to 1.0 with everything else zero, ran `dct_ii`+
   `synth_granule` with no analysis filter involved at all, and inspected
   where the resulting PCM energy lands. Result: a real, bounded, clean
   impulse response spanning ~487 samples, but with its *dominant*
   magnitude concentrated near the *end* of that span (index ~495-497 of a
   [22,508] span) rather than symmetric/centered the way the transcribed
   `ANALYSIS_WINDOW` table's shape is — suggesting `crate::synth`'s
   internal representation (derived from a `minimp3`-style fast/folded
   synthesis algorithm, not a plain per-tap FIR) may not correspond to the
   plain ISO/shine model in the direction or normalization this session
   assumed, even though the *matrixing formula itself* checks out
   algebraically. This was not resolved further.

**Honest status:** the analysis filter is now a verified-faithful port of
a real reference algorithm (a strictly better foundation than the earlier
generic-window attempt), and the bug has been narrowed to a specific,
small, two-function isolation test — but the actual remaining defect is
still unidentified. Per this project's established discipline (see the AAC
SBR investigation and the CELT `final_range` history for the same
pattern), no further guessing was done once the two most-likely hypotheses
(sample-fill direction; formula transcription error) were checked and
ruled out. **Next step for whoever picks this up:** don't trust the
shine/ISO model's applicability to `crate::synth` on faith — instead,
numerically construct the *exact* adjoint of `crate::synth`'s real,
already-trusted transform (probe multiple bands/time-slots the way
`synth_single_band_impulse_response_diagnostic` does for band 0, build up
the full per-band impulse response set, and derive the analysis filter
from *those measured responses* rather than from the separately-sourced
ISO table) — the same strategy that worked for the forward MDCT
(`Mdct36Basis`/`probe_l`) in this same encoder. The isolation test and both
diagnostic tests (`analysis_filter_alone_round_trips_through_synth`,
`synth_single_band_impulse_response_diagnostic`,
`analysis_filter_impulse_response_diagnostic`) are kept in
`src/encoder.rs`'s test module specifically to make this tractable without
re-deriving the isolation harness from scratch.

<details>
<summary>Superseded: original three-fix account from the first session
(generic Hann window era, kept for history)</summary>

The subband analysis filter was originally a Hann-windowed sinc lowpass
folded across 4 stacked 64-sample phases — a *generic* approximation, not
the ISO reference's actual prototype filter. This session tried three
fixes, in order, before running out of time:
1. A longer, better-sidelobe (Blackman vs. Hann) prototype — made it
   *worse* (energy concentration ratio dropped, not rose).
2. Empirically extracting the *real* prototype by probing the decoder's
   own `crate::synth::dct_ii`+`synth_granule` with a unit impulse and
   dividing out the known modulation — theoretically sound, but the
   recovered filter produced an *unbounded-growing* reconstruction.
3. Interpolating across the division's zero-crossing instabilities instead
   of clamping through them — reduced but did not eliminate the growth.
Reverted to the generic (Hann, 4-phase) prototype (stable but low-fidelity)
rather than ship something worse. Superseded by the follow-up session
above, which replaced the generic window with a verified-faithful port of
the real reference algorithm — a real improvement in rigor, even though
the fidelity gap itself is not yet closed.

</details>

The two perceptual-fidelity tests that would catch a real fix
(`mono_sine_tone_decodes_with_concentrated_energy`,
`stereo_white_noise_round_trips_recognizably` in
`tests/encoder_roundtrip.rs`) remain `#[ignore]`d with an explanation
rather than deleted or weakened to pass.

Also observed, not yet root-caused: FFmpeg's `mp3float` decoder logs
non-fatal "overread, skip ..." warnings for some frames of low-bitrate
(128kbps), low-entropy stereo content (e.g. an identical sine tone in both
channels) specifically — reproduced manually, does not occur at 320kbps for
the same content, nor for stereo white noise, nor for mono sine at 128kbps.
It still recovers and decodes the correct sample count (verified in
`low_bitrate_stereo_sine_still_decodes_correct_frame_count`), and this
crate's own decoder accepts the same bitstream without any warning. Left
as a documented open question rather than investigated further given time
already spent on the fidelity issue above.

Explicitly out of scope (by design, not time pressure): MPEG-2/2.5 (LSF)
sample rates, VBR, block-switching/short blocks, mid-side/intensity
stereo, and any psychoacoustic model — see `src/encoder.rs`'s module doc
comment for the full rationale, matching this project's established
"correct but reduced feature set" pattern for encoders (FLAC's fixed-only
predictors, CELT's non-transient mono scope).

Verification: `cargo test -p tpt-av-cadence-mp3` (all decoder tests
unaffected; encoder unit tests for the Huffman table, MDCT derivation, and
quantizer table selection; `tests/encoder_roundtrip.rs` for structural
validity/decodability across bitrates/sample rates/mono/stereo/silence/
short-stream edge cases; `tests/encoder_ffmpeg_crosscheck.rs` for
independent-decoder acceptance), full `cargo test --workspace`, `cargo
clippy --workspace --all-targets -- -D warnings`, and `cargo fmt --check`
all clean; no new dependencies. Added `examples/mp3_encode.rs`.

**Honest summary**: bitstream validity is solid (independently verified via
FFmpeg, not just self-consistent); audio fidelity is not — this is a
real, working, spec-compliant MP3 encoder that currently produces
poor-quality audio due to an unresolved analysis-filter/synthesis-filter
mismatch, not a finished quality-competitive one. Marking the top-level
todo item below `[x]` would overclaim; leaving it open with this note
instead.

### FLAC encoder (2026-09-22)

Added `tpt-av-cadence-flac::FlacEncoder` (`src/encoder.rs`), implementing the
shared `Encoder` trait. Unlike Opus/Vorbis, FLAC's *bitstream* is normative
(it's lossless), so "correct" here means bit-exact reconstruction through a
real decoder, not just "a defensible policy" — verified against both this
crate's own `FlacDecoder` and a live FFmpeg decode (see below).

Implemented:
- Fixed block size (4096 samples; the final frame of a stream may be
  shorter). No variable blocksize / block-switching.
- Subframe types: CONSTANT, VERBATIM, and FIXED predictors (orders 0-4,
  selected per subframe by a sum-of-absolute-residuals heuristic, the same
  cheap proxy the reference encoder uses for this decision). General LPC
  (Levinson-Durbin analysis + coefficient quantization) is **not**
  implemented — the natural next step for better compression, explicitly
  scoped out this session per the task's own guidance that fixed predictors
  alone are a complete, correct first cut.
- Partitioned Rice residual coding: always coding method 1 (Rice2, 5-bit
  parameter) for simplicity rather than choosing between methods 0/1 per
  residual (costs at most 1 extra bit per partition vs. the theoretical
  optimum). Searches partition orders 0..=6 and, per partition, either the
  optimal Rice parameter (bit-cost search over k=0..=30) or an escaped/raw
  partition when that's cheaper (e.g. an all-zero partition costs 0 bits per
  sample via `raw_bits=0`) — not a globally optimal search, but a real,
  correct one, per the task's "doesn't need to be optimal, just valid"
  guidance.
- Correct STREAMINFO metadata block, frame headers (fixed-blocksize framing,
  explicit 16-bit block-size field so the final short frame doesn't need a
  header code lookup table; sample rate and bit depth always signalled via
  STREAMINFO rather than per-frame codes) with CRC-8, and frame footers with
  CRC-16 — reusing the decoder's own `crc8`/`crc16` tables/functions so
  encode and decode can never disagree about the CRC algorithm.
- 1-8 channels, 4-32 bit depth (the same ranges `FlacDecoder` accepts).

Explicitly out of scope this session:
- **No LPC subframes** (see above) — this is the biggest compression-ratio
  gap vs. a reference-quality encoder; fixed predictors alone typically
  reach maybe 50-70% of FLAC's usual compression on music-like signals.
- **No stereo decorrelation** (left/side, right/side, mid/side) — every
  multichannel stream uses INDEPENDENT channel assignment, which is
  spec-valid but leaves stereo-specific redundancy on the table.
- **No wasted-bits detection** — a subframe never declares wasted
  (trailing-zero) bits even when the input has them; this only matters for
  audio that's been bit-shifted up from a narrower source and costs nothing
  on ordinary content.
- **STREAMINFO's MD5 field is left all-zero** (the "not computed"
  convention several real encoders use for this optional field) rather than
  adding an MD5 implementation to this crate; it doesn't affect bitstream
  validity or this crate's own decoder, which never checks it.

Verification: round-trip tests in `src/encoder.rs` (silence, a 440 Hz sine
tone, deterministic white noise, a DC-offset-plus-ripple signal, an
alternating-extremes worst-case-for-low-order-predictors signal, constant
blocks, a partial-final-block stream, 6-channel audio, 8/16/24-bit depths,
tiny/single-sample blocks, and a stream long enough to force a multi-byte
UTF-8 frame number) all assert bit-exact reconstruction through
`FlacDecoder` after quantizing the source to the target bit depth. Beyond
self-round-tripping (which can't catch a bug shared by the encoder and
decoder), `tests/ffmpeg_crosscheck.rs` gained
`flac_encoder_output_decodes_bit_exact_in_ffmpeg`: encodes a synthetic
tone+noise stereo file with `FlacEncoder`, decodes it with a live FFmpeg
subprocess, and asserts the result is bit-exact against the quantized
source — confirming the bitstream is genuinely spec-compliant FLAC, not just
something this crate's own (possibly-buggy-in-the-same-way) decoder happens
to accept. Added `examples/flac_encode.rs` (a one-second 440 Hz stereo tone),
manually verified to both round-trip through `FlacDecoder` and play/decode
cleanly via `ffmpeg -i tone.flac -f null -` (a 46 KB output for 176 KB of
raw 16-bit PCM — real, if modest, lossless compression from the fixed
predictors and Rice coding alone). `cargo test -p tpt-av-cadence-flac` (58
tests total across the crate), full `cargo test --workspace`, `cargo clippy
--workspace --all-targets -- -D warnings`, and `cargo fmt --check` all clean;
no new dependencies.

### Opus CELT encoder — foundation (2026-09-21)

Important scoping note, since it affects how "correct" is defined below: RFC 6716 only specifies the Opus *decoder* — an encoder's internal choices (pulse search heuristic, bit-allocation trim, transient detection, rate control, …) are **not normative**. So "correct" for every piece below means "produces a bitstream the existing (RFC-conformance-tested) decoder reconstructs exactly", not "bit-exact with libopus's own encoder internals". None of this is a port of libopus's `celt_encoder.c`/`quant_bands.c` encode paths (unlike the rest of this codebase, which ports the decoder verbatim) — it's original code built to the same interfaces, verified by round-tripping through the existing trusted decoder rather than against reference vectors.

Landed this session, all in `tpt-av-cadence-opus/src/celt/`, all with passing tests (181/181 in the crate):
- [x] **Forward MDCT** (`mdct.rs::mdct_forward`) — promoted from a test-only port to production `pub(crate)`. Already validated (prior session) against the analytic MDCT definition and round-tripped through `mdct_backward` at >60 dB SNR. Not yet called outside tests (`#[allow(dead_code)]`) — nothing wires it into a frame-by-frame encoder loop yet.
- [x] **PVQ combinatorial index encode** (`cwrs.rs::icwrs` + `encode_pulses`) — the encode-side inverse of the existing `cwrsi`/`decode_pulses`. Uses an existing, already-tested-but-previously-test-only helper (`encode_index`, promoted and renamed `icwrs`) rather than a hand-derived version — it was already exhaustively verified against `cwrsi` for every enumerable small `(n,k)` in `cwrsi_indices_round_trip`. Added a second test, `encode_pulses_round_trips_through_range_coder`, checking real range-coder round trips for larger `(n,k)` (up to n=176, k=120).
- [x] **PVQ pulse search** (`vq.rs::alg_quant`) — a greedy correlation-maximizing search (pyramid pre-projection when `k > n/2`, then one-pulse-at-a-time placement maximizing `(x·y)^2/|y|^2`), original code (not a libopus port — see scoping note above). Tested end-to-end in `alg_quant_round_trips_and_correlates_with_target`: encodes a random target, decodes it back through `alg_unquant`, and checks (a) the decoded pulse vector and resynthesized signal match the encoder's own resynth bit-for-bit, and (b) the result correlates positively with the original target (search-quality sanity check, scaled by `k/n` since very sparse allocations — e.g. 3 pulses over 176 dimensions — necessarily capture little of a near-uniform target).
- [x] **Energy quantization, encode side** (`quant_bands.rs`: `quant_coarse_energy`/`quant_fine_energy`/`quant_energy_finalise`) — mirrors each `unquant_*` counterpart's budget/branch structure exactly (reusing the already-existing, already-tested `laplace::laplace_encode` and `RangeEncoder::{encode_icdf,encode_bit_logp,write_raw_bits,tell}`, all of which turned out to already be implemented and tested — a smaller lift than expected). `quant_coarse_energy` takes the actual per-band target log-energy and picks the integer delta closest to it at whichever budget tier is available (full Laplace / 2-bit icdf / 1-bit / forced), using the *actual* (possibly-clamped) value `laplace_encode` returns so encoder and decoder state can never diverge; fine/finalise pick the raw-bit value closest to the running quantization residual. Tested in `encode_tests::coarse_fine_finalise_round_trip_matches_decoder_state`: encodes a synthetic 2-channel/21-band target spectrum, decodes it back through the real `unquant_*` functions, and asserts the encoder's own end state and the decoder's reconstructed state are bit-for-bit identical (not just close) — plus a sanity check that reconstruction error vs. the original target stays under 1.0 in the log-energy domain. Only the generous-budget Laplace path is exercised so far; the tight-budget icdf/bit/forced fallback branches aren't covered by a dedicated test yet.

Explicitly NOT done — next milestones toward a working `OpusEncoder`, roughly in dependency order:
- [x] **Bit allocation, encode side** (`rate.rs::compute_allocation_encode` + `interp_bits2pulses_encode`). Refactored the shared deterministic setup (bisection over allocation vectors, threshold/trim computation — none of it touches the bitstream) out of `compute_allocation` into a new `compute_bits1_bits2` helper used by both the decode and encode paths, so that logic has one source of truth instead of being duplicated. The genuinely decoder-specific part (`interp_bits2pulses`'s three bitstream reads: per-band skip bit, intensity index, dual-stereo bit) isn't normative — RFC 6716 only specifies the decoder — so `interp_bits2pulses_encode` uses the simplest defensible policy for a first working encoder: never skip a band once there's a real skip decision to make, and no intensity/dual-stereo coupling. (Bands can still end up mechanically skipped when a band's bit budget never clears the threshold at all — that path spends no bit either direction, so encoder and decoder agree automatically without any policy choice.) Tested in `compute_allocation_encode_round_trips_through_decoder`: runs both mono and stereo across all 4 LM values and 4 total-bit budgets (400/1600/6400/16000), asserting the encoder's `pulses`/`ebits`/`fine_priority`/`coded_bands`/`balance`/`intensity`/`dual_stereo` all match what `compute_allocation` (the trusted decoder path) recovers from the emitted bits — passed on the first attempt across all 32 combinations.
- [x] **Per-band spectrum quantization, encode side, mono/non-transient scope** (`bands.rs`: `compute_theta_encode`, `encode_theta_triangular`, `quant_partition_encode`, `quant_band_n1_encode`, `quant_band_encode`) and (`range.rs`: added `RangeEncoder::tell_frac`, which was missing — only `tell()` existed). This is the piece that ties bit allocation + energy quant + PVQ pulse search together into an actual per-band encode, including the recursive binary-split structure CELT uses to place pulses efficiently in wide bands (not just a stereo feature — mono bands split too whenever the bit budget clears a cache-derived threshold, which is mechanical/shared with decode, not an encoder choice). The one genuine per-split encoder decision is the split angle (`theta`): computed from the *actual* norm ratio between the two half-bands (`atan2(|x1|, |x0|)`, matching the decoder's Q14 angle representation) and quantized to the decoder's `qn`-level grid, encoded via `encode_theta_triangular` — the hand-derived exact bijective inverse of `compute_theta`'s triangular-pdf decode search (verified exhaustively for every level 0..=qn across 6 qn values). Explicitly scoped to `stereo == false` and `b_blocks == 1 && tf_change == 0` (debug-asserted) — no transient/TF-split or stereo-coupled paths yet, since neither transient detection nor a stereo encode policy exist. Verified in `bands.rs`'s new `encode_tests` module: `quant_partition_encode_round_trips_through_decoder` (3 band/LM/budget combinations, including one that forces multiple levels of recursive splitting) and `quant_band_encode_round_trips_through_decoder` (covers the `n==1` special case too) both assert the encoder's own collapse mask, quantized spectrum, `lowband_out`, remaining-bit accounting, and LCG seed state are *bit-for-bit* identical to what `quant_band`/`quant_partition` (the real, trusted, RFC-conformance-tested decoder) reconstructs from the emitted bits — all passed on the first attempt.
- [x] **`quant_all_bands`, encode side** (`bands.rs::quant_all_bands_encode`) — the outer per-frame orchestration loop tying bit allocation, energy quantization, and per-band spectrum quantization together across the whole frame. Read the full ~250-line decode version this session before writing anything: for the mono/non-transient/non-hybrid scope this crate's encoder is at, the stereo/dual-stereo/short-blocks branches in `quant_all_bands` turn out to never trigger and the per-band `tf_res` value is always the constant 0 — so rather than threading always-default parameters through, `quant_all_bands_encode` drops `stereo`/`dual_stereo`/`intensity`/`short_blocks`/`tf_res` from its signature entirely and only implements the branches that are actually reachable in that scope. What's left is genuinely shared, bitstream-I/O-free bookkeeping ported unchanged: the per-band bit-budget arithmetic, `lowband_offset`/`effective_lowband` fold-source tracking, and the aliased-buffer overlap-snapshot logic (the trickiest part — when a band's fold-source read range aliases its own `lowband_out` write range, the source must be snapshotted first). The only true encoder step is calling `quant_band_encode` instead of `quant_band`. Verified end-to-end in `quant_all_bands_encode_round_trips_through_decoder`: chains `compute_allocation_encode` into `quant_all_bands_encode` on one range coder (exactly how a real encoder sequences them) across 10 consecutive bands at LM=2, decodes through the real `compute_allocation` + `quant_all_bands` decoder pair, and asserts the encoder's pulse allocation, final spectrum, per-band collapse masks, and folding-RNG seed are all bit-for-bit identical to what the decoder reconstructs — passed on the second attempt (first failure was a missing test-only import, not a logic bug). Also added `RangeEncoder::tell_frac` (range.rs) and a dedicated test proving it tracks `RangeDecoder::tell_frac` step-for-step on the same bitstream, since several pieces this session (`compute_theta_encode`, `quant_all_bands_encode`) depend on that symmetry for correct bit-budget accounting.
- [x] **Transient detection, TF (time-frequency) resolution analysis, and the anti-collapse encode decision** — real (not hardwired) implementations landed 2026-09-22; see "Session log (2026-09-22, continued): transient detection, TF resolution, and anti-collapse" below for the detection heuristic, the `quant_band_encode`/`quant_all_bands_encode` short-block generalization, a real bug found and fixed in `compute_theta_encode` along the way, and the demonstrated pre-echo improvement. TF resolution's per-band search is a known, explicitly-scoped simplification (every band always signals "no change"; only the single frame-level `tf_select` bit is chosen deliberately) — see that session log for exactly what's simplified vs. fully implemented.
- [x] Top-level `OpusEncoder` — **scaffolded and wired up (`tpt-av-cadence-opus/src/celt/encoder.rs::CeltEncoder`); PCM fidelity bug found and fixed 2026-09-22, end-to-end test passing.** Mono/fullband/CBR/20ms only (transient content now supported, see above), matching every scope restriction already established this session. See "Session log (2026-09-22): CeltEncoder PCM fidelity bug found and fixed" below — the root cause was in the end-to-end *test's* decoder setup, not in the encoder or MDCT code.

### Session log (2026-09-21, continued): CeltEncoder built, PCM fidelity bug not resolved

**What's implemented** in `CeltEncoder::encode_frame`: pre-emphasis (the algebraic inverse of the decoder's `deemphasis` filter, derived and checked against the DC/steady-state case by hand), a persistent `overlap`-sample MDCT tail buffer for cross-frame windowing continuity, the full pre-allocation header bit sequence (silence/postfilter/transient/intra_ener flags, each gated on the same `tell()`-budget checks `decoder.rs` uses — this was cross-checked line-by-line against the decoder's read order and 3 real gating bugs were found and fixed, see below), `tf_encode` (writes "no TF change" for every band, matching `tf_decode`'s exact per-band `logp` schedule), `spread_decision`/dynalloc/`alloc_trim` writes, then the full chain: `quant_coarse_energy` → `compute_allocation_encode` → `quant_fine_energy` → `quant_all_bands_encode` → `quant_energy_finalise` → TOC byte + packet assembly (config 31: CELT/fullband/20ms, code 0).

**Bugs found and fixed along the way:**
1. **Missing `m` (`1 << LM` = 8) scale factor** when computing per-band boundaries into the 960-element spectrum array — was indexing with raw `EBAND5MS[i]` instead of `m * EBAND5MS[i]`, so analysis only ever touched the first ~100 of 960 spectrum bins. Real bug, definitely wrong, fixed.
2. **Three header bits written unconditionally** (postfilter, is_transient, intra_ener) instead of gated on the same `tell() + N <= total_bits` budget checks the decoder uses before reading them — would desync the stream at low bitrates where a gate could evaluate false. Fixed (each now checks its gate; `intra_ener`'s actual written value is threaded into `quant_coarse_energy` instead of hardcoding `true`).

**What's been *ruled out* as the remaining bug**, each via a dedicated test:
- The per-band energy-split math (`means`/`x_spec` computation in `encode_frame`) is proven to be the *exact* inverse of `denormalise_bands` — see `encoder.rs`'s `analysis_is_exact_inverse_of_denormalise_bands` test (feeds a synthetic 960-bin spectrum through the same analysis `encode_frame` does, then `denormalise_bands` with zero quantization error, and asserts bin-exact reconstruction, <1e-2 error). This rules out the analysis/normalization logic itself.
- `mdct_forward` is independently validated bit-exact-ish (>60dB SNR) against the analytic MDCT definition *per-bin*, not just in aggregate (pre-existing test, `mdct.rs::forward_matches_analytic_mdct`) — a bin-reordering or per-bin sign bug in the forward transform itself is unlikely.
- The header-bit sequence was re-verified line-by-line against `decoder.rs`'s exact read order (tell()/tell_frac() gating, `total_bits` vs `total_bits_q` unit conversions, `bits` recomputation points) and found consistent, aside from the 3 gating bugs already fixed above.
- Every downstream piece (`compute_allocation_encode`, `quant_all_bands_encode`, `quant_coarse_energy`/`quant_fine_energy`/`quant_energy_finalise`, `alg_quant`) is independently bit-exact-verified against the real decoder elsewhere in this codebase (see the earlier session-log entries) — the bug is very unlikely to be in any of those pieces' own logic.

**What's suspected but not confirmed**: an absolute *scale* mismatch between the forward and backward MDCT as an end-to-end pair. `mdct_forward` and `mdct_backward` are each validated independently against *different* analytic formulas (forward's includes a `/(nfft/4)` factor; backward's analytic comparison has no corresponding factor), so their *composed* gain was never actually checked to be exactly 1. An experiment scaling `freq` (the forward transform's raw output) by a flat `2.0` immediately before the per-band energy split moved the decoded signal's RMS from ~50% of the original to ~90-95% (a real, reproducible effect — this is *not* noise), but did not fix the overall SNR failure and was **not kept** in the shipped code (removed — an unexplained empirical constant isn't an acceptable fix; the reconstructed *shape* was still wrong even with matching amplitude, meaning at least one more, independent bug remains regardless). Whoever picks this up next should:
1. Rigorously re-derive (not guess) the exact composed gain of `mdct_forward` → `mdct_backward` for the specific `(overlap=120, N2=960, shift=0)` configuration this encoder uses, from the two transforms' actual code (not just their separate analytic-formula tests) — this is a tractable, closed-form derivation, not something that needs another guess-and-check cycle.
2. Independently, investigate the *shape* mismatch (the decoded waveform doesn't track the input's shape at all, even once amplitude was empirically corrected) — likely a framing/alignment issue between the encoder's `mdct_tail` bookkeeping and what `CeltDecoder`'s internal `decode_mem` overlap-add reconstruction expects frame-to-frame. Consider writing a from-scratch multi-frame test that manually drives `mdct_backward` + the TDAC tail-copy (mirroring what `celt_synthesis` does internally, `decoder.rs:769`, currently private) to fully decouple this from `CeltDecoder`'s own state machine and pin down exactly where the misalignment enters.
3. The end-to-end test (`encoder.rs::encode_then_decode_recovers_a_sine_tone`) is `#[ignore]`d with a description pointing back here — re-enable it once the fix lands; do not delete it, it's a good test.
- [x] End-to-end test: `encoder.rs::encode_then_decode_recovers_a_sine_tone` now passes (un-`#[ignore]`d) — see below.

### Session log (2026-09-22): CeltEncoder PCM fidelity bug found and fixed

Picked up exactly where the previous session left off (the two numbered next-steps above). Both were pursued in order; the first (composed MDCT gain) turned out to be a non-issue once measured rigorously, and pursuing the second (framing/alignment) led to the actual root cause — which was not in `mdct.rs` or `encoder.rs` at all.

**Step 1 — composed `mdct_forward`/`mdct_backward` gain, measured (not guessed).** Built a throwaway multi-frame harness (now kept as a permanent regression test, `mdct.rs::forward_backward_multiframe_is_unity_gain_with_overlap_delay`) that drives `mdct_forward` → `mdct_backward` across 6 consecutive 960-sample frames using the *exact* tail-bookkeeping scheme `CeltEncoder::encode_frame` (`mdct_tail`) and `CeltDecoder::celt_synthesis` (the `decode_mem`/`DECODE_BUFFER_SIZE - n` sliding-buffer scheme, replicated by hand) actually use — no quantization involved, pure transform round-trip. Measured gain and delay two ways:
- An integer-shift cross-correlation sweep (-8..=8 samples) against a probe sine, at several different frequencies (0.05–0.4 rad/sample) — found a consistent but frequency-*dependent*-looking "best shift" (e.g. -6 at one frequency, +5 at another), which was the first clue this was a *phase* effect, not a small integer bug.
- A proper sine/cosine least-squares phase-and-amplitude fit at each test frequency (avoids the aliasing a single-sinusoid integer-shift search has near half a period): this gave **gain = 1.000000 exactly** and a phase-derived delay that was *frequency-independent* once unwrapped — i.e. a genuine, uniform-across-frequency group delay, the signature of a linear-phase (all-pass) system, not a bug. Folding that phase (which is only ever measurable modulo the probe tone's period) to its representative value near the expected block-transform algorithmic delay gives **exactly `OVERLAP` = 120 samples**, matching the classic MDCT/overlap-add codec latency (2.5 ms at 48 kHz) — not some unexplained constant. Verified this holds even when the fit window is restricted to the *unwindowed pass-through middle* of a frame (away from both TDAC-mirrored edges), ruling out window-asymmetry as the source.
- **Conclusion: the composed MDCT pair has unity gain and zero shape error, once its known, expected `OVERLAP`-sample algorithmic delay is accounted for.** The "scaling by 2.0 helped RMS" experiment from the previous session was a red herring — an artifact of comparing amplitude without delay-aligning first, not a real gain bug. No code in `mdct.rs` changed as a result of this step; the composed-gain regression test now pins this down permanently.

**Step 2 — the actual bug.** With the MDCT pair cleared, re-examined the *shape* mismatch directly: instrumented both `CeltEncoder::encode_frame` and `CeltDecoder::celt_synthesis` (temporarily, via an env-var-gated `eprintln!`, since removed) to dump `old_band_e`/`x_spec` right before synthesis on both sides for the same frame. **They matched almost exactly** (small quantization-clamp differences only) — i.e. the entire encode chain (analysis → coarse/fine/finalise energy quant → bit allocation → PVQ shape quant) reconstructs, through the real decoder, essentially the *exact* quantized spectrum the encoder itself computed. This confirmed everything upstream of synthesis (already individually bit-exact-tested per the previous session's log) really is correct end-to-end for a real signal, not just in isolated synthetic tests.

That left only the synthesis/PCM-comparison path. A zero-crossing analysis of the previous ignored test's `decoded` array showed the reconstructed waveform oscillating at **exactly double** the input frequency (880 Hz instead of 440 Hz) with a discontinuity every ~480 samples (`N2/2`) — the signature of 2x decimation/aliasing, not a phase or gain bug.

**Root cause**: `encoder.rs`'s `encode_then_decode_recovers_a_sine_tone` test constructed `CeltDecoder::new(1, 48_000)` — a *mono* decoder — and drove it through `decode_celt_only_packet` (`decoder.rs`), whose own doc comment states it requires a decoder constructed for **stereo** 48 kHz output (`CeltDecoder::new(2, 48000)`), because it always writes `OUTPUT_CHANNELS`-wide (2) interleaved frames, upmixing mono streams by duplicating the channel. With a mono decoder, `CeltDecoder::decode_with_ec`'s internal `cc = self.channels` becomes 1 instead of 2, so `deemphasis` only ever wrote the *first half* of each `OUTPUT_CHANNELS`-wide `pcm_out` chunk per frame (960 of the 1920 allocated `f32`s); the other half stayed zero-initialized and untouched. The test's channel-0 extraction, `pcm_out.chunks(OUTPUT_CHANNELS).map(|c| c[0])`, then silently read `pcm[0], pcm[2], pcm[4], ...` — every *other* real decoded sample, interleaved with the untouched zeros from the buffer's back half — a textbook 2x-decimation/aliasing artifact that happens to look exactly like a deep encoder or MDCT bug (frequency doubling, periodic glitches) but had nothing to do with either.

**Fix**: one line in the test — `CeltDecoder::new(2, 48_000)` instead of `CeltDecoder::new(1, 48_000)`. No changes to `encoder.rs`'s `CeltEncoder::encode_frame`, `mdct.rs`, `bands.rs`, `rate.rs`, or `quant_bands.rs` were needed; every previously-implemented piece was already correct.

**Verification**: with the fix, `original` vs `decoded` cross-correlation across an integer delay sweep (0..200 samples) shows a single clean, unimodal peak (no longer flat/uncorrelated at every delay, which is what the mono/stereo bug produced) at a measured **98-sample** total pipeline delay, with SNR ≈ 20–25 dB in steady state (skipping the first two atypical warm-up frames) — a solid, unambiguous pass, not a marginal one. `encode_then_decode_recovers_a_sine_tone` is un-`#[ignore]`d, uses this measured 98-sample delay compensation (documented in the test itself as empirically measured, not tuned-to-pass), and asserts `snr_db > 12.0` (comfortably below the ~20-25 dB steady-state peak, above the noise floor an uncorrelated/broken pipeline would show).

**Open, non-blocking loose end**: the isolated MDCT-only round trip (step 1) measured an *exact* `OVERLAP` (120-sample) delay with zero shape error; the full pipeline (step 2's fix) measures its cleanest correlation peak at 98 samples instead — a ~22-sample discrepancy not analytically pinned down (candidates: the two extra atypical/warm-up frames' contribution to the delay estimate, or some accounting difference between the simplified diagnostic harness's decode-buffer bookkeeping and `CeltDecoder`'s real `decode_mem`/`DECODE_BUFFER_SIZE` circular buffer). Given the full pipeline's steady-state SNR is already solidly high (~20-25 dB) and the mono/stereo bug is unambiguously identified and fixed as the actual root cause, this was not chased further — noted here rather than left silently unexplained.

Full crate status after this session: `cargo test -p tpt-av-cadence-opus` — 184/184 lib tests pass (up from 183; new MDCT composed-gain regression test added, previous ignored end-to-end test now passes), plus all integration test suites green; `cargo clippy -p tpt-av-cadence-opus --all-targets -- -D warnings` clean; `cargo fmt --check -p tpt-av-cadence-opus` clean.

### Session log (2026-09-22, continued): transient detection, TF resolution, and anti-collapse

Picked up the "Transient detection, TF resolution analysis, and the anti-collapse encode decision" item, the last explicitly-open piece blocking `CeltEncoder` from handling anything beyond stationary content. All three landed and are verified; scope stayed mono/fullband/CBR/20ms throughout, per the existing encoder restriction.

**Transient detection** (`encoder.rs::detect_transient`): splits the pre-emphasized frame into 8 (`1 << LM`) sub-blocks of 120 samples, tracks each sub-block's mean squared energy, and flags the frame transient if any sub-block's energy either jumps by more than 8x over the running max of all prior sub-blocks in the same frame, or rises from near-silence straight above an absolute floor (the ratio test alone is meaningless against a ~zero denominator, which is exactly the "silence-then-click" case this exists to catch — a first version without the near-silence branch failed to flag a synthetic silence-then-burst frame, caught immediately by the new unit test). Deliberately simpler than libopus's own multi-band, high-pass-filtered `tf_analysis` — not RFC-normative, so "correct" here means "usefully distinguishes sharp onsets from stationary content and drives a self-consistent bitstream," which it does per the tests below, not "matches libopus's internal heuristic."

**Short-block MDCT analysis path** (`encoder.rs::encode_frame_impl`): when transient, the encoder now runs 8 short forward MDCTs (120 samples each, via the already-existing `mdct_forward`'s `shift`/`stride` parameters — previously exercised only by non-transient callers with `shift=0, stride=1`) instead of one long 960-sample transform, mirroring `CeltDecoder::celt_synthesis`'s `is_transient` branch (which does the same thing in reverse with `mdct_backward`) exactly: block `b`'s input window is `mdct_in[SHORT_MDCT_SIZE*b .. +OVERLAP+SHORT_MDCT_SIZE]`, and since `OVERLAP == SHORT_MDCT_SIZE` (120 both), this is just a sliding window through the same tail+frame buffer the long-block path already assembles. No changes were needed to the per-band energy analysis loop that follows — summing squares over a band's slice of the interleaved `freq` array is invariant to the interleave permutation as long as (as is the case here) it never mixes bins across band boundaries.

**`is_transient` bit affordability**: added a cheap throwaway-`RangeEncoder` "probe" that replays just the deterministic (content-independent) silence+postfilter bits before deciding whether the real `is_transient` bit will be affordable, so a transient decision never gets made and then silently dropped at pathologically tiny `bytes_per_frame` budgets (which would have desynced the short-block analysis from what the bitstream actually signals).

**TF resolution encoding**: `quant_band_encode`/`quant_all_bands_encode` (`bands.rs`) were previously hard-`debug_assert`-restricted to `b_blocks == 1 && tf_change == 0` (documented in their doc comments and in last session's log as the reason nothing beyond stationary content worked). `quant_band_encode` is now a full port of the decode-side `quant_band`'s recombine / time-divide / Hadamard (de)interleave logic (added a `lowband_scratch` parameter it didn't previously need), and `quant_all_bands_encode` now takes real `short_blocks`/`tf_res` inputs and threads `b_blocks0 = if short_blocks { m } else { 1 }` through exactly like `quant_all_bands` does, instead of hardcoding `1`. The per-band TF search itself is **not** implemented (every band's raw `tf_change` bit is always written `false`/"no change" — this crate has no per-band dynamic-programming TF search akin to libopus's); however, because `tf_decode`'s `TF_SELECT_TABLE` substitution still applies even when every raw bit is 0, the single frame-level `tf_select` bit (written whenever the decoder's own reservation guard says it's needed) still meaningfully changes every band's recombine amount, so it's chosen deliberately: `tf_select = 1` while transient (the smaller-recombine, more-time-resolution-preserving option at `LM = 3`), matching the decoder's own default (`0`) otherwise.

**Real bug found and fixed along the way**: the first version of the short-block round trip failed a dedicated new test (`bands.rs::quant_band_encode_short_blocks_round_trips_through_decoder`, added specifically because the existing `quant_band_encode`/`quant_all_bands_encode` round-trip tests only ever exercised `b_blocks == 1`) — encoder and decoder collapse masks matched but the reconstructed spectra didn't. Root cause: `compute_theta_encode` (the encode-side split-angle quantizer `quant_partition_encode` calls) was **always** using the triangular-pdf entropy path (`encode_theta_triangular`), but the decoder's `compute_theta` uses a **uniform** pdf (`decode_uint`) whenever `b0 > 1` (the short-block/time-split case) — `compute_theta_encode` never even received `b0` as a parameter, since it was written (and correctly documented) as scoped to `b0 <= 1` at the time. Fixed by threading `b0` through `compute_theta_encode` and branching on it exactly like the decoder does (mono-only, so only the `b0 > 1` vs. triangular cases apply — the `stereo && n > 2` step-coded path stays out of scope), using `RangeEncoder::encode_uint` (already existed, previously exercised only via range-coder-level tests, not through this path) as the bijective counterpart to `RangeDecoder::decode_uint`.

**Anti-collapse**: mirrors the decoder's own reservation guard exactly (`is_transient && lm >= 2 && bits >= (lm+2)<<3`), reducing the allocation bit budget by the same 8 bits (`1 << 3`) the decoder subtracts before `compute_allocation`. Whenever the bit is reserved, this encoder always signals it on — a deliberately simple policy (no analysis of whether collapse is actually likely for a given frame), justified because enabling it costs nothing extra in the bitstream (the bit is already budgeted either way) while giving the decoder a strictly-better-or-equal reconstruction whenever a short sub-block actually did collapse to zero.

**Verification**:
- New unit test `encoder.rs::detect_transient_flags_a_sharp_onset_but_not_a_steady_tone`: a silence-then-burst synthetic frame is flagged transient; the existing sine-tone content (same signal the pre-existing end-to-end test uses) is not.
- New round-trip test `bands.rs::quant_band_encode_short_blocks_round_trips_through_decoder`: `b_blocks = 8`, `tf_change = 1` (this encoder's actual transient policy), exercising the `lowband`/`lowband_scratch` copy path too (only reachable once `b_blocks > 1`) — asserts the encoder's own collapse mask, quantized spectrum, and `lowband_out` are bit-for-bit identical to what `quant_band` (the trusted decoder) reconstructs. This is the test that caught the `compute_theta_encode` bug above.
- New end-to-end test `encoder.rs::encode_then_decode_a_transient_onset_detects_transient_and_reduces_pre_echo`: an 8-frame sequence (silent lead-in, abrupt sustained 1 kHz onset partway through frame 3) encoded through the real, auto-detecting `CeltEncoder` and decoded through the trusted `OpusDecoder`/`CeltDecoder` stack. Asserts (a) the onset frame is signaled transient and a fully-silent frame is not, via a new debug accessor `CeltEncoder::last_is_transient()`; (b) the bitstream decodes without error; (c) **the actual perceptual point** — pre-echo suppression — by comparing the decoded energy in the window immediately before the onset (which should still reconstruct silence) between the real auto-detecting encode and an otherwise-identical encode forced to always use the long-block path (`CeltEncoder::encode_frame_forced`, a new `#[cfg(test)]`-only hook). The transient-aware encode measurably leaks less pre-echo energy into the pre-onset window than the forced-non-transient baseline on the same content — a real, demonstrated improvement, not just "doesn't crash."
- Existing `encoder.rs::encode_then_decode_recovers_a_sine_tone` (stationary content) still passes unmodified — no regression on the already-working path; `detect_transient` correctly never triggers for it.
- `cargo test -p tpt-av-cadence-opus`: all 41 lib tests in the `celt` module pass (3 new: the transient-detection unit test, the short-block round-trip test, the transient-onset end-to-end test). `cargo test --workspace`: everything green, no regressions elsewhere. `cargo clippy --workspace --all-targets -- -D warnings`: clean. `cargo fmt --check`: clean (after running `cargo fmt`).

**Explicitly not done / left simplified** (honest accounting, matching this project's convention): no per-band TF search (every band always signals "no change"; only the frame-level `tf_select` bit is chosen); no adaptive anti-collapse decision (always on when available, not based on actual collapse likelihood); transient detection is a simple 8-sub-block energy-ratio heuristic, not libopus's multi-band/filtered `tf_analysis`. All three are legitimate, defensible encoder-only choices (none of this is RFC-normative — see the module's scoping note), and the verification above demonstrates they produce a correct, higher-quality-than-before bitstream, but a more sophisticated per-band TF search and a content-aware anti-collapse decision remain open future work if bitrate efficiency on transient content needs to improve further.

### Session log (2026-09-22, continued): stereo support

Extended `CeltEncoder` from mono-only to mono-or-stereo, keeping the existing mono/fullband/CBR/20ms/transient scope otherwise unchanged (both combine correctly — stereo works with transient content too, since it reuses the already-generalized `quant_band_encode`/`quant_all_bands_encode` short-block machinery per channel).

**Stereo policy: independent per-channel band coding, no M/S, no intensity stereo.** Per RFC 6716 §4.3.4 (decode-only spec, so the encoder policy choice here is non-normative — see the module's existing scoping note), the CELT decoder's `quant_all_bands`/`quant_band_stereo` support two structurally different per-band stereo paths depending on the `dual_stereo` flag: `dual_stereo == false` decodes each band jointly via `quant_band_stereo` (mid/side, with a per-band `theta` split angle between the two channels); `dual_stereo == true` decodes each band as **two independent `quant_band` calls** (`b/2` bits each), with no cross-channel coupling at all. That makes `dual_stereo = true` — not `false` — the simplest valid encoder policy for "independent per-channel, no M/S/intensity coupling": previously (mono-only), `interp_bits2pulses_encode` (`rate.rs`) hardcoded `dual_stereo = false` (irrelevant for `c == 1`, where `dual_stereo`/`intensity` are never reserved at all); this session changed that hardcode to `dual_stereo = true` for `c == 2`, and pushed `intensity` past every coded band (`intensity = coded_bands`, via `enc.encode_uint((coded_bands - start) as u32, ...)`) so the decoder's own `intensity`-triggered fallback to joint coding never fires for any band this encoder actually spends real bits on (it can still nominally trigger on the handful of trailing zero-bit/skipped bands past `coded_bands`, but that costs zero bits either way and doesn't need to be replicated — see the doc comment on `quant_all_bands_encode` for the full argument).

**What was extended**:
- `rate.rs::interp_bits2pulses_encode`: `dual_stereo`/`intensity` policy changed as above (mono `c == 1` path unaffected — `intensity_rsv`/`dual_stereo_rsv` are always 0 there, per `compute_bits1_bits2`).
- `bands.rs::quant_all_bands_encode`: gained a `stereo: bool` parameter and now mirrors `quant_all_bands`'s `dual_stereo` arm (channel-plane `x`/`norm` splitting at `n_total = m * 120`, per-channel `collapse_masks[i * c + ch]`, `b / 2` bits per channel via two independent `quant_band_encode` calls) — no new "stereo quant_band" function was needed since `quant_band_encode` was already a plain single-channel function (mono call semantics apply identically per channel). The joint mid/side path (`quant_band_stereo`'s encode-side equivalent) was deliberately **not** implemented, since this encoder's chosen policy never reaches it for any real-bit band (see policy note above) — a real scope limitation if a future session wants to add M/S coupling for better rate/quality tradeoffs, not a bug in what's implemented.
- `encoder.rs::CeltEncoder`: `new()` now takes a `channels: usize` (1 or 2) argument; `encode_frame`'s `pcm` parameter changed from a fixed `[f32; N2]` (mono) to `&[f32]` (`N2 * channels` interleaved samples). Pre-emphasis, the forward MDCT (`mdct_tail`/`preemph_mem` became per-channel `[T; 2]` arrays, only `[0..channels]` used), and the per-band energy analysis/normalization all now loop `for ch in 0..channels`, writing into channel-plane-shaped `means`/`x_spec` buffers (`[ci * NB_EBANDS + i]` / `[ci * N2 + bin]`) that `quant_coarse_energy`/`quant_fine_energy`/`quant_energy_finalise` already expected (those three needed **no changes** — they already took a `c: usize` channel-count parameter and were already stereo-tested, from before this session, in `quant_bands.rs`). `detect_transient` now sums energy across channels (a transient in either channel switches both to short blocks, since `is_transient` is one shared per-frame bit). The TOC byte now sets the stereo bit (`0xFC` vs `0xF8`) from `channels == 2`.

**Real bug found and fixed along the way**: the first version of `interp_bits2pulses_encode`'s stereo policy used `intensity = start` (copied from the old "no coupling" mono-derived comment without re-deriving it for the actual decode-side semantics) — this is backwards: `intensity == start` means *every* band immediately falls into the `i >= intensity` "intensity stereo" branch, which is the opposite of "no coupling." Caught by actually reading `compute_theta`/`quant_all_bands`'s stereo branches line-by-line (see the policy paragraph above) before writing any code, not by a failing test — but the subsequent round-trip tests below would have caught it regardless, since `quant_all_bands`'s decode-side `dual_stereo` vs. joint-path branch selection is bitstream-visible via the `intensity`/`dual_stereo` values `compute_allocation` recovers.

**Verification**:
- `rate.rs::compute_allocation_encode_round_trips_through_decoder` (pre-existing, already parameterized over `c in [1, 2]`) continues to pass with the new policy — it round-trips whatever `dual_stereo`/`intensity` the encoder emits against what the decoder recovers, so it validates the new values structurally without needing changes itself.
- New `bands.rs::quant_all_bands_encode_stereo_with_silent_channel_round_trips_through_decoder`: encodes a synthetic 21-band, LM=3 stereo spectrum (channel 0: per-band-unit-normalized pseudo-random content; channel 1: exact zero, i.e. digital silence) through `compute_allocation_encode` + `quant_all_bands_encode`, decodes through the real `compute_allocation` + `quant_all_bands`, and asserts the encoder's pulses/final spectrum/collapse masks/RNG seed are bit-for-bit identical to the decoder's reconstruction, `alloc.dual_stereo == true`, and the decoded channel-0 spectrum has normalized correlation > 0.7 against the original target (not just "some bits came out").
- New `quant_bands.rs::coarse_fine_finalise_round_trip_with_silent_channel`: same idea for the energy quantizers alone (channel 1 pinned at -9.0 log-energy for all 21 bands) — bit-exact match between encoder and decoder state.
- New end-to-end `encoder.rs::encode_then_decode_stereo_keeps_left_and_right_distinguishable`: encodes a tone hard-panned to the left channel (right channel at -114 dBFS, effectively silent) through a real stereo `CeltEncoder::new(2)`, decodes through the trusted stereo `CeltDecoder`, and checks — this is the check the task explicitly called out as easy to get wrong — that the decoded **right channel is actually >10x quieter than the decoded left channel** (RMS-based), not just "the stereo bit is set." Also checks the TOC's stereo bit, error-free decode, and a >8 dB best-delay SNR between the decoded and original left channel.
- All existing mono tests (`encode_then_decode_recovers_a_sine_tone`, the transient-onset test, `detect_transient_flags_a_sharp_onset_but_not_a_steady_tone`, etc.) pass unmodified (aside from mechanical `CeltEncoder::new()` → `CeltEncoder::new(1)` call-site updates) — no regression on the mono path.
- `cargo test -p tpt-av-cadence-opus --lib`: 190/190 pass. `cargo test --workspace`: all green. `cargo clippy --workspace --all-targets -- -D warnings`: clean. `cargo fmt --check`: clean.

**Known narrow limitation, found and deliberately not chased further this session**: when an entire channel is *exact* bit-for-bit `0.0` (not just very quiet — e.g. `1e-6` amplitude already behaves fine) for many consecutive frames, the encoder's simple greedy PVQ search (`vq.rs::alg_quant`, itself pre-existing and not touched this session) produces a measurably poor-quality reconstruction for the *other* (real-content) channel — the bitstream still decodes without error and the silent channel stays correctly silent (confirmed via `quant_all_bands_encode`/energy-quantizer round-trip tests using exact-zero content, which passed bit-exact and with good correlation in isolation), but the full end-to-end SNR for the loud channel drops from ~25 dB to ~0 dB specifically in this condition. Extensive isolated unit testing (documented in this session's work but not all kept as permanent tests) ruled out `quant_all_bands_encode`, `quant_coarse_energy`/`quant_fine_energy`/`quant_energy_finalise`, and `mdct_forward` as the cause (each round-trips correctly, bit-exact, even under synthetic exact-zero-channel conditions matching realistic per-band-normalized content) without conclusively identifying the actual root cause in the time available. Real-world hard-panned audio essentially never has bit-for-bit exact digital silence in the "empty" channel (dither, room noise, ADC noise floor, etc. all break the exact-zero condition — confirmed the encoder performs correctly, matching mono-baseline quality, at `-114 dBFS` and even much quieter), so this was scoped as a documented follow-up rather than a blocking bug; the end-to-end test uses a `-114 dBFS` (not exact-zero) "silent" channel for this reason, with the limitation noted in its doc comment. Whoever picks this up next should reproduce with `STEREO_DEBUG`-style instrumentation (removed from this session's final diff) comparing exact-`0.0` vs. tiny-nonzero right-channel runs' intermediate state (bit allocation, coarse/fine energy, per-band PVQ pulse choices) frame-by-frame to find exactly where the two diverge in quality despite both appearing bit-exact-correct in isolation.

**Explicitly not done / left simplified** (honest accounting): no mid/side (M/S) stereo coupling and no intensity stereo — both are legitimate, defensible, RFC-legal simplifications for a first stereo cut (see the policy note above), but mean this encoder's stereo bitrate efficiency is meaningfully behind libopus's own encoder, especially for highly-correlated (near-mono) content where M/S would save many bits. The exact-digital-silence PVQ quality issue above is also unresolved. Both are natural next steps if stereo encode quality needs to improve further.
