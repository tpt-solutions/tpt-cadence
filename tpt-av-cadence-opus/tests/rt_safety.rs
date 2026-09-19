//! Real-time-safety verification for the Opus decoder: `OpusDecoder`'s
//! `decode_packet` performs zero allocations on successful calls after
//! construction (the crate's RT contract; error paths may format
//! messages). The stream feeds CELT silence frames plus packet-loss
//! concealment so both the decode and PLC paths are covered.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use tpt_av_cadence_opus::{packet::parse_packet, OpusDecoder};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) == 1 && ALLOCATIONS.fetch_add(1, Ordering::Relaxed) == 0
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

/// Config 31 (CELT fullband 20 ms stereo), code 0, zero payload: a valid
/// low-energy frame (same shape as the unit-test silence frame).
fn silence_packet() -> Vec<u8> {
    vec![31u8 << 3, 0, 0, 0, 0, 0]
}

#[test]
fn decode_packet_is_allocation_free() {
    let mut decoder = OpusDecoder::new(2).unwrap();

    // Prime: decode a few packets outside the counted window (first packet
    // may crossfade from the all-zero state; no allocation is expected
    // even there, but priming keeps the sweep purely steady-state).
    let packet = parse_packet(&silence_packet()).unwrap();
    let mut pcm = vec![0.0f32; 960 * 2];
    for _ in 0..4 {
        decoder
            .decode_packet(&packet, &silence_packet(), &mut pcm)
            .unwrap();
    }

    let mut worst = 0usize;
    let mut calls = 0usize;
    COUNTING.store(1, Ordering::Relaxed);
    for n in 0..200 {
        // A one-byte payload is a DTX stub: decode_frame conceals it.
        let data: Vec<u8> = if n % 16 == 15 {
            vec![silence_packet()[0]]
        } else {
            silence_packet()
        };
        ALLOCATIONS.store(0, Ordering::Relaxed);
        COUNTING.store(0, Ordering::Relaxed);
        let packet = parse_packet(&data).unwrap();
        COUNTING.store(1, Ordering::Relaxed);
        ALLOCATIONS.store(0, Ordering::Relaxed);
        match decoder.decode_packet(&packet, &data, &mut pcm) {
            Ok(_) => {
                worst = worst.max(ALLOCATIONS.load(Ordering::Relaxed));
                calls += 1;
            }
            Err(_) => break,
        }
    }
    COUNTING.store(0, Ordering::Relaxed);

    assert!(calls > 100, "sweep decoded only {calls} packets");
    assert_eq!(
        worst, 0,
        "a successful decode_packet allocated {worst} times (real-time contract violation)"
    );
}
