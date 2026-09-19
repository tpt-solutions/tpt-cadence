# tpt-av-cadence-ogg

Ogg page/packet container layer (RFC 3533) for the `tpt-cadence` audio
codec suite, shared by the codecs that ride in Ogg: Vorbis I and Opus
(RFC 7845).

**Status:** ✅ In use — the page/packet layer used by
`tpt-av-cadence-vorbis` and `tpt-av-cadence-opus`.

Provides:

- `PageReader`: reassembles complete packets from the Ogg page sequence of
  one logical stream, carrying the granule position and BOS/EOS flags of
  the page each packet completed on; supports seek (byte-position reset)
  and chained-link detection.
- `page_crc`: the Ogg CRC-32 (poly 0x04c11db7, MSB-first), exposed for
  test-side page muxing.

Header semantics (`OpusHead`, Vorbis identification/comment headers) are
codec-specific and live in the codec crates.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
