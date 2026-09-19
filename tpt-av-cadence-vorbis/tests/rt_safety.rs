//! Real-time-safety verification for the Vorbis decoder: after `open()`,
//! `Decoder::decode` must perform zero allocations on successful calls
//! (the crate's RT contract; error paths may format messages).

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_vorbis::VorbisDecoder;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) == 1
            && ALLOCATIONS.fetch_add(1, Ordering::Relaxed) == 0
        {
            let bt = std::backtrace::Backtrace::force_capture();
            eprintln!(
                "FIRST ALLOCATION layout={layout:?}
{bt}"
            );
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) == 1 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn decode_is_allocation_free_after_init() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut fixtures = 0usize;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("ogg") {
            continue;
        }
        let data = std::fs::read(&path).unwrap();
        let mut decoder = VorbisDecoder::open(Box::new(Cursor::new(data.clone()))).unwrap();

        // Prime once (headers, first audio packets, any lazy setup), then
        // count allocations across a full decode sweep with per-call
        // accounting so error-path formatting stays exempt.
        let channels = decoder.info().channels as usize;
        let mut priming = vec![0.0f32; 8192 * channels];
        let _ = decoder.decode(&mut priming).unwrap();

        let mut worst = 0usize;
        let mut total = 0usize;
        let mut buf = vec![0.0f32; 8192 * channels];
        COUNTING.store(1, Ordering::Relaxed);
        loop {
            ALLOCATIONS.store(0, Ordering::Relaxed);
            match decoder.decode(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    worst = worst.max(ALLOCATIONS.load(Ordering::Relaxed));
                    total += n;
                }
                Err(_) => break,
            }
        }
        COUNTING.store(0, Ordering::Relaxed);

        assert!(total > 0, "{}: decode produced no audio", path.display());
        assert_eq!(
            worst,
            0,
            "{}: a successful decode allocated {worst} times (real-time contract violation)",
            path.display()
        );
        fixtures += 1;
    }
    assert!(fixtures >= 9, "expected the nine bundled fixtures");
}
