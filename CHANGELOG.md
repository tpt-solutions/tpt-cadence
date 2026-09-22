# Changelog

All notable changes to `tpt-cadence` are documented here, grouped by dated
development milestone. The project has not yet published a crates.io release
(see `todo.md`), so entries are organized by date rather than semantic
version. Format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- Opus CELT encoder foundation: forward MDCT wired into a working
  `CeltEncoder` (mono, fullband, non-transient, CBR, 20 ms frames only) with
  a passing encode-then-decode round trip. This is the first landed piece of
  the planned Opus encoder (the user-confirmed first encoder target); hybrid
  SILK+CELT encoding, VBR, transient handling, and other frame sizes/channel
  configs are not yet implemented.
- WAV, AIFF, and headerless PCM writers (`WavEncoder`, `AiffEncoder`,
  `PcmEncoder`), matching each format's existing decoder in supported bit
  depths (8/16/24/32-bit PCM, 32/64-bit float) and channel counts, behind a
  new shared `Encoder` trait in `tpt-av-cadence-core`. Round-trip tested
  bit-exact against each crate's own decoder.
- FLAC encoder (`FlacEncoder`): fixed-blocksize, CONSTANT/VERBATIM/FIXED-
  predictor subframes with partitioned Rice coding, 1-8 channels, 4-32 bit
  depth. Verified bit-exact both against this crate's own decoder and a live
  FFmpeg decode of its output. LPC subframes and stereo decorrelation are
  not yet implemented (see `todo.md`).
- MP3 encoder (`Mp3Encoder`): MPEG-1 Layer III, fixed CBR bitrate, long
  blocks only, bit-reservoir-free framing. Bitstream validity is verified
  independently via a live FFmpeg decode of its output; audio fidelity is
  currently poor pending a documented analysis-filter fix (see `todo.md`).

### Known limitations (tracked in `todo.md`)
- AAC SBR (HE-AAC) has a ~22 dB SNR residual vs. the FFmpeg reference on an
  otherwise-correct decode path; root cause not fully characterized.
- PS / HE-AACv2 is parsed but not applied (decodes as mono).
- Multichannel HE-AAC (5.1/7.1) applies only one SBR context instead of one
  per channel element.
- Four AAC FATE multichannel conformance items (CCE/PCE coupling) decode at
  reduced fidelity (2-53 dB) pending a coupling/PCE-interaction root cause.
- Broader official ISO/IEC AAC and MP3 conformance vector suites are not
  obtainable/integrated; AAC and MP3 correctness currently rest on FFmpeg-
  reference comparison rather than official test vectors.
- MP3 encoder produces valid, independently-FFmpeg-decodable bitstreams but
  poor audio fidelity: its analysis filterbank is a generic substitute, not
  matched to the decoder's real fixed synthesis prototype (see `todo.md`'s
  "MP3 encoder" session log for what was tried and the likely fix). A new
  diagnostic test (`analysis_filter_alone_round_trips_through_synth`)
  isolates `analyze_block_polyphase` from the MDCT/quantization/Huffman
  stages entirely and confirms the mismatch is localized there (weak
  correlation, ~0.24, feeding straight into the decoder's synthesis
  filterbank) rather than anywhere downstream.
- Encoders beyond the Opus CELT foundation, the WAV/AIFF/PCM writers, the
  FLAC encoder, and the MP3 encoder above (Vorbis, AAC) are not yet started;
  AAC encoding is on hold pending a patent-licensing decision.

## 2026-09-21 — CLI, CELT encoder groundwork, panic-safety audit

### Added
- `tpt-av-cadence-cli` (`cadence` binary): auto-detects format by extension
  (with content-sniffing to distinguish Ogg Vorbis from Ogg Opus), and
  provides `info` and `decode` subcommands, including transcode-to-WAV via a
  minimal hand-rolled 16-bit PCM WAV writer.
- `examples/decode.rs` added to every decoder crate (WAV, AIFF, FLAC, MP3,
  PCM) for quick per-crate usage reference.
- Initial `CeltEncoder` scaffolding (mono/fullband/non-transient/CBR/20 ms)
  and a promoted-to-production forward MDCT.

### Fixed
- Workspace-wide panic-safety audit (~230+ sites traced): fixed a
  hybrid-mode subtraction underflow in the Opus decoder on a crafted small
  packet, a missing spec-mandated clamp in the Vorbis Floor1 decode that
  could overflow on crafted amplitude deltas, and an unbounded CCE
  gain-array index in the AAC decoder reachable from a malformed stream.
- AAC/SBR decode could overflow a 1 MiB default thread stack (e.g. a plain
  Windows `main()`) because the `Sbr` struct's large fixed-size tables were
  stored inline in `AacDecoder` rather than boxed; boxed the offending
  fields, cutting the measured release-build minimum stack from ~449 KB to
  ~65 KB.
- FLAC decoder removed unconditional per-frame debug `eprintln!` calls from
  the hot decode path (violated the allocation/lock-free real-time-safety
  contract; discovered via the new CLI).
- CELT `quant_band` collapse-mask bug using a stale pre-time-divide bit
  width, closing the residual 07/08/09 PCM SNR gap left after the hybrid
  fold refactor.

## 2026-09-20 — Opus hybrid fold refactor, shared Ogg crate, real-time safety

### Added
- Shared `tpt-av-cadence-ogg` crate: the Ogg page-parsing layer moved out of
  the Vorbis crate so Opus and Vorbis (and future formats) share one
  implementation.
- `cargo-fuzz` targets and per-crate fuzz/property-test suites across the
  workspace.

### Changed
- Completed the Opus "hybrid fold" refactor: band recursion now uses
  open-ended slices into the shared per-channel norm buffer (mirroring
  libopus's raw-pointer semantics) instead of exact-width slices that
  panicked at band/split boundaries on some vectors.
- Restored allocation-free Opus decode: removed three per-frame `Vec`
  allocations from `compute_allocation`, cached debug-flag environment
  lookups once at construction instead of per band/call, and replaced a
  mode-transition crossfade's heap `to_vec()` with a stack array.

### Result
- All 12 official RFC 6716 Opus test vectors decode with 100% `final_range`
  match on every packet (vectors 02-04 bit-exact PCM; others 37-110 dB SNR
  vs. a live libopus 1.5.2 oracle).

## 2026-09-18/19 — MP3, Opus/CELT/SILK, and Vorbis reach working/conformant status

### Added
- **MP3 decoder** (`tpt-av-cadence-mp3`): Huffman decoding, polyphase
  filterbank, joint stereo, bit-reservoir handling, CRC-protected frame
  support. Ten-stream FFmpeg PCM conformance at 118.7-119.4 dB SNR
  (<=1e-5 peak error); allocation-free `decode()` verified by a
  counting-allocator test. Official ISO/IEC 11172-4 conformance vectors are
  not freely obtainable, so conformance rests on the FFmpeg-oracle
  comparison plus structural/replay/robustness/CRC test suites.
- **Vorbis decoder** (`tpt-av-cadence-vorbis`): MDCT-based Ogg Vorbis
  decode, conformance-tested against FFmpeg at 136-138 dB SNR across six
  bundled fixtures (mono/stereo, 32/44.1/48 kHz, multiple quality levels),
  sample-exact lengths, bit-identical `seek(0)` replay, and exact
  mid-stream seek rejoin.
- **Opus decoder** (`tpt-av-cadence-opus`): range coder (RFC 6716 §4.1/§5.1),
  CELT decoder, SILK decoder, and hybrid SILK+CELT integration, wired into a
  full top-level `OpusDecoder` state machine (mode transitions, redundancy
  frames, DTX/PLC). RFC 7845 Ogg Opus container support
  (`OpusHead`/`OpusTags`, pre-skip/end-trim handling, seek).
- Per-crate README/CHANGELOG documentation added across the workspace.

### Fixed
- Root-caused and fixed the CELT `final_range` entropy-desync bug: raw-bit
  reads in the range coder never advanced the total-bits counter used by
  the CELT bit allocator, silently under-budgeting every allocation
  decision downstream of a postfilter-bearing frame. Found via a byte-exact
  trace against a locally built libopus 1.5.2 oracle; all 6 CELT-containing
  test vectors went from 0-30% to 100% `final_range` match.
- Fixed a SILK multi-subframe (40/60 ms payload) decode bug; 3 of 12 RFC
  6716 vectors reached bit-exact `final_range` and PCM match.
- Fixed raw-bit accounting in the range coder's `tell()`.
- Fixed AAC windowing/overlap-add regressions surfaced while building out
  the MP3/Opus decoders in parallel.

## 2026-09-14/17 — Project bootstrap; WAV/AIFF/PCM/FLAC and AAC-LC/SBR land

### Added
- Workspace scaffolding: `tpt-av-cadence-core` (`Decoder` trait,
  `StreamInfo`, error types), `tpt-av-cadence-test-utils` (FFmpeg oracle
  harness, fuzz helpers, MD5), CI (`cargo test` matrix + `cargo-deny`
  license audit), dual MIT/Apache-2.0 licensing.
- **WAV decoder**: RIFF chunk parsing, PCM/IEEE-float, 8/16/24/32-bit,
  extensible fmt chunk, bit-exact conformance tests.
- **AIFF decoder**: big-endian IFF chunk parsing, AIFF/AIFC (NONE/twos/sowt/
  FL32/FL64/in24/ni24), 80-bit extended sample rates, bit-exact vs. FFmpeg.
- **Raw PCM decoder**: headerless int8/16/24/32 and f32/f64, both byte
  orders.
- **FLAC decoder**: bit-exact conformance vs. FFmpeg across bundled
  subset/uncommon fixtures.
- **AAC-LC decoder** (`tpt-av-cadence-aac`), built from the ISO/IEC 14496-3
  spec: full profile support including PNS, M/S stereo, TNS, PCE-based
  channel configurations (mono through 7.1), CCE coupling channels, and
  ADTS/raw framing. FFmpeg round-trip conformance >120 dB SNR across
  sample rates and channel layouts; 4 official ISO/IEC FATE-mirrored
  conformance items pass at full fidelity (121-129 dB).
- **SBR (HE-AAC) decoding**: full bitstream parsing, frequency-table
  derivation, dequantization, envelope/gain calculation, HF generation, and
  a 64-band QMF analysis/synthesis pair, verified stage-by-stage against an
  independent reference build. See Known Limitations for its residual
  fidelity gap.

### Fixed
- Three independent AAC-LC bugs found via NumPy-assisted coefficient
  bisection and FFmpeg source audit: codebooks 5/6 decoded through the
  wrong (unsigned) branch, PNS noise phase-inverted, and non-common-window
  CPEs double-parsed (corrupting subsequent elements).
- A data-stream-element count field misread as 4 bits instead of 8,
  desynchronizing any ADTS stream containing a non-trivial DSE.
- Fixed configuration 7 to carry 8 channels (7.1) instead of 7.

[Unreleased]: https://github.com/tpt-solutions/tpt-cadence
