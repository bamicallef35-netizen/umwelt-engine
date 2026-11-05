#!/usr/bin/env python3
"""Smoke test for the umwelt-pyo3 Python extension (P7.2).

Prerequisite: run `maturin develop` from `crates/umwelt-pyo3/` with an
active virtual environment so the `umwelt` module is importable.

This script is *not* part of `scripts/verify.sh` — it requires Python
+ maturin to have produced a wheel, which is out of scope for the
Rust-only gates. Run it by hand after `maturin develop`:

    python3 scripts/pyo3_smoke.py

Exits 0 on success, 1 on any assertion failure.
"""
from __future__ import annotations

import sys
from typing import Any


def fail(msg: str) -> None:
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


def expect(cond: bool, msg: str) -> None:
    if not cond:
        fail(msg)


def main() -> int:
    try:
        import umwelt  # noqa: WPS433 — runtime import is the point
    except ImportError as e:
        fail(f"import umwelt — did you run `maturin develop`? ({e})")

    # Module constants surface the compile-time profile dimensions.
    expect(umwelt.CHANNELS == 4, f"CHANNELS={umwelt.CHANNELS}, want 4")
    expect(umwelt.ICON_WINDOW == 64, f"ICON_WINDOW={umwelt.ICON_WINDOW}, want 64")
    expect(umwelt.ICON_BINS == 8, f"ICON_BINS={umwelt.ICON_BINS}, want 8")

    # Q15 helpers round-trip within one LSB.
    for x in (-1.0, -0.5, 0.0, 0.25, 0.5, 0.9999, 1.0):
        q = umwelt.q15_from_f32(x)
        back = umwelt.q15_to_f32(q)
        lsb = 1.0 / 32767.0
        expect(abs(back - x) <= lsb, f"Q15 roundtrip off for {x}: got {back}")

    # Q15 helpers clamp out-of-range input.
    expect(umwelt.q15_from_f32(2.5) == 32767, "positive clamp")
    expect(umwelt.q15_from_f32(-2.5) == -32767, "negative clamp")

    # Construct a pipeline and feed alternating samples on two channels
    # at 10 ms cadence. The alternating pattern is enough to keep the
    # icon-layer entropy high, so the baseline-vs-delta trigger will
    # fire at least once per channel after the ring warms.
    p = umwelt.Pipeline()
    expect(p.capacity == 4, f"capacity={p.capacity}")
    expect(p.occupied == 0, f"occupied={p.occupied} before observe")
    expect(p.icon_window == 64, f"icon_window={p.icon_window}")
    expect(p.icon_bins == 8, f"icon_bins={p.icon_bins}")

    counts: dict[str, int] = {"icon": 0, "index": 0, "symbol": 0}
    samples_seen = 0
    for i in range(200):
        q_hi = umwelt.q15_from_f32(0.55 if i % 2 == 0 else -0.55)
        q_lo = umwelt.q15_from_f32(-0.55 if i % 2 == 0 else 0.55)
        tick = 10_000 + i * 10_000
        for ev in p.observe(channel=1, q15=q_hi, tick_us=tick) + p.observe(
            channel=2, q15=q_lo, tick_us=tick + 500
        ):
            assert_event_shape(ev)
            counts[ev["kind"]] += 1
        samples_seen += 2

    expect(p.occupied == 2, f"occupied={p.occupied} after two channels")
    expect(samples_seen == 400, f"samples_seen={samples_seen}")
    expect(counts["icon"] >= 1, f"no icons fired: {counts}")
    # Index/symbol counts are timing-sensitive; smoke asserts the shape
    # contract rather than a minimum count.

    print(
        f"OK — {samples_seen} samples observed, "
        f"icons={counts['icon']}, indices={counts['index']}, "
        f"symbols={counts['symbol']}"
    )
    return 0


def assert_event_shape(ev: dict[str, Any]) -> None:
    """Every event carries the five common keys plus kind-specific extras."""
    for k in ("kind", "tick_us", "channel", "intensity", "coherence"):
        expect(k in ev, f"event missing key {k!r}: {ev}")
    expect(
        ev["kind"] in {"icon", "index", "symbol"},
        f"bad kind {ev['kind']!r}",
    )
    expect(isinstance(ev["tick_us"], int), f"tick_us not int: {ev}")
    expect(isinstance(ev["channel"], int), f"channel not int: {ev}")
    expect(isinstance(ev["intensity"], float), f"intensity not float: {ev}")
    expect(isinstance(ev["coherence"], float), f"coherence not float: {ev}")
    if ev["kind"] == "icon":
        expect("entropy" in ev, f"icon missing entropy: {ev}")
        expect(isinstance(ev["entropy"], float), f"entropy not float: {ev}")
    else:
        expect("channels" in ev, f"{ev['kind']} missing channels: {ev}")
        expect(
            isinstance(ev["channels"], list) and len(ev["channels"]) == 2,
            f"channels shape: {ev}",
        )
        if ev["kind"] == "index":
            expect("delta_us" in ev, f"index missing delta_us: {ev}")


if __name__ == "__main__":
    sys.exit(main())
