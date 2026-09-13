# tpt-cadence — Project TODO

Tracks all tasks for the whole project, organized by phase. See `spec.txt` (to become `DESIGN.md`) for full design rationale.

## Phase 0 — Project Setup & Governance

- [ ] Initialize git repository
- [ ] Create workspace `Cargo.toml` (resolver "2", members list, `[workspace.package]` with `license = "MIT OR Apache-2.0"`, edition 2021, rust-version 1.75)
- [ ] Add `LICENSE-MIT` and `LICENSE-APACHE` (dual license)
- [ ] Write `README.md` (vision, ecosystem table, quickstart)
- [ ] Write `DESIGN.md` (carry over spec.txt content as the living design doc)
- [ ] Create `deny.toml` (cargo-deny license allow/deny lists per spec §8)
- [ ] Set up CI pipeline (build/test matrix + `cargo-deny` license audit job)
- [ ] Write `CONTRIBUTING.md` (dual-license contribution terms, conformance-test requirement for new decoders)

## Phase 1 — Foundation

- [ ] Scaffold `tpt-av-cadence-core`: `Decoder` trait, `StreamInfo`, `Format`/`SampleFormat`/`ChannelLayout` enums, `CadenceError` enum
- [ ] Scaffold `tpt-av-cadence-pcm`: raw headerless PCM reader
- [ ] Scaffold `tpt-av-cadence-wav`: RIFF chunk parser (`reader.rs`), PCM/IEEE Float decoder (`decoder.rs`) — 8/16/24/32-bit int + 32/64-bit float, mono/stereo
- [ ] Scaffold `tpt-av-cadence-test-utils`: FFmpeg subprocess comparison harness (`reference.rs`), fuzz helpers (`fuzz.rs`)
- [ ] Bit-exact conformance tests for WAV (all bit depths, mono/stereo, float, extensible fmt chunk, odd chunk sizes)
- [ ] Wire up `cargo-deny` CI enforcement for pure MIT/Apache dependency tree

## Phase 2 — Lossless & Migration

- [ ] Implement `tpt-av-cadence-aiff` (big-endian IFF chunk parser, AIFF/AIFC decode)
- [ ] Implement `tpt-av-cadence-flac`: stream/frame parser, subframe types (Constant/Verbatim/Fixed/LPC), Rice coding, LPC prediction, top-level `Decoder` impl
- [ ] Migrate AAC-LC decoder from `tpt-kinetix` into `tpt-av-cadence-aac` (ADTS parser, AudioSpecificConfig, Huffman tables, IMDCT, TNS, PNS, M/S + intensity stereo), repackaged into the new `Decoder` trait
- [ ] Conformance tests against official FLAC test suite
- [ ] Conformance tests against ITU-T AAC reference vectors

## Phase 3 — Modern Compressed

- [ ] Implement Opus packet parser (RFC 6716)
- [ ] Implement SILK decoder (speech-optimized, LP-based)
- [ ] Implement CELT decoder (MDCT-based, music-optimized)
- [ ] Integrate hybrid SILK+CELT mode
- [ ] Conformance tests against official Opus test vectors

## Phase 4 — Legacy & Open Source

- [ ] Implement `tpt-av-cadence-mp3` (Huffman decoding, polyphase filterbank, joint stereo)
- [ ] Implement `tpt-av-cadence-vorbis` (MDCT-based OGG Vorbis decoder)
- [ ] Conformance tests against mpg123 conformance streams (MP3)
- [ ] Conformance tests for Vorbis

## Cross-Cutting (ongoing, applies to every phase)

- [ ] Enforce real-time safety contract per decoder (alloc-free/lock-free/panic-free `decode()`, allocation confined to `init()`/`open()`)
- [ ] Fuzz testing (`cargo-fuzz` + `proptest`) for every new parser — never panics on malformed input
- [ ] Bit-exact validation via `assert_bit_exact_vs_ffmpeg` harness for every new decoder before marking it stable
