# tpt-av-cadence-pcm

Raw headerless PCM reader for the `tpt-cadence` audio codec suite.

**Status:** ✅ Stable — supports signed/unsigned integer PCM (8/16/24/32-bit) and
IEEE float PCM (32/64-bit), in both little-endian and big-endian byte order.

Unlike WAV or AIFF, raw PCM has no container header describing its own layout,
so the caller supplies the format up front.

## Usage

```rust
use std::fs::File;
use tpt_av_cadence_core::{Decoder, SampleFormat};
use tpt_av_cadence_pcm::{ByteOrder, PcmFormat, PcmReader};

let file = File::open("raw.pcm")?;
let format = PcmFormat {
    sample_format: SampleFormat::Int16,
    byte_order: ByteOrder::Little,
    channels: 2,
    sample_rate: 44_100,
};
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

`PcmFormat` has public fields (`sample_format`, `byte_order`, `channels`,
`sample_rate`). The `decode` example in `examples/pcm_decode.rs` maps
command-line names such as `s16le` or `f32be` onto them.

## Encoding

`PcmEncoder` writes the same headerless layout. Sample conversion follows the
`PcmFormat` you supply. Runnable example:
`cargo run -p tpt-av-cadence-pcm --example pcm_encode -- tone.raw`.

```rust
use std::fs::File;
use tpt_av_cadence_core::{Encoder, SampleFormat};
use tpt_av_cadence_pcm::{ByteOrder, PcmEncoder, PcmFormat};

let format = PcmFormat {
    sample_format: SampleFormat::Int16,
    byte_order: ByteOrder::Little,
    channels: 2,
    sample_rate: 44_100,
};
let mut enc = PcmEncoder::new(File::create("out.raw")?, format)?;
enc.encode(&interleaved_f32)?;
enc.finish()?;
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
