//! End-to-end CELT decoder smoke tests: the decoder must never panic and
//! must always produce finite output for arbitrary input, and DTX frames
//! (empty/missing packets) must conceal.

use tpt_av_cadence_opus::celt::CeltDecoder;

fn xorshift(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

/// Random bytes fed as CELT packets must decode without panicking, and the
/// output must be finite.
#[test]
fn random_packets_never_panic() {
    for channels in [1usize, 2usize] {
        let mut dec = CeltDecoder::new(channels, 48_000).unwrap();
        let mut seed = 0x5EEDu64;
        let mut pcm = vec![0f32; channels * 960];
        for _ in 0..200 {
            let len = (xorshift(&mut seed) % 300) as usize;
            let data: Vec<u8> = (0..len).map(|_| xorshift(&mut seed) as u8).collect();
            let _ = dec.decode(Some(&data), 960, &mut pcm);
            assert!(
                pcm.iter().all(|v| v.is_finite()),
                "non-finite output for packet len {len}"
            );
        }
    }
}

/// Interleaving real-looking packets with DTX (empty) frames must conceal
/// without panicking, for both frame sizes.
#[test]
fn dtx_concealment_smoke() {
    for channels in [1usize, 2usize] {
        for frame_size in [120usize, 240, 480, 960] {
            let mut dec = CeltDecoder::new(channels, 48_000).unwrap();
            let mut seed = 0xF00Du64;
            let mut pcm = vec![0f32; channels * frame_size];
            // A few valid-ish packets to build up state.
            for _ in 0..5 {
                let len = 60 + (xorshift(&mut seed) % 200) as usize;
                let data: Vec<u8> = (0..len).map(|_| xorshift(&mut seed) as u8).collect();
                let _ = dec.decode(Some(&data), frame_size, &mut pcm);
            }
            // Now a run of lost packets through both PLC code paths
            // (pitch-based for the first 40 frames, noise-based after).
            for _ in 0..60 {
                dec.decode(None, frame_size, &mut pcm).unwrap();
                assert!(pcm.iter().all(|v| v.is_finite()));
            }
            // Recovery.
            let data: Vec<u8> = (0..120).map(|_| xorshift(&mut seed) as u8).collect();
            let _ = dec.decode(Some(&data), frame_size, &mut pcm);
            assert!(pcm.iter().all(|v| v.is_finite()));
        }
    }
}

/// The silence flag path (first bit set with a 2-byte packet) must decode.
#[test]
fn tiny_packets_smoke() {
    let mut dec = CeltDecoder::new(2, 48_000).unwrap();
    let mut pcm = vec![0f32; 2 * 960];
    // 2-byte packets: first byte's low bit patterns exercise the silence
    // flag and short-frame paths.
    for b0 in [0u8, 1, 2, 3, 0xFC, 0xFF] {
        let _ = dec.decode(Some(&[b0, 0x55]), 960, &mut pcm);
        assert!(pcm.iter().all(|v| v.is_finite()));
    }
}

/// Invalid frame sizes are rejected.
#[test]
fn invalid_frame_size_rejected() {
    let mut dec = CeltDecoder::new(1, 48_000).unwrap();
    let mut pcm = vec![0f32; 1000];
    assert!(dec.decode(Some(&[0u8; 100]), 1000, &mut pcm).is_err());
}

/// DTX with no prior state (cold start) must work.
#[test]
fn cold_start_dtx() {
    let mut dec = CeltDecoder::new(2, 48_000).unwrap();
    let mut pcm = vec![0f32; 2 * 960];
    for _ in 0..10 {
        dec.decode(None, 960, &mut pcm).unwrap();
        assert!(pcm.iter().all(|v| v.is_finite()));
    }
}
