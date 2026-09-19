# Vorbis conformance fixtures

Bundled Ogg Vorbis fixtures (`*.ogg`, encoded with libvorbis through
FFmpeg) and their FFmpeg-decoded references (`*.ref`, interleaved
`f32le`), consumed by `tests/conformance.rs`.

| fixture | content | rate | channels | quality |
|---|---|---|---|---|
| `mono_32000_q2` | 880 + 1760 Hz sines | 32 kHz | mono | q2 |
| `mono_44100_q4` | 440 + 2200 Hz sines, tremolo | 44.1 kHz | mono | q4 |
| `stereo_44100_q4` | two sines panned | 44.1 kHz | stereo | q4 |
| `stereo_44100_qm1` | sine sweeps | 44.1 kHz | stereo | q−1 |
| `stereo_48000_q0` | pink noise | 48 kHz | stereo | q0 |
| `transients_44100_q3` | impulse train (forces long/short block switching) | 44.1 kHz | stereo | q3 |
| `quad_44100_q4` | four sines | 44.1 kHz | quad | q4 |
| `surround51_44100_q4` | six sines | 44.1 kHz | 5.1 | q4 |
| `surround71_44100_q4` | eight sines | 44.1 kHz | 7.1 | q4 |

## Regenerating

Requires `ffmpeg` (with libvorbis) and Python 3 on PATH. From this
directory:

```sh
ffmpeg -y -f lavfi -i "aevalsrc=0.6*sin(2*PI*440*t)+0.2*sin(2*PI*2200*t):s=44100:d=2.5" \
    -af tremolo=f=4:d=0.4 -ac 1 -c:a libvorbis -q:a 4 mono_44100_q4.ogg
ffmpeg -y -f lavfi -i "aevalsrc=0.5*sin(2*PI*330*t)|0.4*sin(2*PI*587*t)+0.1*sin(2*PI*1174*t):s=44100:d=2.5" \
    -ac 2 -c:a libvorbis -q:a 4 stereo_44100_q4.ogg
ffmpeg -y -f lavfi -i "anoisesrc=d=2.5:c=pink:r=48000:a=0.5" \
    -ac 2 -c:a libvorbis -q:a 0 stereo_48000_q0.ogg
ffmpeg -y -f lavfi -i "aevalsrc=0.7*sin(2*PI*(200+300*sin(2*PI*0.5*t))*t)|0.5*sin(2*PI*(500+200*cos(2*PI*0.3*t))*t):s=44100:d=2.5" \
    -ac 2 -c:a libvorbis -q:a -1 stereo_44100_qm1.ogg
ffmpeg -y -f lavfi -i "aevalsrc=0.5*sin(2*PI*880*t)+0.3*sin(2*PI*1760*t):s=32000:d=2.5" \
    -ac 1 -c:a libvorbis -q:a 2 mono_32000_q2.ogg
# transients: render the impulse-train WAV with the Python snippet in the
# repository history, then:
ffmpeg -y -i transients.wav -c:a libvorbis -q:a 3 transients_44100_q3.ogg
ffmpeg -y -f lavfi -i "aevalsrc=0.5*sin(2*PI*300*t)|0.5*sin(2*PI*600*t)|0.5*sin(2*PI*1200*t)|0.5*sin(2*PI*2400*t):s=44100:d=2.5"     -c:a libvorbis -q:a 4 quad_44100_q4.ogg
ffmpeg -y -f lavfi -i "aevalsrc=0.5*sin(2*PI*200*t)|0.5*sin(2*PI*400*t)|0.5*sin(2*PI*800*t)|0.5*sin(2*PI*1600*t)|0.5*sin(2*PI*3200*t)|0.5*sin(2*PI*6400*t):s=44100:d=2.5"     -c:a libvorbis -q:a 4 -channel_layout 5.1 surround51_44100_q4.ogg
ffmpeg -y -f lavfi -i "aevalsrc=0.4*sin(2*PI*150*t)|0.4*sin(2*PI*300*t)|0.4*sin(2*PI*600*t)|0.4*sin(2*PI*1200*t)|0.4*sin(2*PI*2400*t)|0.4*sin(2*PI*4800*t)|0.4*sin(2*PI*7000*t)|0.4*sin(2*PI*9000*t):s=44100:d=2.5"     -c:a libvorbis -q:a 4 -channel_layout 7.1 surround71_44100_q4.ogg

for f in *.ogg; do
    ffmpeg -y -i "$f" -f f32le -acodec pcm_f32le "${f%.ogg}.ref"
done
```

The conformance gate is >100 dB SNR against the reference; measured
values are 136–138 dB.
