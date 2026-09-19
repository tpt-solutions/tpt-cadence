//! Real-time-safety verification: after `open()`, `Decoder::decode` must be
//! allocation-free (the crate's RT contract; see `tpt-av-cadence-core`'s
//! `Decoder` docs). A counting global allocator tallies every allocation
//! during a decode sweep over the bundled fixtures and fails if any occurs
//! after the first priming decode.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_mp3::Mp3Decoder;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// System allocator wrapped with a counter (toggled via a thread-local so
/// `open()` may allocate freely).
struct Counting;

static COUNTING: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) == 1 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
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

/// Full decode sweep. Returns the total frames and the worst allocation
/// count of any *successful* `decode` call (error paths may format
/// messages, which allocates; that is accepted project-wide).
fn decode_sweep(decoder: &mut Mp3Decoder) -> (usize, usize) {
    let channels = decoder.info().channels as usize;
    let mut total = 0usize;
    let mut worst = 0usize;
    let mut buf = vec![0.0f32; 1152 * channels];
    loop {
        ALLOCATIONS.store(0, Ordering::Relaxed);
        match decoder.decode(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                worst = worst.max(ALLOCATIONS.load(Ordering::Relaxed));
                total += n;
            }
            Err(_) => {
                // Error paths may allocate (formatted messages).
                break;
            }
        }
    }
    (total, worst)
}

#[test]
fn decode_is_allocation_free_after_init() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut fixtures = 0usize;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("mp3") {
            continue;
        }
        let data = std::fs::read(&path).unwrap();
        let mut decoder =
            Mp3Decoder::open(Box::new(Cursor::new(data.clone()))).unwrap_or_else(|e| {
                panic!("{}: open failed: {e}", path.display());
            });

        // Prime the decoder once (first frame pulls any lazy one-time
        // setup), then count allocations across a full decode sweep.
        let mut priming = vec![0.0f32; 1152 * decoder.info().channels as usize];
        let _ = decoder.decode(&mut priming).unwrap();

        ALLOCATIONS.store(0, Ordering::Relaxed);
        COUNTING.store(1, Ordering::Relaxed);
        let (frames, worst) = decode_sweep(&mut decoder);
        COUNTING.store(0, Ordering::Relaxed);

        assert!(frames > 0, "{}: decode produced no audio", path.display());
        assert_eq!(
            worst,
            0,
            "{}: a successful decode allocated {worst} times (real-time contract violation)",
            path.display()
        );
        fixtures += 1;
    }
    assert!(fixtures >= 10, "expected the ten bundled fixtures");
}
