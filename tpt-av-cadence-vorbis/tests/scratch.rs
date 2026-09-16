use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_vorbis::VorbisDecoder;

#[test]
fn scratch_decode_env_file() {
    let path = match std::env::var("VORBIS_TEST_FILE") {
        Ok(p) => p,
        Err(_) => return,
    };
    let f = std::fs::File::open(&path).unwrap();
    match VorbisDecoder::open(Box::new(f)) {
        Ok(mut d) => {
            eprintln!("opened: {:?}", d.info());
            let mut buf = vec![0.0f32; 8192 * d.info().channels as usize];
            let mut total = 0usize;
            loop {
                match d.decode(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(e) => {
                        eprintln!("decode error after {total} frames: {e}");
                        break;
                    }
                }
            }
            eprintln!("total frames: {total}");
        }
        Err(e) => eprintln!("open error: {e}"),
    }
}
