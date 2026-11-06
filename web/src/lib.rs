//! The Umwelt Engine core as a WebAssembly module.
//!
//! Nothing of the pipeline is reimplemented here. One
//! [`SemioticPipeline`] at the small profile (`<4, 64, 8>`, the same as
//! the Hyde Park replay example) lives in a static; the page feeds it
//! one sample at a time through [`observe`] and reads the emitted events
//! back as fixed-width records from one buffer.
//!
//! Record layout, eight `u32` per event:
//!
//! | word | icon                | index                     | symbol           |
//! |------|---------------------|---------------------------|------------------|
//! | 0    | 0                   | 1                         | 2                |
//! | 1    | channel             | channels[0]               | channels[0]      |
//! | 2    | 0xFFFF              | channels[1]               | channels[1]      |
//! | 3, 4 | formed_at lo, hi    | formed_at lo, hi          | formed_at lo, hi |
//! | 5    | entropy (Q15 raw)   | coherence (Q15 raw)       | coherence        |
//! | 6    | magnitude (Q15 raw) | delta_us (low 32 bits)    | 0                |
//! | 7    | 0                   | 0                         | 0                |
#![no_std]
#![deny(missing_docs)]

use core::ptr::addr_of_mut;
use umwelt_core::replay::{PipelineEvent, SemioticPipeline};
use umwelt_core::Q15;

const C: usize = 4;
const W: usize = 64;
const B: usize = 8;
const CAP: usize = 512;
const WORDS: usize = 8;

struct State {
    pipe: SemioticPipeline<C, W, B>,
    buf: [u32; CAP * WORDS],
    len: usize,
    dropped: u32,
}

static mut STATE: Option<State> = None;

fn state() -> Option<&'static mut State> {
    // SAFETY: the module is single-threaded; every export runs to
    // completion before the next call, so no two references coexist.
    unsafe { (*addr_of_mut!(STATE)).as_mut() }
}

/// Create (or recreate) the pipeline and empty the event buffer.
#[no_mangle]
pub extern "C" fn init() {
    // SAFETY: as above.
    unsafe {
        *addr_of_mut!(STATE) = Some(State {
            pipe: SemioticPipeline::new(),
            buf: [0; CAP * WORDS],
            len: 0,
            dropped: 0,
        });
    }
}

/// Feed one sample. `q15` is the raw Q15 value; `now_lo`/`now_hi` split
/// the microsecond timestamp. Returns the number of events the sample
/// produced, or `0xFFFF_FFFF` if the pipeline refused it (channel table
/// full) or `init` was never called.
#[no_mangle]
pub extern "C" fn observe(channel: u32, q15: i32, now_lo: u32, now_hi: u32) -> u32 {
    let Some(s) = state() else { return u32::MAX };
    let now = ((now_hi as u64) << 32) | now_lo as u64;
    let before = s.len;
    // Borrow the buffer fields apart from the pipeline so the closure can
    // write while the pipeline is mutably borrowed.
    let State {
        pipe,
        buf,
        len,
        dropped,
    } = s;
    let mut sink = |e: PipelineEvent| {
        let rec = match e {
            PipelineEvent::Icon(i) => [
                0,
                i.channel as u32,
                0xFFFF,
                i.formed_at as u32,
                (i.formed_at >> 32) as u32,
                i.entropy.raw() as i32 as u32,
                i.magnitude.raw() as i32 as u32,
                0,
            ],
            PipelineEvent::Index(x) => [
                1,
                x.channels[0] as u32,
                x.channels[1] as u32,
                x.formed_at as u32,
                (x.formed_at >> 32) as u32,
                x.coherence.raw() as i32 as u32,
                x.delta_us as u32,
                0,
            ],
            PipelineEvent::Symbol(y) => [
                2,
                y.channels[0] as u32,
                y.channels[1] as u32,
                y.formed_at as u32,
                (y.formed_at >> 32) as u32,
                y.coherence.raw() as i32 as u32,
                0,
                0,
            ],
        };
        if *len >= CAP {
            *dropped = dropped.wrapping_add(1);
            return;
        }
        buf[*len * WORDS..(*len + 1) * WORDS].copy_from_slice(&rec);
        *len += 1;
    };
    let q = Q15::from_raw(q15.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    match pipe.observe(channel as u16, q, now, &mut sink) {
        Ok(()) => (s.len - before) as u32,
        Err(_) => u32::MAX,
    }
}

/// Pointer to the event buffer (records of eight `u32`).
#[no_mangle]
pub extern "C" fn events_ptr() -> *const u32 {
    match state() {
        Some(s) => s.buf.as_ptr(),
        None => core::ptr::null(),
    }
}

/// Number of records currently in the buffer.
#[no_mangle]
pub extern "C" fn events_len() -> u32 {
    state().map(|s| s.len as u32).unwrap_or(0)
}

/// Events dropped because the buffer was full since the last `init`.
#[no_mangle]
pub extern "C" fn events_dropped() -> u32 {
    state().map(|s| s.dropped).unwrap_or(0)
}

/// Empty the buffer. The page calls this after it has read the records.
#[no_mangle]
pub extern "C" fn events_clear() {
    if let Some(s) = state() {
        s.len = 0;
    }
}

/// Channels the pipeline has registered so far (at most four).
#[no_mangle]
pub extern "C" fn occupied() -> u32 {
    state().map(|s| s.pipe.occupied() as u32).unwrap_or(0)
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

#[cfg(not(target_arch = "wasm32"))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
