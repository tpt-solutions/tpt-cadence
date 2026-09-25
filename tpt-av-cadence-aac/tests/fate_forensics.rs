//! Temporary forensic harness: per-frame, per-channel SNR against FFmpeg
//! for the FATE CCE/PCE streams. Run with:
//!   AAC_FATE_SAMPLES_DIR=<dir> FATE_NAME=al15_44 cargo test \
//!     -p tpt-av-cadence-aac --test fate_forensics -- --nocapture

use std::path::{Path, PathBuf};

use tpt_av_cadence_core::Decoder;

struct Mp4Aac {
    asc: Option<Vec<u8>>,
    sample_sizes: Vec<usize>,
    chunk_offsets: Vec<usize>,
    mdat: Option<(usize, usize)>,
}

/// Walks MP4 boxes under `range`, collecting the pieces needed to extract
/// the AAC track: the esds AudioSpecificConfig, sample sizes, chunk
/// offsets, and the mdat extent.
fn be32(d: &[u8], p: usize) -> usize {
    u32::from_be_bytes([d[p], d[p + 1], d[p + 2], d[p + 3]]) as usize
}

fn collect_boxes(d: &[u8], start: usize, end: usize, out: &mut Mp4Aac) {
    let mut p = start;
    while p + 8 <= end {
        let mut size = be32(d, p);
        let typ: [u8; 4] = d[p + 4..p + 8].try_into().unwrap();
        let mut hdr = 8;
        if size == 1 {
            size = usize::try_from(u64::from_be_bytes(d[p + 8..p + 16].try_into().unwrap()))
                .unwrap_or(end);
            hdr = 16;
        }
        if size < hdr || p + size > end {
            return;
        }
        match &typ {
            // stsd carries version/flags + entry_count before its sample
            // entries; an mp4a sample entry carries its 28-byte AudioSample
            // entry before child boxes (esds).
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => {
                collect_boxes(d, p + hdr, p + size, out)
            }
            b"stsd" => collect_boxes(d, p + hdr + 8, p + size, out),
            b"mp4a" => collect_boxes(d, p + hdr + 28, p + size, out),
            b"mdat" => out.mdat = Some((p + hdr, p + size)),
            b"stsz" => {
                let uniform = be32(d, p + hdr + 4);
                let count = be32(d, p + hdr + 8);
                if uniform != 0 {
                    out.sample_sizes = vec![uniform; count];
                } else {
                    for i in 0..count {
                        out.sample_sizes.push(be32(d, p + hdr + 12 + i * 4));
                    }
                }
            }
            b"stco" => {
                let count = be32(d, p + hdr + 4);
                for i in 0..count {
                    out.chunk_offsets.push(be32(d, p + hdr + 8 + i * 4));
                }
            }
            b"esds" => {
                // esds: version/flags(4), then MPEG-4 descriptor tags with
                // 0x80-continuation lengths.
                let mut q = p + hdr + 4;
                let read_len = |d: &[u8], q: &mut usize| -> usize {
                    let mut len = 0usize;
                    loop {
                        let b = d[*q];
                        *q += 1;
                        len = (len << 7) | (b & 0x7F) as usize;
                        if b & 0x80 == 0 {
                            break;
                        }
                    }
                    len
                };
                if d[q] == 0x03 {
                    q += 1;
                    let _ = read_len(d, &mut q);
                    q += 3; // ES_ID(2) + flags(1)
                    if d[q] == 0x04 {
                        q += 1;
                        let _ = read_len(d, &mut q);
                        q += 13; // object/stream type, buffer size, bitrates
                        if d[q] == 0x05 {
                            q += 1;
                            let len = read_len(d, &mut q);
                            out.asc = Some(d[q..q + len].to_vec());
                        }
                    }
                }
            }
            _ => {}
        }
        p += size;
    }
}

/// Extracts (AudioSpecificConfig, concatenated AAC samples) from an MP4,
/// mirroring how an MP4 demuxer feeds raw data blocks to `from_config`.
fn demux_aac_mp4(path: &Path) -> (Vec<u8>, Vec<u8>) {
    let d = std::fs::read(path).unwrap();
    let mut out = Mp4Aac {
        asc: None,
        sample_sizes: Vec::new(),
        chunk_offsets: Vec::new(),
        mdat: None,
    };
    collect_boxes(&d, 0, d.len(), &mut out);
    let (_mdat_start, _mdat_end) = out.mdat.expect("no mdat box");
    let mut samples = Vec::new();
    let mut s = 0usize;
    for (i, &co) in out.chunk_offsets.iter().enumerate() {
        let next = out.chunk_offsets.get(i + 1).copied().unwrap_or(usize::MAX);
        let mut pos = co;
        while s < out.sample_sizes.len() && pos < next {
            samples.extend_from_slice(&d[pos..pos + out.sample_sizes[s]]);
            pos += out.sample_sizes[s];
            s += 1;
        }
    }
    (out.asc.expect("no esds AudioSpecificConfig"), samples)
}

#[test]
fn forensics() {
    let dir = std::env::var_os("AAC_FATE_SAMPLES_DIR")
        .map(PathBuf::from)
        .expect("AAC_FATE_SAMPLES_DIR");
    let name = std::env::var("FATE_NAME").unwrap_or_else(|_| "al15_44".into());
    let mp4 = dir.join(format!("{name}.mp4"));
    let (asc_bytes, samples) = demux_aac_mp4(&mp4);
    let asc = tpt_av_cadence_aac::AudioSpecificConfig::parse(&asc_bytes).unwrap();
    let mut decoder = tpt_av_cadence_aac::AacDecoder::from_config(
        &asc,
        Box::new(std::io::Cursor::new(samples)),
    )
    .unwrap();
    let mut pcm: Vec<f32> = Vec::new();
    let mut buf = vec![0.0f32; 6720];
    loop {
        let frames = decoder.decode(&mut buf).unwrap();
        if frames == 0 {
            break;
        }
        let ch = usize::from(decoder.info().channels);
        pcm.extend_from_slice(&buf[..frames * ch]);
    }
    let channels = usize::from(decoder.info().channels);
    let rate = asc.sample_rate().unwrap();
    let reference =
        tpt_av_cadence_test_utils::reference::decode_with_ffmpeg(&mp4, rate, channels as u16)
            .unwrap();
    eprintln!(
        "{name}: ours={} ref={} ch={channels} rate={rate} frame={}",
        pcm.len(),
        reference.len(),
        1024 * channels
    );
    let frame = 1024 * channels;
    let n_frames = pcm.len().min(reference.len()) / frame;
    // find first divergent frame
    let mut first_bad = None;
    for f in 0..n_frames {
        for ch in 0..channels {
            let mut sig = 0.0f64;
            let mut err = 0.0f64;
            for i in 0..1024 {
                let a = pcm[f * frame + i * channels + ch] as f64;
                let b = reference[f * frame + i * channels + ch] as f64;
                sig += a * a;
                err += (a - b) * (a - b);
            }
            let snr = if err > 0.0 { 10.0 * (sig / err).log10() } else { 999.0 };
            if snr < 60.0 {
                eprintln!("frame {f} ch {ch}: SNR {snr:.1} dB");
                if first_bad.is_none() {
                    first_bad = Some((f, ch));
                }
            }
        }
    }
    // correlation matrix for the first divergent frame
    if let Some((f, _)) = first_bad {
        eprintln!("== frame {f} correlation: rows=ours ch, cols=ffmpeg ch ==");
        let rms = |s: &Vec<f32>, off: usize| -> f64 {
            let mut acc = 0.0;
            for i in 0..1024 {
                let v = s[f * frame + i * channels + off] as f64;
                acc += v * v;
            }
            acc.sqrt()
        };
        for a in 0..channels {
            let mut row = String::new();
            let ra = rms(&pcm, a);
            for b in 0..channels {
                let rb = rms(&reference, b);
                let mut dot = 0.0f64;
                for i in 0..1024 {
                    dot += (pcm[f * frame + i * channels + a] as f64)
                        * (reference[f * frame + i * channels + b] as f64);
                }
                let c = if ra > 0.0 && rb > 0.0 { dot / (ra * rb) } else { 0.0 };
                row.push_str(&format!("c{c:+.2} "));
            }
            eprintln!("ours ch{a} (rms {ra:.3}): {row}");
        }
        for b in 0..channels {
            eprintln!("ffmpeg ch{b} rms {:.3}", rms(&reference, b));
        }
    }
}
