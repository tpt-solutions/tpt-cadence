use tpt_av_cadence_mp3::synth;
#[test]
fn dbg_dct_impulse() {
    let mut g = vec![0.0f32; 576];
    g[0] = 1.0;
    synth::dct_ii(&mut g, 18);
    let mut out = Vec::new();
    for v in &g {
        out.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write("../tools/rust_dct.bin", &out).unwrap();
    println!("rust dct band0: {:?}", &g[..6]);
    println!("rust dct band17[0..3]: {:?}", &g[17 * 18..17 * 18 + 3]);
}

#[test]
fn dbg_synth_impulse() {
    let mut grbuf = vec![0.0f32; 1152];
    grbuf[0] = 1.0;
    let mut qmf = [0.0f32; 960];
    let mut lins = vec![0.0f32; 33 * 64];
    let mut pcm = vec![0.0f32; 1152];
    synth::synth_granule(&mut qmf, &mut grbuf, 2, &mut pcm, &mut lins);
    for (i, v) in pcm.iter().enumerate().take(8) {
        println!("rust pcm[{i}] = {v:.9}");
    }
}
