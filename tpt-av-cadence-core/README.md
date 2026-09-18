# tpt-av-cadence-core

Core traits and types for the `tpt-cadence` audio codec suite: the `Decoder` trait,
`StreamInfo`, format enums, and error handling.

**Status:** ✅ Stable core API — every other crate in the suite depends on this one.

This crate has no decoder of its own. It defines the shared contract every format
decoder implements:

- **`Decoder`** — the single most important interface in the suite. After
  `init()` completes, `decode()` MUST be allocation-free, lock-free, and
  panic-free, so it can be driven from a real-time audio thread.
- **`FormatReader`** — for formats that read directly from files (WAV, AIFF,
  FLAC), handles file I/O and header parsing, then hands out a `&mut dyn Decoder`
  for real-time PCM extraction.
- **`StreamInfo`**, `Format`, `SampleFormat`, `ChannelLayout` — shared metadata
  and format enums.
- **`CadenceError`** — the unified error type returned by every decoder.

## Usage

```rust
use tpt_av_cadence_core::{Decoder, FormatReader, StreamInfo};

fn print_info(info: &StreamInfo) {
    println!("{} Hz, {} channels, {} bit", info.sample_rate, info.channels, info.bit_depth);
}
```

Concrete decoders are provided by the format-specific crates, e.g.
[`tpt-av-cadence-wav`](../tpt-av-cadence-wav), [`tpt-av-cadence-flac`](../tpt-av-cadence-flac).

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
