# tpt-av-cadence-aac

AAC-LC decoder (ISO/IEC 14496-3) for the `tpt-cadence` audio codec suite.
Pure Rust, zero external dependencies.

**Status:** ✅ Functionally complete for AAC-LC, with HE-AAC (SBR) support.
Whole-stream PCM conformance against FFmpeg's decoder at >100 dB SNR and
<=1e-5 peak error on bundled mono/stereo fixtures, on live FFmpeg round
trips at 8-48 kHz in mono and stereo (123.6-125.5 dB measured), and on
raw-framed streams (bit-identical to the ADTS path). HE-AAC/SBR reaches
roughly 117-126 dB SNR on self-generated stereo fixtures and roughly
117-133 dB per channel on a self-generated 5.1 fixture after the QMF-window
and per-channel-element SBR-state fixes. Four official CCE/PCE coupling
items still decode at reduced fidelity. See `tests/conformance.rs`.

## Supported

- **Framing:** ADTS (auto-detected) and raw data blocks behind an explicit
  `AudioSpecificConfig` (e.g. an MP4 `esds` DecoderSpecificInfo, including
  PCE-in-ASC for channel configuration 0).
- **Channels:** mono, stereo, channel configurations 3-7 (multichannel, up
  to 7.1), and channel configuration 0 via Program Config Elements — both
  in-band and carried in the ASC. Fixed-configuration output follows the
  WAV channel order; PCE-configured output follows the reference's
  sniffed channel order (see the channel-order note below).
- **Real-world conformance:** the official ISO/IEC conformance items
  mirrored in FFmpeg's FATE suite decode through the MP4/esds entry point
  (opt-in test, `AAC_FATE_SAMPLES_DIR`); four items pass at >120 dB SNR
  and the multichannel coupling items at documented partial fidelity.
- **Tools:** the full LC tool set — Huffman spectral coding with escapes,
  scalefactor deltas, Temporal Noise Shaping, Perceptual Noise Substitution,
  M/S and intensity stereo, all four window sequences with sine and KBD
  shapes, and the pulse tool.
- **SBR (HE-AAC):** implicit and explicit (ASC `extensionSamplingFrequency`)
  signaling, full enhancement-band synthesis — QMF analysis/synthesis
  filterbanks, patch construction, envelope/noise mapping with the limiter,
  chirp-controlled inverse filtering, and sinusoid addition. Detecting an
  SBR payload doubles the output rate. See "Not supported" for the fidelity
  caveat and the PS/multichannel limitations.
- **Seek:** decode-and-discard, sample-frame accurate at transform (1024
  sample) granularity. `seek()` may block and allocate; it is not real-time
  safe (per the core `Decoder` contract).
- **Real-time decode:** all allocation, locking, and environment access is
  confined to `open()`; `decode()` writes into a caller buffer and is
  allocation-free, lock-free, and panic-free.

## Not supported

- **Parametric stereo (HE-AACv2):** a mono-core PS stream decodes as mono
  (the SBR payload's PS extension data is skipped). Four official
  CCE/PCE coupling conformance items still decode at reduced fidelity while
  their interaction with program-config state is investigated.
- **960/480-sample frames** (GASpecificConfig `frameLengthFlag = 1`) and
  gain control — not part of AAC-LC.
- **LTP data** is parsed for bitstream alignment (LC streams may carry it)
  but not applied, matching the reference decoder's LC behavior.

## Channel-order note

For channel configurations 1-7 (mono through 7.1) the output channel order
matches the WAV convention exactly — the bitstream carries front-center
first, which the decoder reorders (mappings verified per configuration
against FFmpeg with distinct per-channel content). For PCE-configured
(configuration 0) streams the channels are emitted in the PCE's bitstream
element order, which may differ from any specific host layout convention;
callers targeting such streams should remap or gate on explicit
channel-layout support in the core `StreamInfo`.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([../LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([../LICENSE-MIT](../LICENSE-MIT))

at your option.
