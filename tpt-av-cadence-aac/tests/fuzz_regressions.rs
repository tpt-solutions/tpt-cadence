use tpt_av_cadence_aac::AacDecoder;
use tpt_av_cadence_core::Decoder;

/// SBR VARVAR/FIXVAR grid with trailing borders underflowing `t_env` used to panic.
#[test]
fn sbr_grid_border_underflow() {
    let data: Vec<u8> = vec![
        255, 249, 90, 224, 3, 1, 0, 41, 0, 0, 0, 8, 25, 249, 90, 224, 3, 1, 224, 231, 1, 0, 41, 0,
        0, 0, 8, 25, 249, 90, 224, 3, 1, 182, 41, 228, 182, 41, 228, 3, 1, 252, 41,
    ];
    if let Ok(mut d) = AacDecoder::open(Box::new(std::io::Cursor::new(data))) {
        let ch = d.info().channels.max(1) as usize;
        let mut buf = vec![0.0f32; 2048 * ch];
        while let Ok(n) = d.decode(&mut buf) {
            if n == 0 {
                break;
            }
        }
    }
}
