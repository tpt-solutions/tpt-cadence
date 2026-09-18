# tpt-av-cadence-wav

RIFF/WAVE parser and PCM/IEEE-float decoder for the `tpt-cadence` audio codec suite.

**Status:** ✅ Stable, bit-exact conformance tested (cross-checked against
FFmpeg via `assert_bit_exact_vs_ffmpeg`). Supports 8/16/24/32-bit integer PCM
and 32/64-bit float WAVE files.

## Usage

```rust
use std::fs::File;
use tpt_av_cadence_core::{Decoder, FormatReader};
use tpt_av_cadence_wav::WavReader;

let mut reader = WavReader::open(Box::new(File::open("clip.wav")?))?;

let info = reader.info();
println!("{} Hz, {} channels, {} bit", info.sample_rate, info.channels, info.bit_depth);

let mut buf = vec![0.0f32; 4096 * info.channels as usize];
loop {
    let frames = reader.decoder().decode(&mut buf)?;
    if frames == 0 {
        break; // end of stream
    }
    // `buf[..frames * channels]` now holds interleaved PCM in [-1.0, 1.0].
}
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
