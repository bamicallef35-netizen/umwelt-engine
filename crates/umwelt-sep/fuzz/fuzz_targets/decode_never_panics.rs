//! P2.6 — libFuzzer target for the SEP decoder.
//!
//! Invariant under test: `umwelt_sep::decode` never panics on
//! arbitrary 24-byte input. On good input it returns `Ok(SepFrame)`;
//! on any other input it must return `Err(SepError::CrcMismatch{..})`
//! — never panic, never integer-overflow, never index out of bounds.
//!
//! This is the libFuzzer-form companion to the proptest
//! `decode_never_panics_on_arbitrary_input` in `crates/umwelt-sep/
//! src/frame.rs`. The proptest runs in CI; this fuzzer is for long
//! offline campaigns that can burn weeks of CPU.
//!
//! ## How to run
//!
//! ```bash
//! rustup install nightly          # once
//! cargo install cargo-fuzz        # once
//! cd crates/umwelt-sep
//! cargo +nightly fuzz run decode_never_panics
//! ```
//!
//! Corpus, crashes, and artifacts land under `crates/umwelt-sep/
//! fuzz/{corpus,artifacts}/` and are git-ignored.

#![no_main]

use libfuzzer_sys::fuzz_target;
use umwelt_sep::{decode, SepError, SEP_FRAME_BYTES};

fuzz_target!(|data: &[u8]| {
    // SEP frames are fixed-width. Pad shorter inputs with zero and
    // truncate longer ones; libFuzzer will still explore the full
    // 24-byte state space via mutation.
    let mut buf = [0u8; SEP_FRAME_BYTES];
    let n = core::cmp::min(data.len(), SEP_FRAME_BYTES);
    buf[..n].copy_from_slice(&data[..n]);

    // The only acceptable outcomes:
    //   - Ok(frame) when the CRC happens to validate
    //   - Err(CrcMismatch) otherwise
    // Any panic here is a fuzz finding.
    match decode(&buf) {
        Ok(_) | Err(SepError::CrcMismatch { .. }) => {}
    }
});
