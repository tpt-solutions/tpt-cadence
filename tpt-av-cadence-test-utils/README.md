# tpt-av-cadence-test-utils

Shared conformance-testing harness for `tpt-cadence`: FFmpeg comparison, fuzz
helpers, and checksum utilities.

**Status:** ✅ Internal dev-only crate.

> **This crate is for internal use within the `tpt-cadence` workspace only.**
> It is not intended for external consumers and provides no semver or API
> stability guarantees. Do not depend on it from outside this repository.

It anchors conformance testing across the other crates:

- FLAC output is MD5-verified against the official IETF decoder testbench
  vectors (CC0, bundled under `tpt-av-cadence-flac/tests/data/`).
- WAV is cross-checked against FFmpeg via `assert_bit_exact_vs_ffmpeg`; tests
  skip gracefully when FFmpeg is not installed locally.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
