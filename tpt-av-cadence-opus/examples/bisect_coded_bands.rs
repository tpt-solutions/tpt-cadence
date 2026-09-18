//! Temporary investigative tool for the CELT `final_range` desync bug (see
//! todo.md's "CELT `final_range` desync bug" section). Walks every
//! CELT-only packet in a single test-vector `.bit` file, buckets them by
//! `coded_bands` (via `CeltDecoder::last_coded_bands`), and reports the
//! pass/fail (final_range match) rate per bucket, plus the packet index of
//! the first failing packet at the smallest failing coded_bands count and
//! the last passing packet before it.
//!
//! Usage: `cargo run -p tpt-av-cadence-opus --release --example
//! bisect_coded_bands -- <path-to-testvector07.bit>`

use std::collections::BTreeMap;
use std::path::Path;

use tpt_av_cadence_opus::celt::CeltDecoder;
use tpt_av_cadence_opus::packet::{parse_packet, Mode};
use tpt_av_cadence_opus::{celt_frame_size, decode_celt_only_packet};

struct BitRecord {
    packet: Vec<u8>,
    final_range: u32,
}

fn read_bit_file(path: &Path) -> Vec<BitRecord> {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    let mut records = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let length = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let final_range = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        pos += 8;
        records.push(BitRecord {
            packet: data[pos..pos + length].to_vec(),
            final_range,
        });
        pos += length;
    }
    records
}

#[derive(Default, Clone, Copy)]
struct Bucket {
    pass: u64,
    fail: u64,
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| panic!("usage: bisect_coded_bands <testvector.bit>"));
    let records = read_bit_file(Path::new(&path));

    let mut celt = CeltDecoder::new(2, 48_000).unwrap();
    let mut buckets: BTreeMap<usize, Bucket> = BTreeMap::new();

    // Track, per coded_bands bucket, the packet index of the last PASS and
    // first FAIL seen (in file order) so we can pick concrete packets for
    // the next bisection step.
    let mut last_pass: BTreeMap<usize, usize> = BTreeMap::new();
    let mut first_fail: BTreeMap<usize, usize> = BTreeMap::new();

    let mut celt_pkt_index = 0usize;

    for (record_idx, record) in records.iter().enumerate() {
        let payload = &record.packet;
        let packet = match parse_packet(payload) {
            Ok(p) => p,
            Err(e) => panic!("parse_packet failed on record {record_idx}: {e}"),
        };
        let frame_size = celt_frame_size(&packet);
        let frame_count = packet.frame_count();

        if packet.toc.mode() == Mode::Celt {
            let mut pcm = vec![0f32; frame_count * frame_size * 2];
            match decode_celt_only_packet(&mut celt, &packet, payload, &mut pcm) {
                Ok(_) => {
                    let coded_bands = celt.last_coded_bands();
                    let got = celt.final_range();
                    let pass = got == record.final_range;
                    let b = buckets.entry(coded_bands).or_default();
                    if pass {
                        b.pass += 1;
                        last_pass.insert(coded_bands, celt_pkt_index);
                        if std::env::var_os("SHOW_PASS").is_some() {
                            println!(
                                "PASS celt_pkt#{celt_pkt_index} (record#{record_idx}) coded_bands={coded_bands} config={} stereo={} code={}",
                                packet.toc.config, packet.toc.stereo, packet.toc.code
                            );
                        }
                    } else {
                        b.fail += 1;
                        first_fail.entry(coded_bands).or_insert(celt_pkt_index);
                        println!(
                            "FAIL celt_pkt#{celt_pkt_index} (record#{record_idx}) coded_bands={coded_bands} config={} stereo={} code={} got={got:08x} want={:08x}",
                            packet.toc.config, packet.toc.stereo, packet.toc.code, record.final_range
                        );
                    }
                }
                Err(e) => panic!("decode_celt_only_packet failed at record {record_idx}: {e}"),
            }
            celt_pkt_index += 1;
        }
    }

    println!("\ncoded_bands  pass  fail  fail%");
    for (bands, b) in &buckets {
        let total = b.pass + b.fail;
        let pct = 100.0 * b.fail as f64 / total as f64;
        println!("{bands:>11}  {:>4}  {:>4}  {pct:>5.1}", b.pass, b.fail);
    }

    println!("\nFirst failing coded_bands with at least one PASS at bands-1:");
    for (&bands, &fail_idx) in &first_fail {
        if bands == 0 {
            continue;
        }
        if let Some(&pass_idx) = last_pass.get(&(bands - 1)) {
            println!(
                "  coded_bands={bands}: first fail at celt_pkt#{fail_idx}; a passing packet at coded_bands={} is celt_pkt#{pass_idx}",
                bands - 1
            );
        }
        if let Some(&pass_idx) = last_pass.get(&bands) {
            println!(
                "  coded_bands={bands}: a PASSING packet at the SAME coded_bands is celt_pkt#{pass_idx} (fail at #{fail_idx})"
            );
        }
    }
}
