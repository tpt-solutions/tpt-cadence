# tpt-av-cadence-cli

A small unified command-line tool over every decoder in the `tpt-cadence`
suite. Format is auto-detected from the file extension (Ogg is sniffed to
tell Vorbis and Opus apart).

```sh
# Print stream metadata.
cargo run -p tpt-av-cadence-cli --bin cadence -- info song.mp3

# Decode to a 16-bit PCM WAV file.
cargo run -p tpt-av-cadence-cli --bin cadence -- decode song.mp3 -o song.wav

# Decode to raw interleaved f32 on stdout.
cargo run -p tpt-av-cadence-cli --bin cadence -- decode song.mp3 > song.f32
```

Supported extensions: `.wav`/`.wave`, `.aif`/`.aiff`/`.aifc`, `.flac`,
`.mp3`, `.aac`/`.adts`, `.opus`, `.ogg`/`.oga` (Vorbis or Opus, sniffed).

Headerless raw PCM isn't auto-detectable — a raw stream carries no format
metadata — so it isn't covered by this tool; see
`tpt-av-cadence-pcm/examples/pcm_decode.rs`, which takes the sample format,
channel count, and sample rate on the command line instead.

`decode` writes a plain 16-bit PCM WAV (hand-rolled in `main.rs`): the suite
doesn't ship any encoders yet, so this is the simplest way to get a
universally-playable file out of any supported input.
