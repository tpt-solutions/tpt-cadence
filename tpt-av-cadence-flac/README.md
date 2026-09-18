# tpt-av-cadence-flac

FLAC (lossless, LPC + Rice coding) parser and decoder for the `tpt-cadence`
audio codec suite.

**Status:** ✅ Stable — MD5-verified against the official IETF FLAC decoder
testbench vectors.

Supports the full frame/subframe model: Constant, Verbatim, Fixed (orders
0-4), and LPC (orders 1-32) subframes, partitioned Rice and Rice2 residuals
with escaped partitions, wasted-bit removal, stereo decorrelation
(left/side, right/side, mid/side), fixed and variable block sizes, CRC-8/CRC-16
integrity checking, and STREAMINFO metadata.

## Usage

```rust
use std::fs::File;
use tpt_av_cadence_core::{Decoder, FormatReader};
use tpt_av_cadence_flac::FlacReader;

let mut reader = FlacReader::open(Box::new(File::open("album.flac")?))?;
let channels = reader.info().channels as usize;
let mut buf = vec![0.0f32; 4096 * channels];
loop {
    let frames = reader.decoder().decode(&mut buf)?;
    if frames == 0 {
        break;
    }
}
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
