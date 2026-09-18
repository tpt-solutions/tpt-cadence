# tpt-av-cadence-opus

Opus (RFC 6716) decoder for the `tpt-cadence` audio codec suite.

**Status:** 🚧 In progress. Implemented and unit-tested so far:

- Packet parser (TOC byte, frame framing)
- Range coder
- Full CELT decoder
- Full SILK decoder

Known open issues (tracked in [`todo.md`](../todo.md) at the repository root):

- CELT has an unresolved bit-exactness bug — a residual `final_range` desync
  versus the RFC 6716 conformance vectors.
- Hybrid SILK+CELT mode is not yet implemented.
- The decoder has not yet been validated end-to-end against the official
  RFC 6716 test vectors.

This crate is not yet recommended for production use. Follow
[`todo.md`](../todo.md) for current debugging status.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
