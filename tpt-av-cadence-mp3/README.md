# tpt-av-cadence-mp3

MPEG Layer III (MP3) decoder for the `tpt-cadence` audio codec suite.

**Status:** 🚧 Functional, but not yet fully audited. Achieves >100dB SNR /
<=1e-5 peak error versus FFmpeg across all ten bundled test streams. A
real-time-safety audit and broader official conformance testing are still
open items.

## Usage

The public decoder API follows the same `Decoder`/`FormatReader` shape used
throughout the suite (see the root [README](../README.md#quickstart) for the
general pattern). Consult `src/lib.rs` in this crate for the exact reader
type and entry points, as the public API is still settling while conformance
work continues.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
