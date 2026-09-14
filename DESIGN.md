# tpt-cadence — Design Document

> The living design doc for the project. (Carried over from the original `spec.txt`.)

**A pure-Rust, zero-dependency audio codec suite. Memory-safe, real-time capable, and 100% permissively licensed (MIT OR Apache-2.0).**

**License:** MIT OR Apache-2.0  
**Status:** Early-stage / Pre-1.0  
**Ecosystem:** [TPT Solutions Open Source](https://opensource.tptsolutions.co.nz/)

---

## 1. Vision & Philosophy

`tpt-cadence` is the **audio codec layer** of the TPT AV Stack. It provides pure-Rust, spec-compliant decoders (and eventually encoders) for every major audio format, written entirely from the specification up with zero external audio dependencies.

The Rust audio ecosystem currently relies on a fragmented patchwork of C bindings (`ffmpeg-sys`, `opus-sys`), weak-copyleft libraries (`symphonia` under MPL-2.0), and single-format crates (`hound`, `claxon`). There is no unified, permissively licensed, pure-Rust audio codec suite.

`tpt-cadence` fills that gap.

### Core Tenets

1. **Pure Rust, Zero External Audio Dependencies:** No FFmpeg. No `symphonia`. No `hound`. No C bindings. Every parser and decoder is written from the format specification in pure Rust.
2. **Strict MIT Licensing:** The entire dependency tree is audited to ensure zero copyleft (GPL/LGPL) and zero weak-copyleft (MPL) contamination. Enforced via `cargo-deny` in CI.
3. **Real-Time Safe Decoding:** All heap allocation occurs during initialization. The `decode()` method writes directly into a caller-provided buffer and is guaranteed to be allocation-free, lock-free, and panic-free.
4. **Bit-Exact Conformance:** Every decoder is validated against official reference test vectors and cross-checked against FFmpeg output to guarantee spec-compliant, bit-exact PCM output.
5. **Feature-Gated Modularity:** Each format is an optional Cargo feature. Users compile only the codecs they need, keeping binary size and compile times minimal.

---

## 2. Ecosystem Integration

`tpt-cadence` sits in the **Codec Layer** of the TPT AV Stack, between the media foundation and the audio processing engine.

| Crate | Role | Relationship to `tpt-cadence` |
| :--- | :--- | :--- |
| **`tpt-kinetix`** | Media containers, video codecs, streaming | Demuxes containers (MP4, MKV, OGG) and extracts raw audio packets. Passes them to `tpt-cadence` for decoding. |
| **`tpt-cadence`** | **Audio codecs (this repo)** | Receives raw audio data (from files or from `tpt-kinetix` demuxers) and decodes it into PCM `f32` samples. |
| **`tpt-audio`** | Audio processing, timeline, mixing, I/O | Consumes the decoded PCM from `tpt-cadence` and feeds it into the real-time mixing graph. |

### Data Flow
File on Disk / Network Stream
↓
tpt-kinetix-demux (parses container, extracts audio packets)
↓
tpt-cadence (decodes audio packets → raw PCM f32)
↓
tpt-audio-core (mixes, applies effects, outputs to hardware)


`tpt-cadence` can also operate standalone: it can read audio files directly from disk without `tpt-kinetix`, making it useful for simple audio tools that don't need container demuxing.

---

## 3. Repository Architecture (Cargo Workspace)

All sub-crates share the `tpt-av-cadence-` prefix for ecosystem coherence and clean namespace resolution on crates.io.

```text
tpt-cadence/                        # GitHub Repository / Workspace Root
├── Cargo.toml                      # Workspace manifest
├── deny.toml                       # cargo-deny license audit config
├── LICENSE-MIT
├── README.md
├── DESIGN.md                       # This file
│
├── tpt-av-cadence-core/            # Shared traits, types, and error handling
│   ├── src/
│   │   ├── lib.rs
│   │   ├── decoder.rs              # The unified `Decoder` trait
│   │   ├── encoder.rs              # The unified `Encoder` trait (future)
│   │   ├── format.rs               # `Format`, `SampleFormat`, `ChannelLayout` enums
│   │   ├── stream_info.rs          # `StreamInfo` metadata struct
│   │   └── error.rs                # `CadenceError` enum
│   └── Cargo.toml
│
├── tpt-av-cadence-wav/             # RIFF/WAV parser and decoder
│   ├── src/
│   │   ├── lib.rs
│   │   ├── reader.rs               # RIFF chunk parser
│   │   ├── decoder.rs              # PCM/IEEE Float decoder
│   │   └── writer.rs               # WAV encoder (future)
│   ├── Cargo.toml
│   └── tests/
│       └── conformance.rs
│
├── tpt-av-cadence-aiff/            # AIFF/AIFC parser and decoder
│   ├── src/
│   ├── Cargo.toml
│   └── tests/
│
├── tpt-av-cadence-flac/            # FLAC parser and decoder
│   ├── src/
│   │   ├── lib.rs
│   │   ├── stream.rs               # FLAC stream/frame parser
│   │   ├── subframe.rs             # Subframe decoding (Constant, Verbatim, Fixed, LPC)
│   │   ├── rice.rs                 # Rice coding decoder
│   │   ├── lpc.rs                  # LPC prediction
│   │   └── decoder.rs              # Top-level `Decoder` impl
│   ├── Cargo.toml
│   └── tests/
│       └── conformance.rs
│
├── tpt-av-cadence-opus/            # Opus decoder (SILK + CELT)
│   ├── src/
│   ├── Cargo.toml
│   └── tests/
│
├── tpt-av-cadence-aac/             # AAC-LC decoder (migrated from tpt-kinetix)
│   ├── src/
│   │   ├── lib.rs
│   │   ├── adts.rs                 # ADTS header parser
│   │   ├── audio_specific.rs       # AudioSpecificConfig parser
│   │   ├── huffman.rs              # Huffman decoding tables
│   │   ├── imdct.rs                # IMDCT transform
│   │   ├── tns.rs                  # Temporal Noise Shaping
│   │   ├── pns.rs                  # Perceptual Noise Substitution
│   │   ├── stereo.rs               # Mid/Side and Intensity stereo
│   │   └── decoder.rs              # Top-level `Decoder` impl
│   ├── Cargo.toml
│   └── tests/
│       └── conformance.rs
│
├── tpt-av-cadence-mp3/             # MPEG Layer III decoder (future)
│   ├── src/
│   ├── Cargo.toml
│   └── tests/
│
├── tpt-av-cadence-vorbis/          # OGG Vorbis decoder (future)
│   ├── src/
│   ├── Cargo.toml
│   └── tests/
│
├── tpt-av-cadence-pcm/             # Raw headerless PCM reader
│   ├── src/
│   ├── Cargo.toml
│   └── tests/
│
└── tpt-av-cadence-test-utils/      # Shared conformance testing harness
    ├── src/
    │   ├── lib.rs
    │   ├── reference.rs            # FFmpeg subprocess comparison
    │   └── fuzz.rs                 # Property-based fuzzing helpers
    └── Cargo.toml

4. Core API Design
4.1. The Decoder Trait (tpt-av-cadence-core)
This is the single most important interface in the entire crate. Every format implements it. The tpt-audio engine calls it during playback.

/// Metadata about a decoded audio stream.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    /// Sample rate in Hz (e.g., 44100, 48000, 96000).
    pub sample_rate: u32,
    /// Number of audio channels.
    pub channels: u16,
    /// Channel layout (e.g., Stereo, 5.1, Mono).
    pub channel_layout: ChannelLayout,
    /// Original bit depth of the source (e.g., 16, 24, 32).
    pub bit_depth: u16,
    /// Total number of sample frames, if known.
    /// `None` for live streams or formats without frame counts.
    pub total_frames: Option<u64>,
    /// The source audio format.
    pub format: Format,
}

/// The core trait every audio codec decoder implements.
///
/// # Real-Time Safety Contract
///
/// After `init()` completes, the `decode()` method MUST be:
/// - Allocation-free (no heap allocations)
/// - Lock-free (no mutexes, no atomics with contention)
/// - Panic-free (returns `Result`, never unwraps)
///
/// All memory required for decoding (Huffman tables, MDCT windows,
/// LPC coefficients, etc.) MUST be allocated during initialization.
pub trait Decoder: Send {
    /// Returns immutable stream metadata. Allocation-free.
    fn info(&self) -> &StreamInfo;

    /// Seeks to an exact sample frame position.
    ///
    /// This method MAY allocate and MAY block (e.g., reading from disk).
    /// It is NOT real-time safe. Call it only from background threads.
    fn seek(&mut self, frame: u64) -> Result<(), CadenceError>;

    /// Decodes the next chunk of audio into the provided buffer.
    ///
    /// Samples are written as interleaved `f32` values in the range `[-1.0, 1.0]`.
    /// Returns the number of **frames** written (not samples).
    ///
    /// # Real-Time Safety
    ///
    /// This method is guaranteed to be allocation-free, lock-free, and
    /// panic-free after initialization. It is safe to call from the
    /// audio callback thread.
    ///
    /// # Arguments
    ///
    /// * `buffer` - A caller-provided slice to write interleaved f32 samples into.
    ///              The slice length must be a multiple of `info().channels`.
    ///
    /// # Returns
    ///
    /// * `Ok(frames_written)` - Number of frames decoded.
    /// * `Ok(0)` - End of stream reached.
    /// * `Err(CadenceError)` - Decoding error (corrupt data, etc.).
    fn decode(&mut self, buffer: &mut [f32]) -> Result<usize, CadenceError>;
}

4.2. The FormatReader Trait
For formats that can read directly from files (WAV, FLAC, AIFF), a higher-level trait handles file I/O:

/// Reads audio data from a byte source (file, memory, network).
///
/// Unlike `Decoder`, this trait handles I/O and MAY allocate.
/// It is intended for use on background threads, not the audio thread.
pub trait FormatReader: Send {
    /// Opens and parses the audio source.
    fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError>
    where
        Self: Sized;

    /// Returns the underlying `Decoder` for real-time PCM extraction.
    fn decoder(&mut self) -> &mut dyn Decoder;

    /// Returns stream metadata.
    fn info(&self) -> &StreamInfo;
}

4.3. Error Handling

#[derive(Debug)]
pub enum CadenceError {
    /// The file or stream is not a valid instance of this format.
    InvalidFormat(String),
    /// The format is recognized but uses unsupported features.
    UnsupportedFeature(String),
    /// Corrupt or truncated data encountered during decoding.
    CorruptData(String),
    /// An I/O error occurred (only from `FormatReader`, never from `Decoder::decode`).
    IoError(std::io::Error),
    /// The seek position is out of range.
    SeekOutOfRange { requested: u64, total: u64 },
    /// End of stream.
    EndOfStream,
}

5. Format Priority & Roadmap
Formats are prioritized by what a professional audio editor or DAW needs on day one.
Phase 1: Foundation (Current)
Format
Crate
Complexity
Rationale
Raw PCM
tpt-av-cadence-pcm
Trivial
Headerless audio. Essential for testing and interop.
WAV/RIFF
tpt-av-cadence-wav
Low
The universal exchange format. Every DAW must read/write WAV. Supports 8/16/24/32-bit integer and 32/64-bit float.
Phase 1 Deliverables:
tpt-av-cadence-core with Decoder trait, StreamInfo, CadenceError
tpt-av-cadence-pcm with raw PCM reader
tpt-av-cadence-wav with full RIFF/WAV parser and decoder
tpt-av-cadence-test-utils with FFmpeg comparison harness
Bit-exact conformance tests for WAV (8/16/24/32-bit, mono/stereo, float)
cargo-deny CI pipeline enforcing pure MIT
Phase 2: Lossless & Migration
Format
Crate
Complexity
Rationale
AIFF/AIFC
tpt-av-cadence-aiff
Low
Nearly identical to WAV but big-endian. Critical for macOS/Logic Pro users.
FLAC
tpt-av-cadence-flac
Medium
The standard for lossless audio. LPC + Rice coding. Well-documented spec.
AAC-LC
tpt-av-cadence-aac
High
Migrated from tpt-kinetix-aac. Already bit-exact vs FFmpeg. Needs repackaging into the new trait system.
Phase 2 Deliverables:
AIFF parser (big-endian IFF chunks)
FLAC decoder (frame parsing, subframe types, Rice coding, LPC)
AAC-LC migration from tpt-kinetix into tpt-av-cadence-aac
Conformance tests against official FLAC test suite
Conformance tests against ITU-T AAC reference vectors
Phase 3: Modern Compressed
Format
Crate
Complexity
Rationale
Opus
tpt-av-cadence-opus
Very High
Modern standard for low-latency audio. SILK + CELT hybrid decoder. Essential for podcasting and real-time communication.
Phase 3 Deliverables:
Opus packet parser (RFC 6716)
SILK decoder (LP-based, speech-optimized)
CELT decoder (MDCT-based, music-optimized)
Hybrid mode integration
Conformance tests against official Opus test vectors
Phase 4: Legacy & Open Source
Format
Crate
Complexity
Rationale
MP3
tpt-av-cadence-mp3
Very High
Ubiquitous but complex (Huffman, polyphase filterbank, joint stereo).
Vorbis
tpt-av-cadence-vorbis
High
Open-source compressed format. MDCT-based.
6. Real-Time Safety Architecture
The most critical design constraint in tpt-cadence is the boundary between initialization and decoding.
Initialization Phase (Background Thread)

┌──────────────────────────────────────────────────┐
│  ALLOWED:                                         │
│  ✅ Heap allocation (Vec, Box, HashMap)           │
│  ✅ File I/O (reading headers, seeking)           │
│  ✅ Parsing (Huffman tables, codebooks)           │
│  ✅ Pre-computation (MDCT windows, LPC filters)   │
│  ✅ Blocking operations                           │
└──────────────────────────────────────────────────┘

Decoding Phase (Audio Thread)

┌──────────────────────────────────────────────────┐
│  REQUIRED:                                        │
│  ❌ No heap allocation                            │
│  ❌ No file I/O                                   │
│  ❌ No mutexes or locks                           │
│  ❌ No panics (unwrap, expect, index out of bounds)│
│  ❌ No blocking operations                        │
│  ✅ Write only into caller-provided buffer        │
│  ✅ Stack-only arithmetic and memory access       │
└──────────────────────────────────────────────────┘

The Two-Thread Pipeline

Background Thread (can allocate, can block)
│
│  tpt-av-cadence-wav::Decoder reads from disk
│       ↓
│  Decodes PCM into a pre-allocated ring buffer
│       ↓
│  Lock-free handoff (atomic pointer swap)
│
Audio Thread (ZERO allocation, ZERO blocking)
│
│  tpt-av-audio-core reads pre-decoded PCM
│       ↓
│  Applies gain, pan, fades, mixing
│       ↓
│  Outputs to tpt-av-audio-io

7. Conformance Testing Strategy
Every decoder MUST pass bit-exact conformance tests before being considered stable.
7.1. Reference Comparison Harness (tpt-av-cadence-test-utils)

/// Decodes a file with the TPT decoder and with FFmpeg (via CLI subprocess),
/// then asserts the PCM output is bit-exact.
pub fn assert_bit_exact_vs_ffmpeg(
    file_path: &Path,
    tpt_decoder: &mut dyn Decoder,
    tolerance: f32,  // 0.0 for bit-exact, small epsilon for float formats
) -> Result<(), ConformanceError>;

7.2. Test Vector Sources
Format
Reference Source
WAV
Generated via libsndfile and sox. Edge cases: extensible fmt chunk, odd sizes, 8/16/24/32-bit, float32/64.
FLAC
Official FLAC test suite — hundreds of reference files.
AAC
ITU-T H.264.1 conformance suite (already used in tpt-kinetix).
Opus
Official Opus test vectors.
MP3
mpg123 conformance streams.
7.3. Fuzz Testing
All parsers MUST be fuzz-tested with cargo-fuzz and proptest to guarantee they never panic on malformed input:

proptest! {
    #[test]
    fn wav_decoder_never_panics(data in proptest::collection::vec(any::<u8>(), 0..10000)) {
        let cursor = Cursor::new(data);
        let _ = WavDecoder::open(Box::new(cursor));
    }
}

8. Dependency & Licensing Rules
Allowed Dependencies (Permissive Only)
tpt-av-cadence-core (internal)
byteorder (MIT/Apache) — endian-aware byte reading
thiserror (MIT/Apache) — error derive macros
log (MIT/Apache) — logging facade
Banned Dependencies
ffmpeg-sys / ffmpeg-next (LGPL/GPL)
symphonia (MPL-2.0)
hound (MIT, but redundant — we write our own)
claxon (MIT, but redundant)
lewton (MIT, but redundant)
Any crate with GPL, LGPL, AGPL, or MPL in its dependency tree
Enforcement
The deny.toml file in the workspace root enforces this automatically:

[licenses]
unlicensed = "deny"
allow = [
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
]
deny = [
    "GPL-2.0",
    "GPL-3.0",
    "LGPL-2.1",
    "LGPL-3.0",
    "AGPL-3.0",
    "MPL-2.0",
]

9. Workspace Cargo.toml

[workspace]
resolver = "2"
members = [
    "tpt-av-cadence-core",
    "tpt-av-cadence-wav",
    "tpt-av-cadence-aiff",
    "tpt-av-cadence-flac",
    "tpt-av-cadence-opus",
    "tpt-av-cadence-aac",
    "tpt-av-cadence-mp3",
    "tpt-av-cadence-vorbis",
    "tpt-av-cadence-pcm",
    "tpt-av-cadence-test-utils",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT OR Apache-2.0"
repository = "https://github.com/tpt-solutions/tpt-cadence"
rust-version = "1.75"

[workspace.dependencies]
tpt-av-cadence-core = { path = "tpt-av-cadence-core", version = "0.1.0" }
tpt-av-cadence-test-utils = { path = "tpt-av-cadence-test-utils", version = "0.1.0" }
byteorder = "1.5"
thiserror = "2"
log = "0.4"

10. Contributing & License
This project is dual-licensed under the MIT OR Apache-2.0 terms.
By contributing to tpt-cadence, you agree that:
Your code will be licensed under the same dual MIT OR Apache-2.0 terms.
You will not introduce any dependency that is GPL, LGPL, AGPL, or MPL licensed.
All new decoders must include bit-exact conformance tests against a reference implementation.
This ensures tpt-cadence remains free, open, and unencumbered by copyleft restrictions for all future audio software development.


