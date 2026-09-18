//! Throwaway debug tool: decode packet 0 of a test vector's .bit file
//! through decode_celt_only_packet with CELT_BAND_TRACE=1, to compare
//! against an instrumented libopus C build's trace for the same packet.
use std::path::Path;

use tpt_av_cadence_opus::celt::CeltDecoder;
use tpt_av_cadence_opus::decode_celt_only_packet;
use tpt_av_cadence_opus::packet::parse_packet;

fn main() {
    let dir = std::env::var("OPUS_TESTVECTORS_DIR").unwrap();
    let name = std::env::args().nth(1).unwrap_or("testvector07".to_string());
    let pkt_idx: usize = std::env::args()
        .nth(2)
        .unwrap_or("0".to_string())
        .parse()
        .unwrap();
    let path = Path::new(&dir).join(format!("{name}.bit"));
    let data = std::fs::read(&path).unwrap();

    let mut pos = 0usize;
    let mut i = 0usize;
    while pos < data.len() {
        let length = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let final_range = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        pos += 8;
        let payload = &data[pos..pos + length];
        pos += length;
        if i == pkt_idx {
            let packet = parse_packet(payload).unwrap();
            println!(
                "packet {i}: mode={:?} config={} stereo={} code={} want_final_range={final_range:08x}",
                packet.toc.mode(),
                packet.toc.config,
                packet.toc.stereo,
                packet.toc.code
            );
            let mut celt = CeltDecoder::new(2, 48_000).unwrap();
            let frame_size = tpt_av_cadence_opus::celt_frame_size(&packet);
            let mut pcm = vec![0f32; frame_size * 2];
            std::env::set_var("CELT_BAND_TRACE", "1");
            let _ = decode_celt_only_packet(&mut celt, &packet, payload, &mut pcm).unwrap();
            println!("got_final_range={:08x}", celt.final_range());
            return;
        }
        i += 1;
    }
    println!("packet {pkt_idx} not found ({i} packets total)");
}
