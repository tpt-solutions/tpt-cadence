# tpt-av-cadence-pcm

Raw headerless PCM reader for the `tpt-cadence` audio codec suite.

**Status:** ✅ Stable — supports signed/unsigned integer PCM (8/16/24/32-bit) and
IEEE float PCM (32/64-bit), in both little-endian and big-endian byte order.

Unlike WAV or AIFF, raw PCM has no container header describing its own layout,
so the caller supplies the format up front.

## Usage

```rust
use std::fs::File;
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_pcm::{PcmFormat, PcmReader};

let file = File::open("raw.pcm")?;
let format = PcmFormat::signed_16le(44_100, 2); // sample rate, channels
let mut reader = PcmReader::open_with_format(Box::new(file), format)?;

let info = reader.info();
let mut buf = vec![0.0f32; 4096 * info.channels as usize];
loop {
    let frames = reader.decoder().decode(&mut buf)?;
    if frames == 0 {
        break;
    }
    // `buf[..frames * channels]` now holds interleaved PCM in [-1.0, 1.0].
}
```

(Exact `PcmFormat` constructors depend on the version in this crate — see
`src/lib.rs` for the full set of supported sample layouts.)

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
