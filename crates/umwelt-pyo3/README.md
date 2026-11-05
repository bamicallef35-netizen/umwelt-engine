# umwelt (Python bindings)

PyO3 bindings for the [Umwelt Engine](https://github.com/benmicallef/umwelt-engine) — a `no_std` Rust runtime that turns raw sensor streams into structured signs (Icon → Index → Symbol).

This package exposes the small-profile semiotic pipeline (4 channels, 64-sample window, 8-bin entropy histogram) to Python so notebooks can drive it end-to-end.

## Install (development)

```bash
pip install maturin
cd crates/umwelt-pyo3
maturin develop
```

`maturin develop` builds the Rust extension with the `extension-module` feature and installs it into your active virtual environment as `umwelt`.

## Build a wheel

```bash
maturin build --release
# → target/wheels/umwelt-*.whl
```

## Quickstart

```python
import umwelt

p = umwelt.Pipeline()
print(p.capacity, p.icon_window, p.icon_bins)  # 4 64 8

q = umwelt.q15_from_f32(0.75)
events = p.observe(channel=1, q15=q, tick_us=10_000)
for ev in events:
    print(ev["kind"], ev["tick_us"], ev["channel"])
```

See `scripts/pyo3_smoke.py` in the repo root for a longer example that drives two channels and inspects every event kind.

## License

Apache-2.0 (same as the rest of the workspace).
