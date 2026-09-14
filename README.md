# tpt-cadence

**A pure-Rust, zero-dependency audio codec suite. Memory-safe, real-time capable, and permissively licensed (MIT OR Apache-2.0).**

**Status:** Early-stage / Pre-1.0
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
| [`tpt-av-cadence-pcm`](tpt-av-cadence-pcm) | Headerless raw PCM | ✅ Stable |
| [`tpt-av-cadence-wav`](tpt-av-cadence-wav) | RIFF/WAVE — 8/16/24/32-bit int + 32/64-bit float | ✅ Stable |
| [`tpt-av-cadence-aiff`](tpt-av-cadence-aiff) | AIFF / AIFC (big-endian IFF) | ✅ Stable |
| [`tpt-av-cadence-flac`](tpt-av-cadence-flac) | FLAC (lossless, LPC + Rice coding) | ✅ Stable |
| [`tpt-av-cadence-opus`](tpt-av-cadence-opus) | Opus (RFC 6716) | 🚧 In progress |
| [`tpt-av-cadence-aac`](tpt-av-cadence-aac) | AAC-LC | 🚧 In progress |
| [`tpt-av-cadence-mp3`](tpt-av-cadence-mp3) | MPEG Layer III | 🚧 In progress |
| [`tpt-av-cadence-vorbis`](tpt-av-cadence-vorbis) | Ogg Vorbis | 🚧 In progress |

## Quickstart

Decode a WAV file to interleaved `f32` samples:

```rust
use std::io::Cursor;
use tpt_av_cadence_core::{Decoder, FormatReader};
use tpt_av_cadence_wav::WavReader;

let file = std::fs::File::open("music.wav")?;
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

## Repository Layout

All sub-crates share the `tpt-av-cadence-` prefix for ecosystem coherence and clean
namespace resolution on crates.io. See [DESIGN.md](DESIGN.md) §3 for the full tree.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). In short: contributions are dual-licensed
MIT OR Apache-2.0, no copyleft dependencies, and every new decoder must ship
bit-exact conformance tests against a reference implementation.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
