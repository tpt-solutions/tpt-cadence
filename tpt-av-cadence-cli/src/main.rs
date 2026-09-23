//! `cadence`: a small CLI over every `tpt-cadence` decoder.
//!
//! ```text
//! cadence info <file>              print format, sample rate, channels, etc.
//! cadence decode <file> [-o out.wav]   decode to a 16-bit PCM WAV file
//!                                       (omit -o to write raw interleaved
//!                                       f32 to stdout instead)
//! ```
//!
//! Format is auto-detected from the file extension; `.ogg`/`.opus`/`.oga`
//! are additionally sniffed (Ogg can carry either Vorbis or Opus) by
//! scanning the first page for the `OpusHead` or `vorbis` identification
//! marker. Headerless raw PCM isn't auto-detectable (no header carries the
//! sample format) — see `tpt-av-cadence-pcm`'s own `pcm_decode` example
//! instead, which takes the format on the command line.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use tpt_av_cadence_core::{CadenceError, FormatReader};

/// At least one decoder path (HE-AAC/SBR — see `todo.md`) uses enough stack
/// per `decode()` call to overflow a platform's default main-thread stack
/// (1 MiB on Windows). Every command therefore runs on a worker thread with
/// a generous stack instead of directly on `main`.
const WORKER_STACK_BYTES: usize = 32 * 1024 * 1024;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = std::thread::Builder::new()
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || run(&args))
        .expect("spawn worker thread")
        .join()
        .expect("worker thread panicked");
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("info") => {
            let path = args.get(1).ok_or_else(usage)?;
            cmd_info(path)
        }
        Some("decode") => {
            let path = args.get(1).ok_or_else(usage)?;
            let out = parse_output_flag(&args[2..]);
            cmd_decode(path, out.as_deref())
        }
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage:\n  cadence info <file>\n  cadence decode <file> [-o out.wav]".to_string()
}

fn parse_output_flag(rest: &[String]) -> Option<String> {
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        if arg == "-o" || arg == "--output" {
            return it.next().cloned();
        }
    }
    None
}

/// A detected input format, opened generically behind the shared
/// [`FormatReader`]/[`Decoder`] traits. Each variant is gated on the
/// matching Cargo feature (see `Cargo.toml`'s `[features]`, all on by
/// default) so a build with a format disabled doesn't even compile in the
/// dependency that would decode it.
enum Kind {
    #[cfg(feature = "wav")]
    Wav,
    #[cfg(feature = "aiff")]
    Aiff,
    #[cfg(feature = "flac")]
    Flac,
    #[cfg(feature = "mp3")]
    Mp3,
    #[cfg(feature = "vorbis")]
    Vorbis,
    #[cfg(feature = "opus")]
    Opus,
    #[cfg(feature = "aac")]
    Aac,
}

fn detect(path: &Path) -> Result<Kind, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    #[allow(unreachable_patterns)] // a disabled format's arm becomes unreachable, not an error
    match ext.as_str() {
        #[cfg(feature = "wav")]
        "wav" | "wave" => Ok(Kind::Wav),
        #[cfg(feature = "aiff")]
        "aif" | "aiff" | "aifc" => Ok(Kind::Aiff),
        #[cfg(feature = "flac")]
        "flac" => Ok(Kind::Flac),
        #[cfg(feature = "mp3")]
        "mp3" => Ok(Kind::Mp3),
        #[cfg(feature = "aac")]
        "aac" | "adts" => Ok(Kind::Aac),
        #[cfg(feature = "opus")]
        "opus" => Ok(Kind::Opus),
        #[cfg(any(feature = "opus", feature = "vorbis"))]
        "ogg" | "oga" => sniff_ogg(path),
        other => Err(format!(
            "can't auto-detect format from extension {other:?} (supported in this build: {})",
            supported_extensions()
        )),
    }
}

fn supported_extensions() -> String {
    let mut formats = Vec::new();
    if cfg!(feature = "wav") {
        formats.push("wav");
    }
    if cfg!(feature = "aiff") {
        formats.push("aif/aiff");
    }
    if cfg!(feature = "flac") {
        formats.push("flac");
    }
    if cfg!(feature = "mp3") {
        formats.push("mp3");
    }
    if cfg!(feature = "aac") {
        formats.push("aac");
    }
    if cfg!(feature = "opus") {
        formats.push("opus");
    }
    if cfg!(any(feature = "opus", feature = "vorbis")) {
        formats.push("ogg");
    }
    formats.join(", ")
}

/// Ogg can carry either Vorbis or Opus; peek the first page's identification
/// packet for the `OpusHead` or `\x01vorbis` marker to tell them apart.
/// Only compiled when at least one of the two is enabled (see `detect`'s
/// `ogg`/`oga` arm).
#[cfg(any(feature = "opus", feature = "vorbis"))]
fn sniff_ogg(path: &Path) -> Result<Kind, String> {
    let mut buf = [0u8; 4096];
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let n = file.read(&mut buf).map_err(|e| e.to_string())?;
    let head = &buf[..n];
    #[cfg(feature = "opus")]
    if contains(head, b"OpusHead") {
        return Ok(Kind::Opus);
    }
    #[cfg(feature = "vorbis")]
    if contains(head, b"vorbis") {
        return Ok(Kind::Vorbis);
    }
    Err(format!(
        "{}: Ogg stream doesn't look like a supported codec in this build (no identification \
         packet found in the first {n} bytes)",
        path.display()
    ))
}

#[cfg(any(feature = "opus", feature = "vorbis"))]
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn open_reader(path: &Path) -> Result<Box<dyn FormatReader>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let source: Box<dyn Read + Send> = Box::new(file);
    let map_err = |e: CadenceError| format!("{}: {e}", path.display());
    let reader: Box<dyn FormatReader> = match detect(path)? {
        #[cfg(feature = "wav")]
        Kind::Wav => Box::new(tpt_av_cadence_wav::WavReader::open(source).map_err(map_err)?),
        #[cfg(feature = "aiff")]
        Kind::Aiff => Box::new(tpt_av_cadence_aiff::AiffReader::open(source).map_err(map_err)?),
        #[cfg(feature = "flac")]
        Kind::Flac => Box::new(tpt_av_cadence_flac::FlacReader::open(source).map_err(map_err)?),
        #[cfg(feature = "mp3")]
        Kind::Mp3 => Box::new(tpt_av_cadence_mp3::Mp3Reader::open(source).map_err(map_err)?),
        #[cfg(feature = "vorbis")]
        Kind::Vorbis => {
            Box::new(tpt_av_cadence_vorbis::VorbisFormatReader::open(source).map_err(map_err)?)
        }
        #[cfg(feature = "opus")]
        Kind::Opus => Box::new(tpt_av_cadence_opus::OggOpusReader::open(source).map_err(map_err)?),
        #[cfg(feature = "aac")]
        Kind::Aac => Box::new(tpt_av_cadence_aac::AacReader::open(source).map_err(map_err)?),
    };
    Ok(reader)
}

fn cmd_info(path_str: &str) -> Result<(), String> {
    let path = Path::new(path_str);
    let reader = open_reader(path)?;
    let info = reader.info();
    println!("file:          {}", path.display());
    println!("format:        {:?}", info.format);
    println!("sample rate:   {} Hz", info.sample_rate);
    println!(
        "channels:      {} ({:?})",
        info.channels, info.channel_layout
    );
    println!("bit depth:     {}", info.bit_depth);
    match info.total_frames {
        Some(frames) => {
            let seconds = frames as f64 / info.sample_rate as f64;
            println!("total frames:  {frames} ({seconds:.3} s)");
        }
        None => println!("total frames:  unknown (streaming source)"),
    }
    Ok(())
}

fn cmd_decode(path_str: &str, out: Option<&str>) -> Result<(), String> {
    let path = Path::new(path_str);
    let mut reader = open_reader(path)?;
    let channels = reader.info().channels as usize;
    let sample_rate = reader.info().sample_rate;

    match out {
        Some(out_path) => {
            let file = File::create(out_path).map_err(|e| format!("{out_path}: {e}"))?;
            let mut writer = WavWriter::new(BufWriter::new(file), sample_rate, channels as u16)
                .map_err(|e| e.to_string())?;
            let mut buf = vec![0.0f32; 8192 * channels];
            let mut total_frames = 0u64;
            loop {
                let frames = reader
                    .decoder()
                    .decode(&mut buf)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                if frames == 0 {
                    break;
                }
                writer
                    .write_samples(&buf[..frames * channels])
                    .map_err(|e| e.to_string())?;
                total_frames += frames as u64;
            }
            writer.finish().map_err(|e| e.to_string())?;
            eprintln!("wrote {total_frames} frames to {out_path}");
        }
        None => {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let mut buf = vec![0.0f32; 8192 * channels];
            loop {
                let frames = reader
                    .decoder()
                    .decode(&mut buf)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                if frames == 0 {
                    break;
                }
                for sample in &buf[..frames * channels] {
                    out.write_all(&sample.to_le_bytes())
                        .map_err(|e| e.to_string())?;
                }
            }
        }
    }
    Ok(())
}

/// Minimal 16-bit PCM WAV writer. `tpt-cadence` doesn't ship any encoders
/// yet (see `todo.md`), but a WAV header is simple enough to hand-roll
/// here without pulling in an encoder dependency.
struct WavWriter<W: Write + std::io::Seek> {
    w: W,
    data_bytes: u32,
}

impl<W: Write + std::io::Seek> WavWriter<W> {
    fn new(mut w: W, sample_rate: u32, channels: u16) -> std::io::Result<Self> {
        let block_align = channels * 2;
        let byte_rate = sample_rate * block_align as u32;
        w.write_all(b"RIFF")?;
        w.write_all(&0u32.to_le_bytes())?; // patched in `finish`
        w.write_all(b"WAVE")?;
        w.write_all(b"fmt ")?;
        w.write_all(&16u32.to_le_bytes())?;
        w.write_all(&1u16.to_le_bytes())?; // PCM
        w.write_all(&channels.to_le_bytes())?;
        w.write_all(&sample_rate.to_le_bytes())?;
        w.write_all(&byte_rate.to_le_bytes())?;
        w.write_all(&block_align.to_le_bytes())?;
        w.write_all(&16u16.to_le_bytes())?; // bits per sample
        w.write_all(b"data")?;
        w.write_all(&0u32.to_le_bytes())?; // patched in `finish`
        Ok(WavWriter { w, data_bytes: 0 })
    }

    fn write_samples(&mut self, samples: &[f32]) -> std::io::Result<()> {
        for &s in samples {
            let clamped = s.clamp(-1.0, 1.0);
            let v = (clamped * i16::MAX as f32) as i16;
            self.w.write_all(&v.to_le_bytes())?;
        }
        self.data_bytes += (samples.len() * 2) as u32;
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<()> {
        use std::io::SeekFrom;
        let riff_size = 4 + (8 + 16) + (8 + self.data_bytes);
        self.w.seek(SeekFrom::Start(4))?;
        self.w.write_all(&riff_size.to_le_bytes())?;
        self.w.seek(SeekFrom::Start(40))?;
        self.w.write_all(&self.data_bytes.to_le_bytes())?;
        self.w.flush()
    }
}
