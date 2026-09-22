use std::io::Cursor;
use tpt_av_cadence_core::{Decoder, Encoder};
use tpt_av_cadence_mp3::{Mp3Decoder, Mp3Encoder};

fn sine_tone(sample_rate: u32, freq: f32, seconds: f32, amp: f32) -> Vec<f32> {
    let n = (sample_rate as f32 * seconds) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            (2.0 * std::f32::consts::PI * freq * t).sin() * amp
        })
        .collect()
}

fn pearson(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len()) as f64;
    let (mut sa, mut sb) = (0.0f64, 0.0f64);
    for i in 0..n as usize { sa += a[i] as f64; sb += b[i] as f64; }
    let (ma, mb) = (sa/n, sb/n);
    let (mut num, mut da, mut db) = (0.0f64,0.0f64,0.0f64);
    for i in 0..n as usize {
        let xa = a[i] as f64 - ma; let xb = b[i] as f64 - mb;
        num += xa*xb; da += xa*xa; db += xb*xb;
    }
    if da<=0.0||db<=0.0 {return 0.0;}
    num/(da.sqrt()*db.sqrt())
}

fn main() {
    let sample_rate = 44_100u32;
    let freq = 1000.0f32;
    let frames = sine_tone(sample_rate, freq, 1.0, 0.6);
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new(&mut buf, sample_rate, 1, 128).unwrap();
        enc.encode(&frames).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    let data = buf.into_inner();
    let mut dec = Mp3Decoder::open(Box::new(Cursor::new(data))).unwrap();
    let info = dec.info().clone();
    let mut out = Vec::new();
    let mut b = vec![0.0f32; 4096*info.channels as usize];
    loop {
        let got = dec.decode(&mut b).unwrap();
        if got == 0 { break; }
        out.extend_from_slice(&b[..got*info.channels as usize]);
    }
    println!("decoded len {} source len {}", out.len(), frames.len());
    for shift in [0i64, 32,64,96,128,160,192,224,256,288,320,352,384,416,448,480,481,512,544,576,608,640,672,700,704,736,768,800,832,864,896,928,960,1000,1024] {
        let (a,b2): (Vec<f32>, Vec<f32>) = if shift >= 0 {
            let s = shift as usize;
            if s >= out.len() { continue; }
            (out[s..].to_vec(), frames.clone())
        } else { (out.clone(), frames.clone()) };
        let c = pearson(&a, &b2);
        println!("shift={shift} corr={c:.4}");
    }
}
