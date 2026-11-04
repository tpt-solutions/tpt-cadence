# tpt-cadence fuzz targets

`cargo-fuzz` (libFuzzer) coverage over every parser-facing entry point.
All decoder `decode()` implementations are panic-free by contract, so a
crash (or sanitizer hit) in any target is a real bug.

## Running

cargo-fuzz needs a nightly Rust toolchain and a platform libFuzzer
supports (Linux/macOS; not Windows/MSVC):

```sh
cargo install cargo-fuzz
cd fuzz
cargo fuzz list
cargo fuzz run opus_packet        # example
```

The workspace root excludes this directory, so the fuzz crate keeps its
own dependency resolution.

## Targets

| target | surface |
|---|---|
| `opus_packet` | Opus packet framing (`parse_packet`) |
| `opus_decode` | full `OpusDecoder::decode_packet` (SILK/CELT/hybrid) |
| `opus_ogg_decode` | Ogg Opus reader (`OggOpusReader`) end to end |
| `vorbis_stream` | Ogg Vorbis container + decode |
| `ogg_pages` | the shared Ogg page/packet layer |
| `mp3_decode` | MP3 probe + decode |
| `flac_decode` | FLAC frame decode |
| `aiff_decode` | AIFF chunk parser + decode |
| `wav_decode` | RIFF/WAVE parser + decode |
| `aac_adts` | AAC-LC ADTS probe + decode |

The target names are the `[[bin]]` names in `fuzz/Cargo.toml`, so
`cargo fuzz run <name>` takes exactly these.
