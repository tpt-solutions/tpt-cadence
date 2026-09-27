//! Generated-material FFmpeg oracle matrix for the MP3 decoder.
//!
//! The official ISO/IEC 11172-4 conformance bitstreams remain unobtainable
//! (re-verified 2026-09-27: mpg123's SVN `test/` directory is the only known
//! home, its HTTP DAV interface is disabled, no mirror ships the streams, and
//! nothing new is publicly indexed), so MP3 correctness rests on comparison
//! against FFmpeg — an independent, spec-complete Layer III implementation.
//!
//! This suite turns that oracle into a *systematic* substitute for the
//! official conformance set: at test time it synthesizes small WAV inputs,
//! encodes them through FFmpeg's libmp3lame across MPEG-1/2/2.5 sample
//! rates, channel modes, the bitrate ladder extremes (32–320 kbps MPEG-1,
//! 8–160 kbps MPEG-2, 8–64 kbps MPEG-2.5), forced joint stereo and reservoir
//! settings, plus three header-surgery variants (dual-channel mode,
//! copyright/original/emphasis flag flips, Xing-frame strip), and decodes
//! every stream through this crate and through FFmpeg. Each stream must
//! byte-tile exactly per the ISO frame-size formula, decode to the same
//! length as the oracle with finite output, and match it at the suite gate
//! (>100 dB SNR, <=1e-5 peak error).
//!
//! A second test parses every generated stream's headers and side info with
//! a small independent reader and asserts the corpus actually *exercises*
//! the Layer III features those SNR numbers implicitly claim to cover
//! (short/mixed/start/stop blocks, scfsi scalefactor sharing, bit-reservoir
//! use, preflag, scalefac_scale, subblock gains, both count1 tables,
//! count1-only and zero-length granules, mid/side mode extension). LAME
//! never emits Layer III intensity stereo or CRC protection, so those paths
//! are not covered by this corpus (CRC is covered separately by
//! `crc_streams.rs` against rewritten real encoder output); the coverage
//! test's stderr report documents the gap.
//!
//! Skips gracefully when FFmpeg (with libmp3lame) is not on PATH. Set
//! CADENCE_REQUIRE_FFMPEG=1 to fail instead.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use tpt_av_cadence_core::Decoder as _;
use tpt_av_cadence_mp3::Mp3Decoder;
use tpt_av_cadence_test_utils::reference::{decode_with_ffmpeg, ffmpeg_available};

const SECONDS: f32 = 1.2;

// ---------------------------------------------------------------- content

#[derive(Clone, Copy)]
enum Content {
    /// Pure sine at the given frequency.
    Tone(f32),
    /// Independent white noise (xorshift).
    Noise(u32),
    /// Decaying impulse trains, which force LAME into short/mixed blocks.
    Clicks(u32),
}

fn sample_at(content: Content, i: usize, channel: u16, sample_rate: u32, state: &mut u32) -> f32 {
    match content {
        Content::Tone(freq) => {
            let t = i as f32 / sample_rate as f32;
            (2.0 * std::f32::consts::PI * freq * t).sin() * 0.6
        }
        Content::Noise(_) => {
            *state ^= *state << 13;
            *state ^= *state >> 17;
            *state ^= *state << 5;
            (((*state >> 8) as f32 / 0x7F_FFFF as f32) * 2.0 - 1.0) * 0.8
        }
        Content::Clicks(_) => {
            let stagger = channel as usize * sample_rate as usize / 10;
            let period = sample_rate as usize / 5;
            let phase = (i + stagger) % period;
            let decay = (-(phase as f32) / (sample_rate as f32 * 0.002)).exp();
            *state ^= *state << 13;
            *state ^= *state >> 17;
            *state ^= *state << 5;
            let noise = (*state >> 12) as f32 / 0xF_FFFF as f32 * 2.0 - 1.0;
            decay * (phase as f32 * 0.7).sin() * 0.9 + noise * 0.02
        }
    }
}

fn render(content: Content, sample_rate: u32, channels: u16) -> Vec<f32> {
    let n = (sample_rate as f32 * SECONDS) as usize;
    let mut state = match content {
        Content::Noise(seed) | Content::Clicks(seed) => seed,
        Content::Tone(_) => 0xDEAD_BEEF,
    };
    let mut out = Vec::with_capacity(n * channels as usize);
    for i in 0..n {
        for channel in 0..channels {
            out.push(sample_at(content, i, channel, sample_rate, &mut state));
        }
    }
    out
}

fn write_wav(path: &Path, sample_rate: u32, channels: u16, interleaved: &[f32]) {
    let mut pcm = Vec::with_capacity(interleaved.len() * 2);
    for &s in interleaved {
        let clamped = s.clamp(-1.0, 0.99);
        pcm.extend_from_slice(&((clamped * 32767.0) as i16).to_le_bytes());
    }
    let mut file = std::fs::File::create(path).expect("create wav");
    let byte_len = pcm.len() as u32;
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + byte_len).to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap();
    file.write_all(&channels.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&(sample_rate * channels as u32 * 2).to_le_bytes())
        .unwrap();
    file.write_all(&(channels * 2).to_le_bytes()).unwrap();
    file.write_all(&16u16.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&byte_len.to_le_bytes()).unwrap();
    file.write_all(&pcm).unwrap();
}

// ------------------------------------------------------------ matrix spec

struct Case {
    label: &'static str,
    content: Content,
    sample_rate: u32,
    channels: u16,
    bitrate_kbps: u32,
    joint_stereo: Option<bool>,
    use_reservoir: Option<bool>,
}

/// Coverage targets: the MPEG-1 44.1 kHz bitrate ladder (tone), stereo with
/// joint stereo forced on and off, the highest MPEG-1 bitrate, transient
/// content in all three channel modes, reservoir disabled, the other MPEG-1
/// sample rates, and MPEG-2 / MPEG-2.5 from the 8 kbps floor upward.
const CASES: &[Case] = &[
    Case {
        label: "m1_441_mono_32k",
        content: Content::Tone(1000.0),
        sample_rate: 44_100,
        channels: 1,
        bitrate_kbps: 32,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_441_mono_64k",
        content: Content::Tone(1000.0),
        sample_rate: 44_100,
        channels: 1,
        bitrate_kbps: 64,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_441_mono_96k",
        content: Content::Tone(1000.0),
        sample_rate: 44_100,
        channels: 1,
        bitrate_kbps: 96,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_441_mono_128k",
        content: Content::Tone(1000.0),
        sample_rate: 44_100,
        channels: 1,
        bitrate_kbps: 128,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_441_mono_192k",
        content: Content::Tone(1000.0),
        sample_rate: 44_100,
        channels: 1,
        bitrate_kbps: 192,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_441_mono_320k",
        content: Content::Tone(1000.0),
        sample_rate: 44_100,
        channels: 1,
        bitrate_kbps: 320,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_441_stereo_128k",
        content: Content::Noise(0xC0FF_EE01),
        sample_rate: 44_100,
        channels: 2,
        bitrate_kbps: 128,
        joint_stereo: Some(false),
        use_reservoir: None,
    },
    Case {
        label: "m1_441_joint_128k",
        content: Content::Noise(0xC0FF_EE01),
        sample_rate: 44_100,
        channels: 2,
        bitrate_kbps: 128,
        joint_stereo: Some(true),
        use_reservoir: None,
    },
    Case {
        label: "m1_441_joint_320k",
        content: Content::Noise(0xC0FF_EE01),
        sample_rate: 44_100,
        channels: 2,
        bitrate_kbps: 320,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_441_joint_clicks_128k",
        content: Content::Clicks(0x5EED_0001),
        sample_rate: 44_100,
        channels: 2,
        bitrate_kbps: 128,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_441_joint_clicks_64k",
        content: Content::Clicks(0x5EED_0002),
        sample_rate: 44_100,
        channels: 2,
        bitrate_kbps: 64,
        joint_stereo: Some(true),
        use_reservoir: None,
    },
    Case {
        label: "m1_441_mono_clicks_128k",
        content: Content::Clicks(0x5EED_0003),
        sample_rate: 44_100,
        channels: 1,
        bitrate_kbps: 128,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_441_mono_128k_nores",
        content: Content::Tone(1000.0),
        sample_rate: 44_100,
        channels: 1,
        bitrate_kbps: 128,
        joint_stereo: None,
        use_reservoir: Some(false),
    },
    Case {
        label: "m1_320_mono_128k",
        content: Content::Tone(1000.0),
        sample_rate: 32_000,
        channels: 1,
        bitrate_kbps: 128,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_480_mono_320k",
        content: Content::Tone(1000.0),
        sample_rate: 48_000,
        channels: 1,
        bitrate_kbps: 320,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m1_480_joint_64k",
        content: Content::Noise(0x1234_5678),
        sample_rate: 48_000,
        channels: 2,
        bitrate_kbps: 64,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m2_16k_mono_8k",
        content: Content::Tone(500.0),
        sample_rate: 16_000,
        channels: 1,
        bitrate_kbps: 8,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m2_16k_mono_160k",
        content: Content::Tone(500.0),
        sample_rate: 16_000,
        channels: 1,
        bitrate_kbps: 160,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m2_22050_joint_96k",
        content: Content::Tone(400.0),
        sample_rate: 22_050,
        channels: 2,
        bitrate_kbps: 96,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m2_24k_joint_32k",
        content: Content::Tone(300.0),
        sample_rate: 24_000,
        channels: 2,
        bitrate_kbps: 32,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m25_11k_mono_8k",
        content: Content::Tone(220.0),
        sample_rate: 11_025,
        channels: 1,
        bitrate_kbps: 8,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m25_12k_joint_32k",
        content: Content::Tone(250.0),
        sample_rate: 12_000,
        channels: 2,
        bitrate_kbps: 32,
        joint_stereo: None,
        use_reservoir: None,
    },
    Case {
        label: "m25_8k_mono_64k",
        content: Content::Tone(150.0),
        sample_rate: 8_000,
        channels: 1,
        bitrate_kbps: 64,
        joint_stereo: None,
        use_reservoir: None,
    },
];

#[derive(Clone)]
struct Generated {
    label: String,
    path: PathBuf,
    sample_rate: u32,
    channels: u16,
}

fn libmp3lame_available() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-h", "encoder=libmp3lame"])
        .output()
        .map(|out| {
            out.status.success()
                && (out.stdout.starts_with(b"Encoder libmp3lame")
                    || out.stderr.starts_with(b"Encoder libmp3lame"))
        })
        .unwrap_or(false)
}

/// Returns true when both oracle halves (FFmpeg and its libmp3lame encoder)
/// are usable; otherwise skips the calling test unless
/// CADENCE_REQUIRE_FFMPEG demands a hard failure.
fn require_oracles() -> bool {
    if ffmpeg_available() && libmp3lame_available() {
        return true;
    }
    assert!(
        std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_none(),
        "FFmpeg with libmp3lame is required but unavailable on PATH"
    );
    eprintln!("skipping: FFmpeg with libmp3lame not on PATH");
    false
}

fn ffmpeg_encode(case: &Case, wav: &Path, mp3: &Path) {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(wav)
        .args([
            "-c:a",
            "libmp3lame",
            "-b:a",
            &format!("{}k", case.bitrate_kbps),
        ]);
    if let Some(js) = case.joint_stereo {
        cmd.args(["-joint_stereo", if js { "1" } else { "0" }]);
    }
    if let Some(res) = case.use_reservoir {
        cmd.args(["-reservoir", if res { "1" } else { "0" }]);
    }
    cmd.arg(mp3);
    let out = cmd.output().expect("spawn ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg failed to encode {}: {}",
        case.label,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Bit-level header surgery covering stream shapes LAME cannot produce:
/// dual-channel mode (LAME only emits mono/stereo/joint; stereo only —
/// flipping mono to dual would change the side-info size and corrupt the
/// stream), flipped copyright/original/emphasis flags (decoders must ignore
/// them), and a stripped Xing/Info frame (plain headerless CBR).
fn surgery_variants(dir: &Path, base: &Generated, allow_dual: bool) -> Vec<Generated> {
    let data = std::fs::read(&base.path).expect("read base stream");
    let starts = frame_starts(&data);
    assert!(!starts.is_empty(), "surgery base failed to tile");

    let mut variants: Vec<(&str, Vec<u8>)> = Vec::new();
    if allow_dual {
        let mut dual = data.clone();
        for &s in &starts {
            dual[s + 3] = (dual[s + 3] & 0b0011_1111) | (0b10 << 6);
        }
        variants.push(("dualmode", dual));
    }
    let mut flags = data.clone();
    for &s in &starts {
        flags[s + 3] = (flags[s + 3] | 0b0000_1000) & !0b0000_0100 | 0b0000_0001;
    }
    variants.push(("flags", flags));
    // Drop the leading Xing/Info frame itself (starts[0]), keeping the ID3
    // stripped too, so the stream is plain headerless CBR.
    assert!(starts.len() >= 2, "surgery base too short to strip Xing");
    variants.push(("noxing", data[starts[1]..].to_vec()));

    let mut out = Vec::new();
    for (suffix, bytes) in variants {
        let label = format!("{}_{}", base.label, suffix);
        let path = dir.join(format!("{label}.mp3"));
        std::fs::write(&path, &bytes).expect("write surgery stream");
        out.push(Generated {
            label,
            path,
            sample_rate: base.sample_rate,
            channels: base.channels,
        });
    }
    out
}

fn generate_matrix(dir: &Path) -> Vec<Generated> {
    std::fs::create_dir_all(dir).expect("create matrix dir");
    let mut out = Vec::new();
    for case in CASES {
        let wav = dir.join(format!("{}_src.wav", case.label));
        let mp3 = dir.join(format!("{}.mp3", case.label));
        let frames = render(case.content, case.sample_rate, case.channels);
        write_wav(&wav, case.sample_rate, case.channels, &frames);
        ffmpeg_encode(case, &wav, &mp3);
        let _ = std::fs::remove_file(&wav);
        out.push(Generated {
            label: case.label.to_string(),
            path: mp3,
            sample_rate: case.sample_rate,
            channels: case.channels,
        });
    }
    // Surgery needs one plain stereo stream and one mono stream.
    let stereo_base = out
        .iter()
        .find(|g| g.label == "m1_441_stereo_128k")
        .expect("stereo surgery base")
        .clone();
    let mono_base = out
        .iter()
        .find(|g| g.label == "m1_441_mono_96k")
        .expect("mono surgery base")
        .clone();
    out.extend(surgery_variants(dir, &stereo_base, true));
    out.extend(surgery_variants(dir, &mono_base, false));
    out
}

// -------------------------------------------------------------- decoding

/// LAME gapless info parsed from a first-frame `Info` tag: (encoder delay,
/// encoder padding, samples per frame). FFmpeg's mp3 demuxer consumes this
/// tag and yields exactly the original source samples: it discards the tag
/// frame's own audio plus the `delay` leading samples and the `padding`
/// trailing samples. Our decoder deliberately emits the raw stream, so the
/// comparison must apply the same alignment.
fn lame_gapless(data: &[u8]) -> Option<(usize, usize, usize)> {
    let start = id3_len(data);
    let frame = frame_info(data, start)?;
    let tag_at = start + 4 + usize::from(frame.crc) * 2 + frame.side_info_bytes;
    if tag_at + 4 + 4 > data.len() {
        return None;
    }
    if &data[tag_at..tag_at + 4] != b"Info" && &data[tag_at..tag_at + 4] != b"Xing" {
        return None;
    }
    let flags = u32::from_be_bytes([
        data[tag_at + 4],
        data[tag_at + 5],
        data[tag_at + 6],
        data[tag_at + 7],
    ]);
    // The LAME delay/padding field's offset assumes the full tag layout
    // (flags, frames, bytes, 100-byte seek toc, vbr scale).
    if flags & 0xF != 0xF {
        return None;
    }
    // Offset of the 24-bit delay/padding field, verified empirically against
    // FFmpeg's muxer output (info string pos + 141): 'Info' (4) + flags (4)
    // + frames (4) + bytes (4) + toc (100) + vbr scale (4), then 21 bytes
    // (encoder version string + revision area) into the LAME tag.
    let field = tag_at + 4 + 4 + 4 + 4 + 100 + 4 + 21;
    if field + 3 > data.len() {
        return None;
    }
    let v = (u32::from(data[field]) << 16)
        | (u32::from(data[field + 1]) << 8)
        | u32::from(data[field + 2]);
    let delay = (v >> 12) as usize;
    let padding = (v & 0xFFF) as usize;
    let samples_per_frame = if frame.version == 3 { 1152 } else { 576 };
    Some((delay, padding, samples_per_frame))
}

fn decode_ours(label: &str, path: &Path) -> Result<Vec<f32>, String> {
    let mut dec = Mp3Decoder::open(Box::new(std::fs::File::open(path).unwrap()))
        .map_err(|e| format!("{label}: open failed: {e}"))?;
    let channels = dec.info().channels as usize;
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 1152 * channels];
    loop {
        let n = dec.decode(&mut buf).map_err(|e| format!("{label}: {e}"))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n * channels]);
    }
    Ok(out)
}

/// Our decode vs the FFmpeg oracle. Streams carrying a LAME gapless tag are
/// aligned by skipping the tag frame plus the encoder delay and dropping the
/// encoder padding, so both sides start at source time zero; tag-less
/// streams compare raw. Returns the measured numbers for the caller's
/// report.
fn compare_with_oracle(gen: &Generated) -> Result<(f64, f64), String> {
    let ours = decode_ours(&gen.label, &gen.path)?;
    let oracle = decode_with_ffmpeg(&gen.path, gen.sample_rate, gen.channels)
        .map_err(|e| format!("{}: ffmpeg oracle decode failed: {e}", gen.label))?;
    if ours.is_empty() {
        return Err(format!("{}: our decoder produced no samples", gen.label));
    }
    let raw = std::fs::read(&gen.path).map_err(|e| format!("{}: {e}", gen.label))?;
    let channels = gen.channels as usize;
    let aligned: &[f32] = match lame_gapless(&raw) {
        Some((delay, padding, samples_per_frame)) => {
            // FFmpeg's mp3 demuxer discards the tag frame itself, then skips
            // delay + 529 leading samples (529 = the LAME/decoder synthesis
            // delay FFmpeg compensates for) and drops padding - 529 trailing
            // samples — net per channel: spf + delay + padding. Empirically
            // pinned on the muxer output (front 2257 = 1152+576+529, net
            // 2376 = 1152+576+648) and verified per stream by the identity
            // below.
            let front = channels * (samples_per_frame + delay + 529);
            if ours.len() != oracle.len() + channels * (samples_per_frame + delay + padding) {
                return Err(format!(
                    "{}: gapless arithmetic mismatch: ours={} oracle={} delay={delay} pad={padding} spf={samples_per_frame}",
                    gen.label,
                    ours.len(),
                    oracle.len()
                ));
            }
            if ours.len() < front + oracle.len() {
                return Err(format!(
                    "{}: gapless front trim overruns the raw decode",
                    gen.label
                ));
            }
            &ours[front..front + oracle.len()]
        }
        None => &ours[..],
    };
    if aligned.len() != oracle.len() {
        return Err(format!(
            "{}: length vs oracle: ours={} oracle={}",
            gen.label,
            aligned.len(),
            oracle.len()
        ));
    }
    if !aligned.iter().chain(&oracle).all(|s| s.is_finite()) {
        return Err(format!("{}: non-finite output", gen.label));
    }
    let mut signal = 0.0f64;
    let mut error = 0.0f64;
    let mut peak = 0.0f64;
    for (&actual, &expected) in aligned.iter().zip(&oracle) {
        let delta = f64::from(actual) - f64::from(expected);
        error += delta * delta;
        signal += f64::from(expected).powi(2);
        peak = peak.max(delta.abs());
    }
    if signal <= 0.0 {
        return Err(format!("{}: silent oracle output", gen.label));
    }
    let snr = 10.0 * (signal / error).log10();
    eprintln!("{}: SNR={snr:.2} dB, max error={peak:.3e}", gen.label);
    if snr <= 100.0 || peak > 1e-5 {
        return Err(format!(
            "{}: SNR={snr:.2} dB, peak={peak:.3e} misses the >100 dB / <=1e-5 gate",
            gen.label
        ));
    }
    Ok((snr, peak))
}

// ------------------------------------------------- independent bit reader

struct MsbReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> MsbReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        MsbReader { data, bit_pos: 0 }
    }

    fn get_bits(&mut self, n: u32) -> u32 {
        let mut value = 0u32;
        for _ in 0..n {
            let byte = self.data[self.bit_pos >> 3];
            value = (value << 1) | u32::from((byte >> (7 - (self.bit_pos & 7))) & 1);
            self.bit_pos += 1;
        }
        value
    }
}

// ------------------------------------------------------- feature scanner

const BITRATES_MPEG1: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
const BITRATES_LSF: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];

struct FrameInfo {
    len: usize,
    version: u32,
    mode: u32,
    mode_ext: u32,
    crc: bool,
    side_info_bytes: usize,
}

/// ISO/IEC 11172-3 / 13818-3 Layer III frame geometry from the 4-byte
/// header. `None` if the header is not a valid Layer III frame start.
fn frame_info(data: &[u8], i: usize) -> Option<FrameInfo> {
    if data[i] != 0xFF || (data[i + 1] & 0xE0) != 0xE0 {
        return None;
    }
    let b1 = data[i + 1];
    let b2 = data[i + 2];
    let b3 = data[i + 3];
    let version = u32::from((b1 >> 3) & 0b11); // 3 = MPEG-1, 2 = MPEG-2, 0 = 2.5
    let layer = u32::from((b1 >> 1) & 0b11);
    let crc = b1 & 1 == 0;
    let bitrate_idx = usize::from((b2 >> 4) & 0x0F);
    let sr_idx = usize::from((b2 >> 2) & 0b11);
    let padding = u32::from((b2 >> 1) & 1);
    let mode = u32::from((b3 >> 6) & 0b11);
    let mode_ext = u32::from((b3 >> 4) & 0b11);
    if layer != 1 || version == 1 || sr_idx == 3 {
        return None;
    }
    let (bitrate, sample_rate, header_multiplier) = match version {
        3 => (
            BITRATES_MPEG1[bitrate_idx],
            [44_100u32, 48_000, 32_000][sr_idx],
            144,
        ),
        2 => (
            BITRATES_LSF[bitrate_idx],
            [22_050u32, 24_000, 16_000][sr_idx],
            72,
        ),
        _ => (
            BITRATES_LSF[bitrate_idx],
            [11_025u32, 12_000, 8_000][sr_idx],
            72,
        ),
    };
    if bitrate == 0 {
        return None;
    }
    let side_info_bytes = match (version, mode) {
        (3, 3) => 17,
        (3, _) => 32,
        (_, 3) => 9,
        (_, _) => 17,
    };
    let len = header_multiplier * bitrate * 1000 / sample_rate + padding;
    Some(FrameInfo {
        len: len as usize,
        version,
        mode,
        mode_ext,
        crc,
        side_info_bytes,
    })
}

fn id3_len(data: &[u8]) -> usize {
    if data.len() >= 10 && &data[..3] == b"ID3" {
        let size = (u32::from(data[6] & 0x7F) << 21)
            | (u32::from(data[7] & 0x7F) << 14)
            | (u32::from(data[8] & 0x7F) << 7)
            | u32::from(data[9] & 0x7F);
        10 + size as usize
    } else {
        0
    }
}

#[derive(Default, Debug)]
struct Tally {
    streams: u32,
    frames: u32,
    version1_frames: u32,
    version2_frames: u32,
    version25_frames: u32,
    mono_frames: u32,
    stereo_frames: u32,
    joint_frames: u32,
    mode_ext_0_frames: u32,
    mode_ext_2_frames: u32,
    reservoir_frames: u32,
    scfsi_frames: u32,
    long_granules: u32,
    start_granules: u32,
    short_granules: u32,
    mixed_granules: u32,
    stop_granules: u32,
    subblock_gain_granules: u32,
    preflag_granules: u32,
    scalefac_scale_granules: u32,
    count1_table_0_granules: u32,
    count1_table_1_granules: u32,
    count1_only_granules: u32,
    zero_length_granules: u32,
}

fn tally_side_info(tally: &mut Tally, data: &[u8], offset: usize, frame: &FrameInfo) {
    let end = offset + frame.side_info_bytes;
    if end > data.len() {
        return;
    }
    let reader = &mut MsbReader::new(&data[offset..end]);
    let channels = if frame.mode == 3 { 1 } else { 2 };
    let mpeg1 = frame.version == 3;
    // MPEG-1: 9-bit main_data_begin, 0/3 private bits, then 4 scfsi bits per
    // channel. LSF (MPEG-2/2.5): 8-bit main_data_begin with 1/2 private
    // bits, and NO scfsi (single granule). The LSF preflag is also not
    // transmitted (derived from scalefac_compress >= 500).
    let mdb = if mpeg1 {
        let _mdb = reader.get_bits(9);
        reader.get_bits(if frame.mode == 3 { 0 } else { 3 });
        true
    } else {
        let raw = reader.get_bits(8 + if frame.mode == 3 { 1 } else { 2 });
        raw != 0
    };
    if mdb {
        tally.reservoir_frames += 1;
    }
    if mpeg1 {
        let mut any_scfsi = false;
        for _ in 0..channels {
            if reader.get_bits(4) != 0 {
                any_scfsi = true;
            }
        }
        if any_scfsi {
            tally.scfsi_frames += 1;
        }
    }
    let granules = if frame.version == 3 { 2 } else { 1 };
    for _ in 0..granules {
        for _ in 0..channels {
            let part2_3_length = reader.get_bits(12);
            let big_values = reader.get_bits(9);
            reader.get_bits(8); // global_gain
            reader.get_bits(if mpeg1 { 4 } else { 9 }); // scalefac_compress
            if reader.get_bits(1) == 1 {
                let block_type = reader.get_bits(2);
                let mixed = reader.get_bits(1);
                reader.get_bits(5); // table_select[0]
                reader.get_bits(5); // table_select[1]
                match block_type {
                    0 => tally.long_granules += 1,
                    1 => tally.start_granules += 1,
                    2 => {
                        tally.short_granules += 1;
                        if mixed == 1 {
                            tally.mixed_granules += 1;
                        }
                    }
                    _ => tally.stop_granules += 1,
                }
                let mut any_subblock_gain = false;
                for _ in 0..3 {
                    if reader.get_bits(3) != 0 {
                        any_subblock_gain = true;
                    }
                }
                if any_subblock_gain {
                    tally.subblock_gain_granules += 1;
                }
            } else {
                reader.get_bits(5); // table_select[0]
                reader.get_bits(5); // table_select[1]
                reader.get_bits(5); // table_select[2]
                reader.get_bits(4); // region0_count
                reader.get_bits(3); // region1_count
                tally.long_granules += 1;
            }
            if mpeg1 && reader.get_bits(1) != 0 {
                tally.preflag_granules += 1;
            }
            if reader.get_bits(1) != 0 {
                tally.scalefac_scale_granules += 1;
            }
            if reader.get_bits(1) == 0 {
                tally.count1_table_0_granules += 1;
            } else {
                tally.count1_table_1_granules += 1;
            }
            if part2_3_length == 0 {
                tally.zero_length_granules += 1;
            } else if big_values == 0 {
                tally.count1_only_granules += 1;
            }
        }
    }
}

/// Walks the stream frame by frame with the ISO size formula; returns false
/// unless every byte from the first frame to EOF is covered by exact frames.
fn scan_stream(tally: &mut Tally, data: &[u8]) -> bool {
    tally.streams += 1;
    let mut i = id3_len(data);
    while i < data.len() {
        if i + 4 > data.len() {
            return false;
        }
        let Some(frame) = frame_info(data, i) else {
            return false;
        };
        if i + frame.len > data.len() {
            return false;
        }
        tally.frames += 1;
        match frame.version {
            3 => tally.version1_frames += 1,
            2 => tally.version2_frames += 1,
            _ => tally.version25_frames += 1,
        }
        match frame.mode {
            0 => tally.stereo_frames += 1,
            1 => {
                tally.joint_frames += 1;
                if frame.mode_ext == 0 {
                    tally.mode_ext_0_frames += 1;
                } else if frame.mode_ext == 2 {
                    tally.mode_ext_2_frames += 1;
                }
            }
            3 => tally.mono_frames += 1,
            _ => {}
        }
        let side_info_offset = i + 4 + usize::from(frame.crc) * 2;
        tally_side_info(tally, data, side_info_offset, &frame);
        i += frame.len;
    }
    true
}

fn frame_starts(data: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut i = id3_len(data);
    while i + 4 <= data.len() {
        match frame_info(data, i) {
            Some(frame) if i + frame.len <= data.len() => {
                starts.push(i);
                i += frame.len;
            }
            _ => break,
        }
    }
    starts
}

/// Asserts the Layer III feature space the SNR gate implicitly relies on is
/// really present in the generated corpus. Thresholds sit far below the
/// measured counts so a LAME behavior change cannot silently hollow out the
/// oracle's coverage without failing here.
fn assert_coverage(tally: &Tally) {
    let check = |actual: u32, min: u32, what: &str| {
        assert!(
            actual >= min,
            "oracle corpus no longer exercises {what}: {actual} < {min}"
        );
    };
    check(tally.frames, 800, "frames total");
    check(tally.version1_frames, 250, "MPEG-1 frames");
    check(tally.version2_frames, 50, "MPEG-2 frames");
    check(tally.version25_frames, 25, "MPEG-2.5 frames");
    check(tally.mono_frames, 200, "mono frames");
    check(tally.stereo_frames, 40, "stereo frames");
    check(tally.joint_frames, 120, "joint stereo frames");
    check(tally.mode_ext_0_frames, 20, "full-band mid/side frames");
    check(tally.mode_ext_2_frames, 100, "lower-band mid/side frames");
    check(
        tally.reservoir_frames,
        200,
        "bit-reservoir (main_data_begin>0) frames",
    );
    check(tally.scfsi_frames, 100, "scfsi scalefactor-sharing frames");
    check(tally.long_granules, 500, "long-block granules");
    check(tally.start_granules, 30, "start-block granules");
    check(tally.short_granules, 50, "short-block granules");
    check(tally.mixed_granules, 20, "mixed-block granules");
    check(tally.stop_granules, 50, "stop-block granules");
    check(tally.subblock_gain_granules, 200, "subblock-gain granules");
    check(tally.preflag_granules, 150, "preflag granules");
    check(
        tally.scalefac_scale_granules,
        300,
        "scalefac_scale=1 granules",
    );
    check(
        tally.count1_table_0_granules,
        500,
        "count1 table 0 granules",
    );
    check(
        tally.count1_table_1_granules,
        150,
        "count1 table 1 granules",
    );
    check(tally.count1_only_granules, 3, "count1-only granules");
    check(tally.zero_length_granules, 50, "zero-length granules");
}

// ------------------------------------------------------------------ tests

#[test]
fn generated_oracle_matrix_matches_ffmpeg_at_conformance_fidelity() {
    if !require_oracles() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("cadence_mp3_matrix_snr_{}", std::process::id()));
    let streams = generate_matrix(&dir);
    let mut failures = Vec::new();
    for gen in &streams {
        if let Err(msg) = compare_with_oracle(gen) {
            failures.push(msg);
        }
    }
    let count = streams.len();
    drop(streams);
    let _ = std::fs::remove_dir_all(&dir);
    if !failures.is_empty() {
        panic!("oracle mismatches: {failures:#?}");
    }
    eprintln!("matrix: {count} generated streams compared against FFmpeg at the suite gate");
}

#[test]
fn generated_oracle_matrix_tiles_exactly_and_exercises_layer3_features() {
    if !require_oracles() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("cadence_mp3_matrix_cov_{}", std::process::id()));
    let streams = generate_matrix(&dir);

    let mut tally = Tally::default();
    let mut untiled = Vec::new();
    for gen in &streams {
        let data = std::fs::read(&gen.path).expect("read generated stream");
        if !scan_stream(&mut tally, &data) {
            untiled.push(gen.label.clone());
        }
    }
    // The bundled fixtures must byte-tile too; their features join the report
    // (not the assertions, whose thresholds are calibrated on the matrix).
    let mut fixture_tally = Tally::default();
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for entry in std::fs::read_dir(&fixture_dir).expect("fixture dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("mp3") {
            continue;
        }
        let data = std::fs::read(&path).expect("read fixture");
        if !scan_stream(&mut fixture_tally, &data) {
            untiled.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        untiled.is_empty(),
        "streams that do not byte-tile: {untiled:#?}"
    );
    assert_coverage(&tally);
    eprintln!(
        "coverage: matrix+fixtures = {} streams, {} frames",
        tally.streams + fixture_tally.streams,
        tally.frames + fixture_tally.frames
    );
    eprintln!("matrix tally: {tally:#?}");
    eprintln!("fixture tally: {fixture_tally:#?}");
}
