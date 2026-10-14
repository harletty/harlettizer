//! Encoding a syncframe allocates nothing once the encoder is warm.
//!
//! The house rule for a frame path, and one that is easy to break without
//! noticing: a buffer made per frame costs nothing a test of the output can
//! see. So the allocator counts, and the test holds the count to nought over
//! frames encoded after the first few, for each shape of stream — coupled,
//! uncoupled, stereo with rematrixing, the alternating frame size of 44.1 kHz.

use hz_ac3::{ChannelMode, Config, Encoder, FRAME_SAMPLES, Metadata};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn a_warm_encoder_allocates_nothing() {
    for (rate, mode, lfe, kbps, coupling) in [
        (48_000, ChannelMode::ThreeTwo, true, 448, true),
        (48_000, ChannelMode::ThreeTwo, true, 640, true),
        (48_000, ChannelMode::Stereo, false, 192, true),
        (44_100, ChannelMode::Stereo, false, 192, false),
    ] {
        let mut encoder = Encoder::new(Config {
            sample_rate: rate,
            mode,
            lfe,
            bit_rate: Some(kbps),
            coupling,
            metadata: Metadata::default(),
        })
        .unwrap();
        let channels = encoder.channels();
        let mut planes = vec![vec![0f32; FRAME_SAMPLES]; channels];
        let mut out = Vec::with_capacity(8192);
        let mut seed = 1u32;
        let mut frame = |encoder: &mut Encoder, planes: &mut Vec<Vec<f32>>, out: &mut Vec<u8>| {
            for plane in planes.iter_mut() {
                for v in plane.iter_mut() {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    *v = ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.5;
                }
            }
            let mut refs: [&[f32]; 6] = [&[]; 6];
            for (r, plane) in refs.iter_mut().zip(planes.iter()) {
                *r = plane;
            }
            out.clear();
            encoder.encode(&refs[..channels], out).unwrap();
        };
        for _ in 0..3 {
            frame(&mut encoder, &mut planes, &mut out);
        }
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        for _ in 0..20 {
            frame(&mut encoder, &mut planes, &mut out);
        }
        let allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;
        assert_eq!(allocated, 0, "{rate} Hz, {mode:?}, {kbps} kb/s");
    }
}
