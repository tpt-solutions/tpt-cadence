# tpt-av-cadence-aiff

AIFF/AIFC (big-endian IFF) parser and decoder for the `tpt-cadence` audio codec suite.

**Status:** ✅ Stable, bit-exact conformance tested. Supports the classic AIFF
encodings (signed 8/16/24/32-bit big-endian PCM) and the AIFF-C compression
types `NONE`, `twos` (big-endian PCM), `sowt` (little-endian PCM), `FL32`,
`in24`, and `ni24`. Compressed types such as `ima4`, `ULAW`, and `ALAW` are
rejected with `CadenceError::UnsupportedFeature`.

## Usage

```rust
use std::fs::File;
use tpt_av_cadence_core::{Decoder, FormatReader};
use tpt_av_cadence_aiff::AiffReader;

let mut reader = AiffReader::open(Box::new(File::open("track.aif")?))?;
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
