# Changelog

All notable changes to `tpt-cadence` are documented here, grouped by dated
development milestone. The project has not yet published a crates.io release
(see `todo.md`), so entries are organized by date rather than semantic
version. Format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- HE-AACv2 Parametric Stereo synthesis (QMF-domain PS stage ported from the
  reference decoder implementation, FFmpeg `aacps.c`/`aacpsdsp_template.c`):
  hybrid analysis/synthesis filterbank, transient-aware all-pass
  decorrelation, IID/ICC mixing with IPD/OPD phase smoothing, and the
  reference's end-of-frame envelope fix-ups. Streams flagged HE-AACv2
  (ASC AOT 29) now open as stereo with the doubled SBR rate instead of
  being rejected, and mono ADTS/raw-AOT-2 streams flip to stereo output on
  their first in-band SBR payload (the reference decoder's "treating HE-AAC
  mono as stereo" behavior) — until a PS header arrives the mono channel
  is duplicated, exactly like the reference. Component-level verification
  compares the Rust port against a standalone build of FFmpeg's
  `ff_ps_apply` (generated tables value-for-value, per-stage pipeline
  state, and final L/R output on four parameter scenarios; fixtures in
  `tpt-av-cadence-aac/tests/data/ps_oracle/`), and a new end-to-end
  fixture (`ps_tone.aac` encoded with a from-source libfdk-aac build)
  decodes at 98.4 dB whole-stream SNR (82-139 dB per frame) against
  FFmpeg's PS-capable decode.
- Opus CELT encoder hardening: `CeltEncoder::try_encode_frame` for
  callers that want explicit PCM validation (length, finiteness, [-1, 1]
  range), `OggOpusEncoder::encode` now rejects non-finite/out-of-range
  samples with a precise error, and encoding after `finish()` is rejected.
  `finish()` is idempotent.
- Non-publishing release preparation: `tools/release_prep.py` validates workspace
  metadata and can generate a local version/changelog patch; the manual
  `release-prep` workflow uploads that patch as an artifact and never commits,
  tags, pushes, or publishes.
- Opus CELT encoder foundation: forward MDCT wired into a working
  `CeltEncoder` (mono or stereo, fullband, CBR, all four CELT frame sizes) with a
  passing encode-then-decode round trip, now including real transient
  detection, short-block MDCT/TF-resolution encoding, an anti-collapse
  decision (verified to measurably reduce pre-echo on transient content vs.
  the previous non-transient-only path), and stereo support (independent
  per-channel band coding via `dual_stereo`, no mid/side or intensity-stereo
  coupling yet — verified end-to-end, including that hard-panned left/right
  content actually decodes distinguishably rather than collapsing to mono).
  This is the first landed piece of the planned Opus encoder (the
  user-confirmed first encoder target); hybrid SILK+CELT encoding, VBR, and
  other frame sizes are not yet implemented.
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
  independently via a live FFmpeg decode of its output; active reduced-scope
  mono and independent-stereo end-to-end fidelity gates pass. Psychoacoustic
  tuning, reservoir borrowing, short blocks, and stereo coupling remain out of
  scope.

### Fixed
- The SBR extension parser matched the wrong extension id for Parametric
  Stereo (1 instead of the normative 2), so in-band PS payloads were never
  decoded at all; every previous PS test wrote the same wrong id, so the
  bug was invisible until a real HE-AACv2 stream was decoded.
- The PS parameter Huffman decoder assigned canonical codes by symbol
  value within each code length, but the normative codebooks (and FFmpeg's
  `ff_vlc_init_from_lengths`) assign them in table order — several PS
  codebooks list same-length symbols out of numeric order, so every
  long-codeword parameter decoded incorrectly. Only the all-zero-codeword
  unit tests had exercised this path before.
- PS per-envelope delta flags (`bs_dt`) were consumed even when their
  parameter family (IID/ICC) was disabled, shifting every subsequent read
  in the payload; the reference only reads them for enabled families.
- ICC parameters rejected only values above 7 but accepted negatives; the
  reference rejects negative accumulated ICC (unsigned compare). IPD/OPD
  deltas now wrap modulo 8 (reference `MASK` 0x07) instead of failing on
  values above the old 5-bit cap, and their read no longer caps at 9 bits
  (the codebooks contain codes up to 14/17 bits — long codewords used to
  fail the whole PS payload).
- The end-of-frame reference fix-ups were missing: a fake final envelope
  is now appended whenever the parameter run ends before the last QMF
  slot (so synthesis-time interpolation covers the whole frame), and
  20/34-band mode history (`is34bands`/`is34bands_old`) is recomputed at
  end-of-frame from both the IID and ICC modes instead of only inside the
  IID header.
- Reserved SBR extension payloads used `read_bits(count)` with the full
  remaining bit count, hitting a >32-bit debug assertion (and mis-
  skipping in release builds) on any HE-AAC stream whose FIL element
  carried non-SBR extended data.
- `OggOpusEncoder::finish()` was not idempotent (a second call or drop
  path could rewrite EOS bookkeeping).

### Known limitations (tracked in `todo.md`)
- Parametric Stereo synthesis is implemented (see Added above). HE-AACv2
  with explicit AOT 29 signaling decodes as stereo, and mono HE-AAC
  streams with in-band PS flip to stereo; a from-source libfdk-aac
  end-to-end fixture decodes at 98.4 dB whole-stream SNR against FFmpeg
  (per-frame 82-139 dB). Explicit HE-AAC AOT 5 streams whose SBR payloads
  contain PS data still keep mono output (the container explicitly said
  "no PS", matching the reference decoder's behavior of ignoring PS in
  that configuration).
- Four AAC FATE multichannel conformance items (CCE/PCE coupling) decode at
  reduced fidelity (2-53 dB) pending a coupling/PCE-interaction root cause.
  The previous stereo and 5.1/7.1 SBR context bugs are fixed; HE-AAC/SBR
  self-generated fixtures now reach roughly 117–133 dB SNR against FFmpeg.
- The CELT-only Ogg Opus encoder's CBR storage/allocation mismatch is fixed:
  range-coded output now uses libopus-style fixed-size storage, including
  correct final carry flushing and partial raw-bit placement. Mono and stereo
  frames remain exactly on budget across multiple rates, preventing the
  decoder-side PVQ reallocation that previously caused silent corruption.
- The Ogg Opus writer now signals the measured 120-sample CELT algorithmic
  delay as RFC 7845 pre-skip, offsets all audio granules, flushes delayed tail
  frames, and sets the EOS granule to `input_samples + 120`. Wire-level header
  and granule assertions plus exact-length round trips verify gapless sample
  recovery. SILK/hybrid encoding and psychoacoustic tuning remain out of scope.
- Broader official ISO/IEC AAC and MP3 conformance vector suites are not
  obtainable/integrated; AAC and MP3 correctness currently rest on FFmpeg-
  reference comparison rather than official test vectors.
- MP3 encoder produces valid, independently-FFmpeg-decodable bitstreams. The
  analysis polyphase path matches shine's reverse circular-buffer fill;
  forward MDCT/antialias/change-sign precompensation, escape-Huffman pair
  orientation, count1 zero-width handling, MPEG-1 stereo side-info field
  order, and mono/stereo synthesis state now have regression coverage.
  Active mono and independent-stereo end-to-end fidelity gates pass within
  the reduced flat-gain/no-reservoir encoder scope. Psychoacoustic tuning,
  bit-reservoir borrowing, and full feature-set expansion remain out of scope.
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
