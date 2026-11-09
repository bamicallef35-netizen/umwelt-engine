//! Release-mode integration test for ReplayGuard (P0.8.1).
//!
//! `debug_assert_eq!` elides its arguments in release builds. Before
//! the P0.8.1 fix, `SeenSet::observe` was called *inside* a
//! `debug_assert_eq!`, so release builds silently skipped literal-
//! duplicate recording. This test catches the regression: submitting
//! the same frame twice must yield `Duplicate` on the second call,
//! regardless of optimisation level.
//!
//! Run with: `cargo test --release -p umwelt-sep`

use umwelt_sep::{ReplayVerdict, SepFrame};

fn frame(agent_id: u32, ts: u32) -> SepFrame {
    SepFrame {
        agent_id,
        timestamp_ms: ts,
        ..SepFrame::ZERO
    }
}

#[test]
fn duplicate_detected_in_release_mode() {
    let mut guard = umwelt_sep::ReplayGuard::<64, 16>::new(1_000);

    let f = frame(1, 100);
    assert_eq!(guard.check(&f), ReplayVerdict::Fresh);
    assert_eq!(
        guard.check(&f),
        ReplayVerdict::Duplicate,
        "second submission of the same frame must be Duplicate even in release mode"
    );
}

#[test]
fn duplicate_detected_after_newer_frame() {
    let mut guard = umwelt_sep::ReplayGuard::<64, 16>::new(1_000);

    let f1 = frame(1, 100);
    let f2 = frame(1, 200);

    assert_eq!(guard.check(&f1), ReplayVerdict::Fresh);
    assert_eq!(guard.check(&f2), ReplayVerdict::Fresh);
    // Re-submit f1 — it's within the window, but should be Duplicate
    // because the seen-set already recorded it.
    assert_eq!(
        guard.check(&f1),
        ReplayVerdict::Duplicate,
        "re-submitted earlier frame must be Duplicate, not Fresh"
    );
}

#[test]
fn late_arrival_then_duplicate_in_release_mode() {
    let mut guard = umwelt_sep::ReplayGuard::<64, 16>::new(1_000);

    // Establish high-water at 5000.
    assert_eq!(guard.check(&frame(1, 5_000)), ReplayVerdict::Fresh);
    // Late arrival within window: should be Fresh.
    let late = frame(1, 4_500);
    assert_eq!(guard.check(&late), ReplayVerdict::Fresh);
    // Re-submit the same late arrival: must be Duplicate.
    assert_eq!(
        guard.check(&late),
        ReplayVerdict::Duplicate,
        "re-submitted late arrival must be Duplicate in release mode"
    );
}
