//! Conformance test against the official IETF RFC 6716 Opus test vectors.
//!
//! This walks every packet of every `.bit` file through one persistent
//! [`tpt_av_cadence_opus::OpusDecoder`] (the full `opus_decoder.c` state
//! machine: SILK-only, CELT-only, and hybrid packets, mode transitions,
//! 5 ms CELT redundancy frames, and internal DTX/PLC handling) and checks
//! two independent signals:
//!
//! 1. **`final_range` (the real, version-independent bit-exactness
//!    contract):** each `.bit` record carries the encoder's final
//!    range-coder state for that packet; a decoder that read exactly the
//!    bits the encoder wrote reproduces it for every packet. This test
//!    requires a 100% match on every vector — any mismatch means the
//!    decoder consumed the wrong bits.
//! 2. **PCM SNR vs the bundled `.dec` files (coarse sanity check only):**
//!    the `.dec` files were produced by the circa-2012 RFC reference
//!    decoder and have drifted from modern libopus at the ~1-LSB level
//!    (libopus 1.5.2's own decode of testvector07 only reaches 83 dB SNR
//!    against its own `.dec`), so a strict 90 dB gate against these files
//!    is stricter than the reference implementation itself can satisfy.
//!    The comparison is reported per vector and gated at a generous floor
//!    that only trips on gross reconstruction errors (missing bands,
//!    desynced state), not on float-rounding drift.
//!
//! This test is `#[ignore]`d by default because the vectors are ~63MB
//! extracted and are not bundled in this repo. To run it:
//!
//! 1. Download `opus_testvectors.tar.gz` from
//!    <https://opus-codec.org/static/testvectors/opus_testvectors.tar.gz>
//!    (cited in RFC 6716 §6.1 / Appendix A.4) and extract it somewhere.
//! 2. `OPUS_TESTVECTORS_DIR=<path to the extracted testvectorNN.{bit,dec}
//!    files> cargo test -p tpt-av-cadence-opus --release -- --ignored`

use std::path::{Path, PathBuf};

use tpt_av_cadence_opus::packet::parse_packet;
use tpt_av_cadence_opus::{celt_frame_size, OpusDecoder};

/// PCM sanity floor vs the bundled `.dec` files. The files predate modern
/// libopus by years of float refinements (see module docs), so this only
/// catches gross reconstruction errors; the durable bit-exactness gate is
/// the per-packet `final_range` match.
const MIN_SANITY_SNR_DB: f64 = 20.0;

struct BitRecord {
    /// One packet's bytes, exactly as fed to `parse_packet`.
    packet: Vec<u8>,
    /// The encoder's range-coder final state after this packet.
    final_range: u32,
}

fn read_bit_file(path: &Path) -> Vec<BitRecord> {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    let mut records = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        assert!(
            pos + 8 <= data.len(),
            "{path:?}: truncated record header at offset {pos}"
        );
        let length = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let final_range = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        pos += 8;
        assert!(
            pos + length <= data.len(),
            "{path:?}: record at offset {pos} claims {length} bytes, past EOF"
        );
        records.push(BitRecord {
            packet: data[pos..pos + length].to_vec(),
            final_range,
        });
        pos += length;
    }
    records
}

fn read_dec_file(path: &Path) -> Vec<i16> {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    assert_eq!(data.len() % 2, 0, "{path:?}: odd byte length for i16 PCM");
    data.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// `opus_demo`'s float→int16 conversion (`FLOAT2INT16`): scale, clamp,
/// round-to-nearest.
fn f32_to_i16(x: f32) -> i16 {
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

#[derive(Default)]
struct RunStats {
    samples_compared: u64,
    max_abs_diff: i32,
    error_energy: f64,
    signal_energy: f64,
}

impl RunStats {
    fn record(&mut self, decoded_i16: i16, reference_i16: i16) {
        let diff = (decoded_i16 as i32 - reference_i16 as i32).abs();
        self.max_abs_diff = self.max_abs_diff.max(diff);
        self.error_energy += (diff as f64) * (diff as f64);
        self.signal_energy += (reference_i16 as f64) * (reference_i16 as f64);
        self.samples_compared += 1;
    }

    fn snr_db(&self) -> f64 {
        if self.error_energy <= 0.0 || self.signal_energy <= 0.0 {
            return f64::INFINITY;
        }
        10.0 * (self.signal_energy / self.error_energy).log10()
    }
}

/// Result of scanning one test vector.
struct VectorResult {
    name: String,
    stats: RunStats,
    /// Stats against a live libopus 1.5.2 decode (`OPUS_ORACLE_PCM_DIR`),
    /// when available — the authoritative PCM comparison target.
    oracle_stats: Option<RunStats>,
    packet_count: u64,
    range_match_count: u64,
    /// (packet byte offset, got, want) of the first range mismatch, if any.
    first_range_mismatch: Option<(usize, u32, u32)>,
}

fn read_oracle_pcm(dir: &Path, name: &str) -> Option<Vec<i16>> {
    let path = dir.join(format!("{name}.pcm"));
    let data = std::fs::read(path).ok()?;
    Some(
        data.chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

fn run_vector(dir: &Path, name: &str) -> VectorResult {
    let bit_path = dir.join(format!("{name}.bit"));
    let dec_path = dir.join(format!("{name}.dec"));
    let records = read_bit_file(&bit_path);
    let reference = read_dec_file(&dec_path);
    let oracle = std::env::var_os("OPUS_ORACLE_PCM_DIR").map(|d| {
        read_oracle_pcm(Path::new(&d), name)
            .unwrap_or_else(|| panic!("{name}: oracle PCM file missing or truncated"))
    });
    let mut oracle_stats = RunStats::default();

    let mut opus = OpusDecoder::new(2).unwrap();
    let mut sample_offset: u64 = 0; // samples-per-channel already produced
    let mut stats = RunStats::default();
    let mut packet_count = 0u64;
    let mut range_match_count = 0u64;
    let mut first_range_mismatch = None;

    for (record_index, record) in records.iter().enumerate() {
        let payload = &record.packet;
        let packet = match parse_packet(payload) {
            Ok(p) => p,
            Err(e) => {
                panic!(
                    "{name}: parse_packet failed on record {record_index} (byte-parallel with \
                     the reference stream, so this aborts the vector): {e}"
                );
            }
        };
        let frame_size = celt_frame_size(&packet);
        let packet_samples = (packet.frame_count() * frame_size) as u64;

        let mut pcm = vec![0f32; packet_samples as usize * 2];
        match opus.decode_packet(&packet, payload, &mut pcm) {
            Ok(produced) => {
                debug_assert_eq!(produced as u64, packet_samples);
                packet_count += 1;
                let got = opus.range_final();
                if got == record.final_range {
                    range_match_count += 1;
                } else if first_range_mismatch.is_none() {
                    first_range_mismatch = Some((record_index, got, record.final_range));
                    if std::env::var_os("OPUS_TV_DEBUG").is_some() {
                        eprintln!(
                            "  {name} pkt#{record_index}@offset={sample_offset}: RANGE MISMATCH \
                             got={got:08x} want={:08x} toc.config={} stereo={} code={} len={}",
                            record.final_range,
                            packet.toc.config,
                            packet.toc.stereo,
                            packet.toc.code,
                            payload.len(),
                        );
                    }
                }

                let ref_start = (sample_offset as usize) * 2;
                let ref_end = ref_start + pcm.len();
                if ref_end <= reference.len() {
                    let mut pkt_se = 0f64;
                    let mut pkt_sd = 0f64;
                    for (i, chunk) in pcm.chunks_exact(2).enumerate() {
                        let dl = f32_to_i16(chunk[0]);
                        let dr = f32_to_i16(chunk[1]);
                        let rl = reference[ref_start + i * 2];
                        let rr = reference[ref_start + i * 2 + 1];
                        stats.record(dl, rl);
                        stats.record(dr, rr);
                        if let Some(o) = &oracle {
                            let ol = o[ref_start + i * 2];
                            let orr = o[ref_start + i * 2 + 1];
                            pkt_se += ((dl as i32 - ol as i32).pow(2)
                                + (dr as i32 - orr as i32).pow(2))
                                as f64;
                            pkt_sd += (ol as i32 * ol as i32 + orr as i32 * orr as i32) as f64;
                        }
                    }
                    if oracle.is_some() && pkt_se > 0.0 && pkt_sd > 0.0 && pkt_se > pkt_sd * 1e-6 {
                        eprintln!(
                            "PKTSNR {name} pkt#{record_index} t={:.3}s snr={:.1} toc=0x{:02x} cfg={} st={} code={} frames={} len={}",
                            sample_offset as f64 / 96000.0,
                            10.0 * (pkt_sd / pkt_se).log10(),
                            packet.toc.config as u8 * 8
                                + u8::from(packet.toc.stereo) * 4
                                + packet.toc.code,
                            packet.toc.config,
                            packet.toc.stereo,
                            packet.toc.code,
                            packet.frame_count(),
                            payload.len(),
                        );
                    }
                }
                if let Some(oracle) = &oracle {
                    if ref_end <= oracle.len() {
                        for (i, chunk) in pcm.chunks_exact(2).enumerate() {
                            oracle_stats.record(f32_to_i16(chunk[0]), oracle[ref_start + i * 2]);
                            oracle_stats
                                .record(f32_to_i16(chunk[1]), oracle[ref_start + i * 2 + 1]);
                        }
                    }
                }
            }
            Err(e) => {
                panic!(
                    "{name}: decode_packet failed on record {record_index} at sample offset \
                     {sample_offset}: {e}"
                );
            }
        }

        sample_offset += packet_samples;
    }

    // Optional: dump our decoded PCM for offline sample-level diffing.
    if let Ok(path) = std::env::var("OPUS_TV_DUMP_PCM") {
        // Re-run the vector, this time writing the interleaved s16 stream.
        let records = read_bit_file(&bit_path);
        let mut opus2 = OpusDecoder::new(2).unwrap();
        let mut pcm_all: Vec<u8> = Vec::new();
        for record in &records {
            let packet = parse_packet(&record.packet).unwrap();
            let fs = celt_frame_size(&packet);
            let mut pcm = vec![0f32; packet.frame_count() * fs * 2];
            opus2
                .decode_packet(&packet, &record.packet, &mut pcm)
                .unwrap();
            for chunk in pcm.chunks_exact(2) {
                let l = f32_to_i16(chunk[0]).to_le_bytes();
                let r = f32_to_i16(chunk[1]).to_le_bytes();
                pcm_all.extend_from_slice(&l);
                pcm_all.extend_from_slice(&r);
            }
        }
        std::fs::write(path, &pcm_all).unwrap();
    }

    VectorResult {
        name: name.to_string(),
        stats,
        oracle_stats: if oracle.is_some() {
            Some(oracle_stats)
        } else {
            None
        },
        packet_count,
        range_match_count,
        first_range_mismatch,
    }
}

/// Runs the full conformance comparison over all 12 official test vectors.
/// See the module doc-comment for how to point this at a local copy of the
/// vectors.
#[test]
#[ignore = "needs OPUS_TESTVECTORS_DIR pointing at the extracted opus_testvectors; see module docs"]
fn opus_conformance_test_vectors() {
    let dir = match std::env::var_os("OPUS_TESTVECTORS_DIR") {
        Some(v) => PathBuf::from(v),
        None => panic!(
            "OPUS_TESTVECTORS_DIR is not set. Download opus_testvectors.tar.gz from \
             https://opus-codec.org/static/testvectors/opus_testvectors.tar.gz, extract it, \
             and rerun as: OPUS_TESTVECTORS_DIR=<path> cargo test -p tpt-av-cadence-opus \
             --release -- --ignored"
        ),
    };
    assert!(
        dir.is_dir(),
        "OPUS_TESTVECTORS_DIR={dir:?} is not a directory"
    );

    let mut failures = Vec::new();
    println!(
        "{:<14} {:>18} {:>14} {:>10} {:>16}  {:>10}",
        "vector", "samples/ch", "max |diff|", "SNR (dB)", "range matches", "SNR vs oracle"
    );
    // Optional single-vector filter (e.g. OPUS_TV_ONLY=testvector10), used
    // to align oracle-trace frame counters with one vector's stream.
    let only = std::env::var("OPUS_TV_ONLY").ok();
    for n in 1..=12u32 {
        let name = format!("testvector{n:02}");
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        let result = run_vector(&dir, &name);
        let s = &result.stats;
        let oracle_col = match &result.oracle_stats {
            Some(o) if o.samples_compared > 0 => format!("{:>10.1}", o.snr_db()),
            _ => format!("{:>10}", "-"),
        };
        println!(
            "{:<14} {:>18} {:>14} {:>10.1} {:>8}/{:<8}  {}",
            result.name,
            s.samples_compared / 2, // per-channel
            s.max_abs_diff,
            s.snr_db(),
            result.range_match_count,
            result.packet_count,
            oracle_col,
        );

        // The durable RFC 6716 contract: a decoder that read exactly the
        // bits the encoder wrote reproduces every packet's final range
        // state. Anything less is an entropy-decode desync and fails.
        if result.range_match_count != result.packet_count {
            let detail = match result.first_range_mismatch {
                Some((idx, got, want)) => {
                    format!(" (first mismatch: packet #{idx}, got {got:08x}, want {want:08x})")
                }
                None => String::new(),
            };
            failures.push(format!(
                "{}: final_range match {}/{}{}",
                result.name, result.range_match_count, result.packet_count, detail
            ));
        }
        // Coarse PCM sanity floor (see module docs for why this is not
        // the 90 dB gate the raw `.dec` comparison once used).
        if s.samples_compared > 0 && s.snr_db() < MIN_SANITY_SNR_DB {
            failures.push(format!(
                "{}: PCM SNR vs bundled .dec = {:.1} dB (sanity floor {MIN_SANITY_SNR_DB} dB), \
                 max |diff| = {}",
                result.name,
                s.snr_db(),
                s.max_abs_diff
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Opus conformance failures:\n{}",
        failures.join("\n")
    );
}
