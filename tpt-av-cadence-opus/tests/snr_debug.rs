use tpt_av_cadence_opus::celt::{encoder::CeltEncoder, decoder::CeltDecoder};
use tpt_av_cadence_opus::packet::parse_packet;
use tpt_av_cadence_opus::decoder::{decode_celt_only_packet, OUTPUT_CHANNELS};

#[test]
fn debug_snr_measurement() {
    const LM: usize = 3;
    const SHORT_MDCT_SIZE: usize = 120;
    const N2: usize = SHORT_MDCT_SIZE << LM; // 960
    const OVERLAP: usize = 120;
    
    let mut enc = CeltEncoder::new(1, LM);
    let mut dec = CeltDecoder::new(2, 48_000).unwrap();
    
    let bytes_per_frame = 160;
    let freq_hz = 440.0f32;
    let sample_rate = 48_000.0f32;
    let mut phase = 0.0f32;

    let mut original = Vec::new();
    let mut decoded = Vec::new();

    for frame_idx in 0..8 {
        let mut pcm_in = [0.0f32; N2];
        for s in pcm_in.iter_mut() {
            *s = 0.5 * phase.sin();
            phase += 2.0 * std::f32::consts::PI * freq_hz / sample_rate;
        }
        original.extend_from_slice(&pcm_in);

        let packet_bytes = enc.encode_frame(&pcm_in, bytes_per_frame);
        let packet = parse_packet(&packet_bytes).unwrap();
        let mut pcm_out = vec![0.0f32; N2 * OUTPUT_CHANNELS];
        decode_celt_only_packet(&mut dec, &packet, &packet_bytes, &mut pcm_out).unwrap();
        
        // Extract both channels for comparison
        let ch0: Vec<f32> = pcm_out.chunks(OUTPUT_CHANNELS).map(|c| c[0]).collect();
        let ch1: Vec<f32> = pcm_out.chunks(OUTPUT_CHANNELS).map(|c| c[1]).collect();
        
        decoded.extend(ch0.iter().copied());
        
        // Print some debug info
        if frame_idx < 4 {
            println!("Frame {}: input[0..10]={:?}", frame_idx, &pcm_in[0..10]);
            println!("Frame {}: ch0[0..10]={:?}", frame_idx, &ch0[0..10]);
            println!("Frame {}: ch1[0..10]={:?}", frame_idx, &ch1[0..10]);
            println!("Frame {}: packet_bytes.len()={}", frame_idx, packet_bytes.len());
            // Print TOC
            println!("Frame {}: TOC config={} stereo={} code={}", frame_idx, packet.toc.config, packet.toc.stereo, packet.toc.code);
        }
    }

    // Print a longer segment for visual comparison
    println!("\n=== Comparison (samples 1000-1100) ===");
    for i in 1000..1100 {
        if i < original.len() && i < decoded.len() {
            println!("  i={}: orig={:.4} dec={:.4} diff={:.4}", i, original[i], decoded[i], original[i] - decoded[i]);
        }
    }
    
    println!("\n=== Comparison (samples 1920-2020) ===");
    for i in 1920..2020 {
        if i < original.len() && i < decoded.len() {
            println!("  i={}: orig={:.4} dec={:.4} diff={:.4}", i, original[i], decoded[i], original[i] - decoded[i]);
        }
    }

    const CODEC_DELAY: i32 = 98;
    let skip = N2 * 2;
    let sig_pow: f64 = original[skip..]
        .iter()
        .map(|&v| (v as f64) * (v as f64))
        .sum();
    let err_pow: f64 = original[skip..]
        .iter()
        .enumerate()
        .map(|(i, &a)| {
            let di = skip as i32 + i as i32 - CODEC_DELAY;
            let b = if di >= 0 && (di as usize) < decoded.len() {
                decoded[di as usize]
            } else {
                0.0
            };
            let d = a as f64 - b as f64;
            d * d
        })
        .sum();
    let snr_db = 10.0 * (sig_pow / err_pow.max(1e-12)).log10();
    println!("\nSNR with CODEC_DELAY=98: {:.1} dB", snr_db);
    
    // Try different delays
    let mut best_snr = f64::NEG_INFINITY;
    let mut best_delay = 0;
    for delay in 0..150 {
        let err_pow: f64 = original[skip..]
            .iter()
            .enumerate()
            .map(|(i, &a)| {
                let di = skip as i32 + i as i32 - delay;
                let b = if di >= 0 && (di as usize) < decoded.len() {
                    decoded[di as usize]
                } else {
                    0.0
                };
                let d = a as f64 - b as f64;
                d * d
            })
            .sum();
        let snr = 10.0 * (sig_pow / err_pow.max(1e-12)).log10();
        if snr > best_snr {
            best_snr = snr;
            best_delay = delay;
        }
        if snr > 0.0 {
            println!("  Delay {}: SNR={:.1} dB", delay, snr);
        }
    }
    println!("Best delay: {} with SNR: {:.1} dB", best_delay, best_snr);
}