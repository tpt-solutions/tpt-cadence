# tpt-av-cadence-flac

FLAC (lossless, LPC + Rice coding) parser, decoder and encoder (`FlacEncoder`) for the
`tpt-cadence` audio codec suite.

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

## Encoding

`FlacEncoder` writes fixed-blocksize FLAC. Each frame is coded with the
cheapest of Verbatim, Fixed, or LPC subframes. The LPC order (up to 12) comes
from one windowed Levinson-Durbin pass and is priced by actual coded size.
Stereo frames choose independent, left/side, side/right, or mid/side coding.
Wasted bits are shifted out, and STREAMINFO carries a real MD5 and min/max frame
sizes. Runnable example:
`cargo run -p tpt-av-cadence-flac --example flac_encode -- tone.flac`.

```rust
use std::fs::File;
use tpt_av_cadence_core::Encoder;
use tpt_av_cadence_flac::FlacEncoder;

// 44.1 kHz, stereo, 16 bits per sample.
let mut enc = FlacEncoder::new(File::create("out.flac")?, 44_100, 2, 16)?;
enc.encode(&interleaved_f32)?;
enc.finish()?; // writes the STREAMINFO MD5 and frame sizes
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
