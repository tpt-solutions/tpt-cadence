//! Conformance test against the official IETF RFC 6716 Opus test vectors.
//!
//! This crate has no SILK decoder yet, so it cannot decode the SILK/Hybrid
//! frames these vectors also exercise. Instead, this test walks each
//! packet in a `.bit` file, decodes contiguous runs of CELT-only packets
//! with [`tpt_av_cadence_opus::decode_celt_only_packet`], and compares the
//! produced PCM against the matching slice of the reference `.dec` file
//! (skipping SILK/Hybrid packets while still advancing the sample offset).
//!
//! This test is `#[ignore]`d by default because the vectors are ~63MB
//! extracted and are not bundled in this repo. To run it:
//!
//! 1. Download `opus_testvectors.tar.gz` from
//!    <https://opus-codec.org/static/testvectors/opus_testvectors.tar.gz>
//!    (cited in RFC 6716 §6.1 / Appendix A.4) and extract it somewhere.
//! 2. `OPUS_TESTVECTORS_DIR=<path to the extracted testvectorNN.{bit,dec}
//!    files> cargo test -p tpt-av-cadence-opus --release -- --ignored`
//!
//! ## File formats (reverse-engineered, not documented upstream)
//!
//! `testvectorNN.bit`: a sequence of records, each
//! `[4-byte BE length][4-byte BE final range-coder state][length bytes of
//! packet data]`. The final-range field is the encoder's range-coder state
//! after encoding that packet (what the reference `opus_compare`/
//! `opus_demo` tools use to detect decoder desync). This test cross-checks
//! it against [`CeltDecoder::final_range`] for every CELT-only packet
//! (independent of the PCM comparison, and a much more precise signal: a
//! range mismatch means the decoder read the *wrong bits* for that packet,
//! whereas a PCM mismatch alone could just be float rounding) and reports
//! the match count per vector.
//!
//! `testvectorNN.dec`: raw interleaved 16-bit little-endian PCM, 48 kHz,
//! 2 channels, for the entire decoded stream.
//!
//! ## Known limitation (as of this writing)
//!
//! The CELT-only packet mapping in [`decode_celt_only_packet`] (start/end
//! band, channel handling, frame sizing) has been verified correct: it
//! matches libopus's `opus_decode_frame` mapping line for line, and a
//! meaningful fraction of CELT-only packets in every vector below decode
//! bit-exactly (`final_range` matches the encoder's recorded value). But a
//! substantial fraction of packets *don't* match, and the mismatch rate
//! climbs steeply with `coded_bands` (the number of PVQ-coded bands in a
//! packet: near 0% at 1-2 bands, >80% at 15+), independent of whether the
//! frame is transient, stereo, dual-stereo, or postfilter-enabled. Since a
//! `final_range` mismatch means the decoder consumed the wrong number of
//! bits from the entropy stream, this points to a residual, not-yet-
//! root-caused bug somewhere in the per-band PVQ/allocation recursion in
//! `celt/bands.rs` or `celt/rate.rs` (most likely `quant_partition`'s split
//! path or the leaf-band `bits2pulses`/`alg_unquant` path, since those are
//! the only bit-consuming calls whose frequency scales with `coded_bands`)
//! — not in this file's packet-to-CELT wiring. See `todo.md` for the
//! current status and what's been ruled out.

use std::path::{Path, PathBuf};

use tpt_av_cadence_opus::celt::CeltDecoder;
use tpt_av_cadence_opus::packet::{parse_packet, Mode};
use tpt_av_cadence_opus::silk::decoder::SilkDecoder;
use tpt_av_cadence_opus::{celt_frame_size, decode_celt_only_packet, decode_silk_only_packet};

/// Max acceptable per-sample error, in 16-bit PCM units, for a CELT-only
/// run to be considered passing. The RFC's own quality guidance targets
/// at least 90dB SNR; we also check that directly below. A couple of ULP
/// of slack accounts for float MDCT synthesis order differing slightly
/// from the reference's derivation, not for a wrong band/channel mapping.
const MAX_ABS_DIFF_I16: i32 = 2;
const MIN_SNR_DB: f64 = 90.0;

struct BitRecord {
    /// Byte offset/length of the packet payload within the `.bit` file's
    /// backing buffer.
    packet: Vec<u8>,
    /// The encoder's range-coder final state after this packet, for
    /// cross-checking against [`CeltDecoder::final_range`].
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
        if self.error_energy <= 0.0 {
            return f64::INFINITY;
        }
        if self.signal_energy <= 0.0 {
            return f64::INFINITY;
        }
        10.0 * (self.signal_energy / self.error_energy).log10()
    }
}

/// Result of scanning one test vector.
struct VectorResult {
    name: String,
    stats: RunStats,
    celt_packet_count: u64,
    range_match_count: u64,
    silk_packet_count: u64,
    silk_stats: RunStats,
}

fn run_vector(dir: &Path, name: &str) -> VectorResult {
    let bit_path = dir.join(format!("{name}.bit"));
    let dec_path = dir.join(format!("{name}.dec"));
    let records = read_bit_file(&bit_path);
    let reference = read_dec_file(&dec_path);

    let mut celt = CeltDecoder::new(2, 48_000).unwrap();
    let mut silk = SilkDecoder::new(2).unwrap();
    let mut prev_mode_was_celt = false;
    let mut sample_offset: u64 = 0; // samples-per-channel already advanced
    let mut stats = RunStats::default();
    let mut silk_stats = RunStats::default();
    // Tracked for debug output only (to print at the start of a contiguous
    // CELT-only run) — the decoder itself is never reset mid-file. Real
    // libopus does not invoke celt_decode_with_ec at all for SILK-only
    // frames, so the CELT decoder's state (energy history, MDCT overlap,
    // postfilter memory) simply carries over unchanged across a run of
    // SILK-only packets to the next CELT-only run; resetting here would
    // (and empirically did) desync the very first frame of each run from
    // the reference. Hybrid packets are the one case where this crate's
    // skip diverges from the reference (real libopus does run CELT, at
    // start_band 17, on hybrid frames) — expect the first CELT-only run
    // after a hybrid packet to mismatch until state resynchronizes, if it
    // does at all; this is the missing-SILK/hybrid limitation, not a bug
    // in the CELT-only mapping itself.
    let mut prev_was_celt = false;
    let mut celt_packet_count = 0u64;
    let mut range_match_count = 0u64;
    let mut silk_packet_count = 0u64;

    for record in &records {
        let payload = &record.packet;
        let packet = match parse_packet(payload) {
            Ok(p) => p,
            Err(e) => {
                // A packet this crate's parser rejects outright: cannot
                // safely continue tracking offsets against the reference,
                // so stop here (this shouldn't happen for genuine RFC
                // vectors within the CELT-only spec this test covers).
                panic!(
                    "{name}: parse_packet failed on a packet at sample offset {sample_offset}: {e}"
                );
            }
        };
        let frame_size = celt_frame_size(&packet);
        let frame_count = packet.frame_count();
        let packet_samples = (frame_count * frame_size) as u64;

        if packet.toc.mode() == Mode::Celt {
            let starting_run = !prev_was_celt;
            prev_was_celt = true;
            let mut pcm = vec![0f32; frame_count * frame_size * 2];
            match decode_celt_only_packet(&mut celt, &packet, payload, &mut pcm) {
                Ok(produced) => {
                    debug_assert_eq!(produced as u64, packet_samples);
                    celt_packet_count += 1;
                    let got = celt.final_range();
                    if got == record.final_range {
                        range_match_count += 1;
                    } else if std::env::var_os("OPUS_TV_DEBUG").is_some() {
                        eprintln!(
                            "  {name} pkt@offset={sample_offset}: RANGE MISMATCH got={got:08x} want={:08x}",
                            record.final_range
                        );
                    }
                    let ref_start = (sample_offset as usize) * 2;
                    let ref_end = ref_start + pcm.len();
                    if std::env::var_os("OPUS_TV_DEBUG").is_some() {
                        if starting_run {
                            eprintln!(
                                "{name}: run start at sample_offset={sample_offset} toc.config={} stereo={} code={} frame_count={} frame_size={}",
                                packet.toc.config, packet.toc.stereo, packet.toc.code, frame_count, frame_size
                            );
                        }
                        let mut first_bad = None;
                        for (i, chunk) in pcm.chunks_exact(2).enumerate() {
                            let dl = (f32_to_i16(chunk[0]) as i32
                                - reference.get(ref_start + i * 2).copied().unwrap_or(0) as i32)
                                .abs();
                            let dr = (f32_to_i16(chunk[1]) as i32
                                - reference.get(ref_start + i * 2 + 1).copied().unwrap_or(0)
                                    as i32)
                                .abs();
                            if (dl > 50 || dr > 50) && first_bad.is_none() {
                                first_bad = Some(i);
                            }
                        }
                        if let Some(i) = first_bad {
                            eprintln!(
                                "  {name} pkt@offset={sample_offset}: first big diff at i={i}/{} decoded=({:.5},{:.5}) i16=({},{}) ref=({},{})",
                                pcm.len() / 2,
                                pcm[i * 2],
                                pcm[i * 2 + 1],
                                f32_to_i16(pcm[i * 2]),
                                f32_to_i16(pcm[i * 2 + 1]),
                                reference.get(ref_start + i * 2).copied().unwrap_or(0),
                                reference.get(ref_start + i * 2 + 1).copied().unwrap_or(0),
                            );
                        }
                    }
                    if ref_end <= reference.len() {
                        for (i, chunk) in pcm.chunks_exact(2).enumerate() {
                            let ref_l = reference[ref_start + i * 2];
                            let ref_r = reference[ref_start + i * 2 + 1];
                            stats.record(f32_to_i16(chunk[0]), ref_l);
                            stats.record(f32_to_i16(chunk[1]), ref_r);
                        }
                    }
                }
                Err(e) => {
                    panic!("{name}: decode_celt_only_packet failed at sample offset {sample_offset}: {e}");
                }
            }
        } else if packet.toc.mode() == Mode::Silk {
            prev_was_celt = false;
            // libopus resets the SILK decoder after CELT-only packets.
            if prev_mode_was_celt {
                silk.reset();
            }
            let mut pcm = vec![0i16; packet_samples as usize * 2];
            match decode_silk_only_packet(&mut silk, &packet, payload, &mut pcm) {
                Ok(produced) => {
                    debug_assert_eq!(produced as u64, packet_samples);
                    silk_packet_count += 1;
                    let ref_start = (sample_offset as usize) * 2;
                    let ref_end = ref_start + pcm.len();
                    if ref_end <= reference.len() {
                        if std::env::var_os("OPUS_TV_DEBUG").is_some() {
                            let mut pkt_max_diff = 0i32;
                            for (i, &d) in pcm.iter().enumerate() {
                                pkt_max_diff = pkt_max_diff
                                    .max((d as i32 - reference[ref_start + i] as i32).abs());
                            }
                            if pkt_max_diff > 50 {
                                eprintln!(
                                    "  {name} SILK pkt@offset={sample_offset}: max_diff={pkt_max_diff} toc.config={} stereo={} code={} frame_count={} frame_size={}",
                                    packet.toc.config, packet.toc.stereo, packet.toc.code, frame_count, frame_size
                                );
                            }
                        }
                        for (i, &d) in pcm.iter().enumerate() {
                            silk_stats.record(d, reference[ref_start + i]);
                        }
                    } else if std::env::var_os("OPUS_TV_DEBUG").is_some() {
                        eprintln!(
                            "  {name} SILK pkt@offset={sample_offset}: reference too short for {} samples",
                            pcm.len()
                        );
                    }
                }
                Err(e) => {
                    panic!(
                        "{name}: decode_silk_only_packet failed at sample offset {sample_offset}: {e}"
                    );
                }
            }
        } else {
            // Hybrid: not decodable yet; state bookkeeping only.
            prev_was_celt = false;
        }
        prev_mode_was_celt = packet.toc.mode() == Mode::Celt;

        sample_offset += packet_samples;
    }

    VectorResult {
        name: name.to_string(),
        stats,
        celt_packet_count,
        range_match_count,
        silk_packet_count,
        silk_stats,
    }
}

/// Runs the CELT-only conformance comparison over all 12 official test
/// vectors. See the module doc-comment for how to point this at a local
/// copy of the vectors.
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
        "{:<14} {:>18} {:>14} {:>10} {:>16}",
        "vector", "CELT samples/ch", "max |diff|", "SNR (dB)", "range matches"
    );
    for n in 1..=12u32 {
        let name = format!("testvector{n:02}");
        let result = run_vector(&dir, &name);
        let s = &result.stats;
        if s.samples_compared == 0 {
            println!("{:<14} {:>18}", result.name, "no CELT-only segments found");
        } else {
            let snr = s.snr_db();
            println!(
                "{:<14} {:>18} {:>14} {:>10.1} {:>8}/{:<8}",
                result.name,
                s.samples_compared / 2, // per-channel
                s.max_abs_diff,
                snr,
                result.range_match_count,
                result.celt_packet_count,
            );
            if s.max_abs_diff > MAX_ABS_DIFF_I16 || snr < MIN_SNR_DB {
                failures.push(format!(
                    "{}: max_abs_diff={} (limit {}), snr={:.1}dB (limit {}dB)",
                    result.name, s.max_abs_diff, MAX_ABS_DIFF_I16, snr, MIN_SNR_DB
                ));
            }
        }

        let ss = &result.silk_stats;
        if ss.samples_compared == 0 {
            println!(
                "{:<14} {:>18}",
                format!("{}(SILK)", result.name),
                "no SILK-only segments found"
            );
        } else {
            println!(
                "{:<14} {:>18} {:>14} {:>10.1} {:>16}",
                format!("{}(SILK)", result.name),
                ss.samples_compared / 2, // per-channel
                ss.max_abs_diff,
                ss.snr_db(),
                result.silk_packet_count,
            );
        }
    }

    assert!(
        failures.is_empty(),
        "CELT-only conformance failures:\n{}",
        failures.join("\n")
    );
}
