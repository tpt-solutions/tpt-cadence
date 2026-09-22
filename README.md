# tpt-cadence

[![CI](https://github.com/tpt-solutions/tpt-cadence/actions/workflows/ci.yml/badge.svg)](https://github.com/tpt-solutions/tpt-cadence/actions/workflows/ci.yml)

**A pure-Rust, zero-dependency audio codec suite. Memory-safe, real-time capable, and permissively licensed (MIT OR Apache-2.0).**

**Status:** Early-stage / Pre-1.0 — WAV, AIFF, FLAC, and raw PCM are implemented and
bit-exact conformance-tested; AAC-LC and MP3 pass FFmpeg-reference conformance
(>100 dB SNR); Opus and Vorbis are under active development.
**Ecosystem:** [TPT Solutions Open Source](https://opensource.tptsolutions.co.nz/)

`tpt-cadence` is the **audio codec layer** of the TPT AV Stack. It provides pure-Rust,
spec-compliant decoders for every major audio format, written from the specification up
with zero external audio dependencies — no FFmpeg, no `symphonia`, no `hound`, no C bindings.

See [DESIGN.md](DESIGN.md) for the full design rationale.

## Core Tenets

1. **Pure Rust, zero external audio dependencies.**
2. **Permissive licensing only** — the whole dependency tree is audited by `cargo-deny` in CI; no GPL/LGPL/MPL anywhere.
3. **Real-time safe decoding** — all heap allocation happens during `init()`/`open()`; `decode()` writes into a caller-provided buffer and is allocation-free, lock-free, and panic-free.
4. **Bit-exact conformance** — every decoder is validated against official reference vectors and cross-checked against FFmpeg output.
5. **Feature-gated modularity** — each format is its own crate; compile only what you need.

## Ecosystem

| Crate | Role | Relationship to `tpt-cadence` |
| :--- | :--- | :--- |
| **`tpt-kinetix`** | Media containers, video codecs, streaming | Demuxes containers (MP4, MKV, OGG) and extracts raw audio packets, passing them to `tpt-cadence` for decoding. |
| **`tpt-cadence`** | **Audio codecs (this repo)** | Decodes raw audio data into PCM `f32` samples. |
| **`tpt-audio`** | Audio processing, timeline, mixing, I/O | Consumes decoded PCM and feeds it into the real-time mixing graph. |

`tpt-cadence` can also operate standalone: it reads audio files directly from disk without
`tpt-kinetix`, useful for simple audio tools that don't need container demuxing.

## Crates

| Crate | Format | Status |
| :--- | :--- | :--- |
| [`tpt-av-cadence-core`](tpt-av-cadence-core) | `Decoder` trait, `StreamInfo`, shared types | ✅ Core API |
| [`tpt-av-cadence-pcm`](tpt-av-cadence-pcm) | Headerless raw PCM — int 8/16/24/32 + float 32/64, both byte orders | ✅ Stable |
| [`tpt-av-cadence-wav`](tpt-av-cadence-wav) | RIFF/WAVE — 8/16/24/32-bit int + 32/64-bit float | ✅ Stable |
| [`tpt-av-cadence-aiff`](tpt-av-cadence-aiff) | AIFF / AIFC (big-endian IFF) | ✅ Stable |
| [`tpt-av-cadence-flac`](tpt-av-cadence-flac) | FLAC (lossless, LPC + Rice coding) | ✅ Stable |
| [`tpt-av-cadence-opus`](tpt-av-cadence-opus) | Opus (RFC 6716) + Ogg Opus container (RFC 7845) | ✅ Conformance-tested — 100% `final_range` on all 12 official RFC 6716 vectors; SILK-only vectors bit-exact; `Decoder`/`FormatReader` impls |
| [`tpt-av-cadence-ogg`](tpt-av-cadence-ogg) | Ogg page/packet container (RFC 3533) shared by Vorbis and Opus | ✅ In use |
| [`tpt-av-cadence-aac`](tpt-av-cadence-aac) | AAC-LC (ISO/IEC 14496-3) | ✅ Conformance-tested (>100 dB SNR vs FFmpeg) |
| [`tpt-av-cadence-mp3`](tpt-av-cadence-mp3) | MPEG Layer III | ✅ Conformance-tested (>100 dB SNR vs FFmpeg, ten bundled streams) |
| [`tpt-av-cadence-vorbis`](tpt-av-cadence-vorbis) | Ogg Vorbis I | ✅ Conformance-tested (136–138 dB SNR vs FFmpeg on six bundled fixtures) |
| [`tpt-av-cadence-test-utils`](tpt-av-cadence-test-utils) | Conformance harness — FFmpeg comparison, fuzz helpers, MD5 | ✅ Internal (dev-only) |

## Quickstart

Decode a WAV file to interleaved `f32` samples:

```rust
use std::fs::File;
use tpt_av_cadence_core::{Decoder, FormatReader};
use tpt_av_cadence_wav::WavReader;

let file = File::open("music.wav")?;
let mut reader = WavReader::open(Box::new(file))?;

let info = reader.info();
println!("{} Hz, {} channels, {} bit",
    info.sample_rate, info.channels, info.bit_depth);

// `decode()` is allocation-free, lock-free, and panic-free:
// safe to drive from a background decoding thread.
let mut buf = vec![0.0f32; 4096 * info.channels as usize];
loop {
    let frames = reader.decoder().decode(&mut buf)?;
    if frames == 0 {
        break; // end of stream
    }
    // `buf[..frames * channels]` now holds interleaved PCM in [-1.0, 1.0].
}
```

### Quickstart per format

Every decoder crate ships a runnable example that decodes to raw interleaved
`f32` PCM. Each one prints stream info (`sample_rate`, channels, etc.) to
stderr; most write raw PCM to stdout — redirect it to a file:

| Format | Crate | Run it |
| :--- | :--- | :--- |
| WAV | [`tpt-av-cadence-wav`](tpt-av-cadence-wav/examples/wav_decode.rs) | `cargo run -p tpt-av-cadence-wav --example wav_decode -- clip.wav > out.f32` |
| AIFF / AIFF-C | [`tpt-av-cadence-aiff`](tpt-av-cadence-aiff/examples/aiff_decode.rs) | `cargo run -p tpt-av-cadence-aiff --example aiff_decode -- track.aif > out.f32` |
| Raw PCM (headerless) | [`tpt-av-cadence-pcm`](tpt-av-cadence-pcm/examples/pcm_decode.rs) | `cargo run -p tpt-av-cadence-pcm --example pcm_decode -- audio.raw s16le 2 48000 > out.f32` |
| FLAC | [`tpt-av-cadence-flac`](tpt-av-cadence-flac/examples/flac_decode.rs) | `cargo run -p tpt-av-cadence-flac --example flac_decode -- album.flac > out.f32` |
| MP3 | [`tpt-av-cadence-mp3`](tpt-av-cadence-mp3/examples/mp3_decode.rs) | `cargo run -p tpt-av-cadence-mp3 --example mp3_decode -- song.mp3 > out.f32` |
| AAC-LC (+ SBR/HE-AAC) | [`tpt-av-cadence-aac`](tpt-av-cadence-aac/examples/aac_dump.rs) | `cargo run -p tpt-av-cadence-aac --release --example aac_dump -- song.aac out.f32` |
| Ogg Vorbis I | [`tpt-av-cadence-vorbis`](tpt-av-cadence-vorbis/examples/vorbis_decode.rs) | `cargo run -p tpt-av-cadence-vorbis --example vorbis_decode -- file.ogg > out.f32` |
| Opus (RFC 6716) | [`tpt-av-cadence-opus`](tpt-av-cadence-opus) | no dedicated `--example` yet — decode via [`tpt-av-cadence-cli`](tpt-av-cadence-cli) (`cadence decode song.opus`, see below) or `OggOpusReader` directly per the crate's own `# Example` in its `src/lib.rs` doc comment |

`tpt-av-cadence-pcm`'s example takes the sample format on the command line
(`s8`, `s16le`/`s16be`, `s24le`/`s24be`, `s32le`/`s32be`, `f32le`/`f32be`,
`f64le`/`f64be`) since headerless PCM carries no self-describing metadata.

Encoders exist today for the three uncompressed formats (WAV, AIFF, PCM);
there are no compressed-format encoders yet (FLAC/MP3/AAC/Opus/Vorbis are
decode-only):

| Format | Crate | Run it |
| :--- | :--- | :--- |
| WAV | [`tpt-av-cadence-wav`](tpt-av-cadence-wav/examples/wav_encode.rs) | `cargo run -p tpt-av-cadence-wav --example wav_encode -- tone.wav` |
| AIFF-C | [`tpt-av-cadence-aiff`](tpt-av-cadence-aiff/examples/aiff_encode.rs) | `cargo run -p tpt-av-cadence-aiff --example aiff_encode -- tone.aiff` |
| Raw PCM (headerless) | [`tpt-av-cadence-pcm`](tpt-av-cadence-pcm/examples/pcm_encode.rs) | `cargo run -p tpt-av-cadence-pcm --example pcm_encode -- tone.raw` |

### Decode any supported format to PCM

For format auto-detection across the whole suite, the
[`tpt-av-cadence-cli`](tpt-av-cadence-cli) crate (`cadence info <file>` /
`cadence decode <file>`) is the canonical reference — see its
[`src/main.rs`](tpt-av-cadence-cli/src/main.rs) for the full implementation,
including sniffing an `.ogg` file's first page for `OpusHead` vs. `\x01vorbis`
to tell Opus and Vorbis apart. The core of it distills to matching on file
extension and opening the matching reader behind the shared `FormatReader`
trait:

```rust
use std::fs::File;
use std::path::Path;
use tpt_av_cadence_core::{CadenceError, FormatReader};

fn open_any(path: &Path) -> Result<Box<dyn FormatReader>, CadenceError> {
    let file = File::open(path).expect("open input");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();

    let reader: Box<dyn FormatReader> = match ext.to_ascii_lowercase().as_str() {
        "wav" | "wave" => Box::new(tpt_av_cadence_wav::WavReader::open(Box::new(file))?),
        "aif" | "aiff" | "aifc" => Box::new(tpt_av_cadence_aiff::AiffReader::open(Box::new(file))?),
        "flac" => Box::new(tpt_av_cadence_flac::FlacReader::open(Box::new(file))?),
        "mp3" => Box::new(tpt_av_cadence_mp3::Mp3Reader::open(Box::new(file))?),
        "aac" | "adts" => Box::new(tpt_av_cadence_aac::AacReader::open(Box::new(file))?),
        "opus" => Box::new(tpt_av_cadence_opus::OggOpusReader::open(Box::new(file))?),
        // ".ogg"/".oga" need the OpusHead/vorbis sniff — see main.rs's `sniff_ogg`.
        other => panic!("unsupported extension: {other}"),
    };
    Ok(reader)
}

// Then decode identically regardless of format:
let mut reader = open_any(Path::new("music.wav"))?;
let mut buf = vec![0.0f32; 4096 * reader.info().channels as usize];
loop {
    let frames = reader.decoder().decode(&mut buf)?;
    if frames == 0 { break; } // end of stream
    // buf[..frames * channels] is interleaved f32 PCM in [-1.0, 1.0].
}
```

Headerless raw PCM isn't in this dispatch — with no header, there's nothing
to auto-detect the sample format *from*; use `tpt-av-cadence-pcm` directly
with the format supplied out-of-band, as in `pcm_decode` above. A
`cargo-generate` template isn't provided (the snippet above plus
`tpt-av-cadence-cli`'s full implementation cover the same ground); one may
be added later if there's demand for a scaffolded starter project.

## Comparison to other Rust audio crates

An honest comparison, not a sales pitch — some of these projects are more
mature and more broadly capable than `tpt-cadence` today.

| | `tpt-cadence` | [`symphonia`](https://github.com/pdeljanov/Symphonia) | [`hound`](https://github.com/ruuda/hound) | [`minimp3-rs`](https://github.com/germangb/minimp3-rs) |
| :--- | :--- | :--- | :--- | :--- |
| Scope | WAV/AIFF/PCM/FLAC decode+encode; MP3/AAC-LC+SBR/Opus/Vorbis decode-only | Very broad: demuxing (MP4/MKV/OGG/...) plus WAV, FLAC, MP3, AAC, Vorbis, Opus, ALAC, ADPCM decoders | WAV read + write only | MP3 decode only |
| Implementation | Pure Rust, zero external audio dependencies, no `unsafe` in the decode/encode path (verified: no `unsafe` blocks, no `-sys` crates in the workspace) | Pure Rust | Pure Rust | Wraps the C `minimp3` library by default via FFI (`unsafe`); has an optional pure-Rust backend |
| Conformance | Every decoder is validated against official reference vectors (e.g. bit-exact on all 12 RFC 6716 Opus test vectors, IETF FLAC testbench MD5s) and/or cross-checked against FFmpeg output at >100 dB SNR | Broad real-world use and testing; no published bit-exactness claims of this kind | Simple round-trip tests (WAV has no lossy path to conform against) | Relies on the underlying `minimp3` C decoder's own correctness |
| Real-time safety | Explicit contract: all allocation happens at `open()`/`init()`; `decode()`/`encode()` are allocation-free, lock-free, and panic-free | Not a documented contract | N/A (simple I/O) | Not a documented contract |
| Licensing | MIT OR Apache-2.0; whole dependency tree enforced permissive-only in CI via `cargo-deny` (no GPL/LGPL/MPL) | MPL-2.0 (copyleft on the crate itself) | MIT OR Apache-2.0 | MIT (bindings); links a C library at build time |
| Maturity | Pre-1.0, very young (started Sept 2026), not yet published to crates.io | Mature, widely deployed, large contributor base | Mature, narrow scope, widely used | Mature, narrow scope |

Format-specific incumbents worth knowing about too:

- [`claxon`](https://github.com/ruuda/claxon) — pure-Rust FLAC decoder, mature and widely used; `tpt-av-cadence-flac` covers the same ground plus an encoder, with conformance pinned to the official IETF vectors.
- [`lewton`](https://github.com/RustAudio/lewton) — pure-Rust Vorbis decoder; `tpt-av-cadence-vorbis` is a newer, independently-from-spec implementation, cross-checked against FFmpeg (136-138 dB SNR on the bundled fixtures) rather than against `lewton` itself.
- [`audiopus`](https://github.com/lakelezz/audiopus) / the `opus` crate — Rust bindings to the reference C `libopus` via FFI (`unsafe`, requires a system or vendored libopus build); `tpt-av-cadence-opus` is a from-spec pure-Rust reimplementation with no C dependency, validated against the same RFC 6716 test vectors libopus itself ships.

**Why pick `tpt-cadence` today:** you want a pure-Rust dependency tree with
no C bindings or `unsafe` anywhere in the codec path, permissive licensing
enforced by CI, and decoders that are individually conformance-tested
against official references rather than only exercised in aggregate.

**Why you might not (yet):** it's a young, pre-1.0 project — smaller
ecosystem and less battle-tested in production than `symphonia`; no
compressed-format encoders yet; no container demuxing (that's `tpt-kinetix`'s
job, not this crate's); not yet published to crates.io; and there's no
published benchmark suite yet, so no performance claims are made here one
way or the other. Development so far has been on Windows/MSVC, though CI
exercises Linux, macOS, and Windows on every change.

## Repository Layout

All sub-crates share the `tpt-av-cadence-` prefix for ecosystem coherence and clean
namespace resolution on crates.io. See [DESIGN.md](DESIGN.md) §3 for the full tree.

## Building & Testing

Requires Rust **1.75+** (the MSRV enforced in CI). CI builds and tests the workspace on
Linux, Windows, and macOS:

```sh
cargo build --workspace --all-targets
cargo test --workspace                                        # includes bit-exact conformance suites
cargo fmt --all -- --check                                    # CI also runs clippy -D warnings
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check licenses                                     # MIT/Apache-only dependency audit
```

Conformance testing is anchored by [`tpt-av-cadence-test-utils`](tpt-av-cadence-test-utils):

- FLAC output is MD5-verified against the official IETF decoder testbench vectors
  (CC0, bundled under `tpt-av-cadence-flac/tests/data/`).
- WAV is cross-checked against FFmpeg via `assert_bit_exact_vs_ffmpeg`; tests skip
  gracefully when FFmpeg is not installed locally.

## Contributing

This project does not accept pull requests. See [CONTRIBUTING.md](CONTRIBUTING.md) for
how to report bugs or request features via GitHub issues.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
