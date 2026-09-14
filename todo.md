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

### AAC-LC from scratch — session handoff status (next steps)

Working state: 142 workspace tests pass; the AAC crate compiles, all 8 module
unit tests pass, and ADTS framing + element parsing + spectral decode run
end-to-end against a real FFmpeg-encoded ADTS file (`tests/data/test.aac` +
FFmpeg reference decode `test_ref.f32`).

Remaining (next session, in order):
1. **Time-domain aliasing bug**: decoded output shows the classic MDCT
   sign-alternation between consecutive frames — the IMDCT/windowing/lap
   interaction is wrong. `imdct_and_window` ports FFmpeg's
   `imdct_and_windowing`; my `Mdct` produces natural-order synthesis with
   ISO scale 2/N. Candidates: (a) FFmpeg's av_tx output arrangement is
   half-swapped relative to natural order — try swapping/reversing buf
   halves in `imdct_and_window` (a naive full-reversal was tried and did
   NOT fix it — revert it); (b) my synthesis kernel shift (n/4) may need
   to be (n − n/4) i.e. negative-frequency variant.
2. **Mid-stream desync at ~frame 22** ("section extends past max_sfb") —
   likely fixed by (1) being a red herring; re-check after lap fix.
   Also relax/verify: ics_info reserved bit is already tolerated.
3. **Global scale check**: my coefficient convention is ISO-natural
   (sf = +2^((gg+δ−100)/4)); FFmpeg uses negated sf and its av_tx imdct
   scale is 1/1024 (vs ISO 2/N). After (1), compare signed output vs
   `test_ref.f32`; a constant power-of-two factor or global sign is a
   single-constant fix.
4. Then: conformance vs ISO vectors (see CONTRIBUTING; ffmpeg binary via
   `pip install imageio-ffmpeg` works locally), proptest, todo.md.

Key references fetched to `/tmp/aacref` (re-fetch if gone): FFmpeg
aactab.c/aacdec.c/aacdec_dsp_template.c/aacdec_proc_template.c/kbdwin.c/
sinewin_tablegen.h/av_tx (tx_template.c). Constants confirmed: ZERO_BT=0,
NOISE_BT=13, INTENSITY_BT2=14, INTENSITY_BT=15, ESC book 11; SCALE_DIFF_ZERO
=60, NOISE_OFFSET=90, NOISE_PRE=256/9 bits, POW_SF2_ZERO=200; KBD α=4.0
(long 1024), α=6.0 (short 128); noise LCG seed 0x1f2e3d4c, ×1664525
+1013904223; TNS order limits 12 long / 7 short; TNS coef_len =
coef_res + 3 − coef_compress, tmp2_idx = 2·compress + res.

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
