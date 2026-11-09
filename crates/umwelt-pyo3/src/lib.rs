//! PyO3 bindings for the Umwelt Engine (ADR-0002, P7.1).
//!
//! Exposes the small-profile semiotic pipeline (4 channels, 64-sample
//! icon window, 8-bin entropy histogram) and its `observe` callback
//! surface to Python. Designed as the minimum surface that lets a
//! notebook drive the pipeline end-to-end: construct, feed Q15 samples,
//! read back the emitted Icon/Index/Symbol events as dicts.
//!
//! This crate is **std-only** by construction — PyO3 itself depends on
//! std — and is intentionally a separate workspace member so that
//! `umwelt-core` and `umwelt-sep` preserve their `no_std` invariants
//! untouched. No code in this file is reachable from an embedded build.
//!
//! ## Python surface
//!
//! ```python
//! import umwelt
//! p = umwelt.Pipeline()
//! for tick, ch, q15 in samples:
//!     for ev in p.observe(ch, q15, tick):
//!         print(ev["kind"], ev["tick_us"], ev["channel"])
//! ```
//!
//! See `docs/decisions.md` § ADR-0002.

#![deny(unsafe_code)]
#![warn(missing_docs)]
// PyO3's proc-macro-generated error plumbing trips
// `clippy::useless_conversion` on every `#[pymethods]` item. The lint
// fires on code we did not write; silence it crate-wide rather than
// smear `#[allow(...)]` annotations across every method.
#![allow(clippy::useless_conversion)]

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use umwelt_core::replay::{PipelineEvent, SemioticPipeline};
use umwelt_core::Q15;

/// Compile-time channel count of the exposed pipeline (small profile).
const CHANNELS: usize = 4;
/// Compile-time icon-layer window width (samples).
const ICON_WINDOW: usize = 64;
/// Compile-time icon-layer entropy histogram bin count.
const ICON_BINS: usize = 8;

/// Python-visible wrapper around [`SemioticPipeline<4, 64, 8>`].
///
/// The small profile is the only shape exposed in P7.1; a medium
/// `<8, 128, 16>` variant is scheduled for a follow-up fire. See
/// ADR-0020 for the profile menu.
#[pyclass]
struct Pipeline {
    inner: SemioticPipeline<CHANNELS, ICON_WINDOW, ICON_BINS>,
}

#[pymethods]
impl Pipeline {
    /// Construct an empty small-profile pipeline.
    #[new]
    fn new() -> Self {
        Self {
            inner: SemioticPipeline::new(),
        }
    }

    /// Compile-time channel capacity of the underlying pipeline.
    #[getter]
    fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Number of distinct channels registered so far.
    #[getter]
    fn occupied(&self) -> usize {
        self.inner.occupied()
    }

    /// Icon-layer rolling-window width (samples). Fixed at construction.
    #[getter]
    fn icon_window(&self) -> usize {
        ICON_WINDOW
    }

    /// Icon-layer entropy-histogram bin count. Fixed at construction.
    #[getter]
    fn icon_bins(&self) -> usize {
        ICON_BINS
    }

    /// Feed one sample through the pipeline and return every
    /// [`PipelineEvent`] it emits as a list of Python dicts.
    ///
    /// `q15` is a raw Q15 value in `[-32767, 32767]`; use
    /// `umwelt.q15_from_f32` for the typical `float -> Q15` encode.
    /// `tick_us` is a monotonic microsecond timestamp.
    ///
    /// Each dict contains `kind`, `tick_us`, `channel`, `intensity`,
    /// and `coherence`. Index and Symbol events additionally carry a
    /// `channels` list (always sorted ascending) and, for Index, a
    /// `delta_us` field. Icon events additionally carry `entropy`.
    fn observe<'py>(
        &mut self,
        py: Python<'py>,
        channel: u16,
        q15: i16,
        tick_us: u64,
    ) -> PyResult<Bound<'py, PyList>> {
        let sample = Q15::from_raw(q15);
        // Buffer the emissions outside the callback so we can surface
        // PyErr cleanly without aborting the pipeline mid-observe.
        let mut buffered: Vec<PipelineEvent> = Vec::new();
        self.inner
            .observe(channel, sample, tick_us, |ev| buffered.push(ev))
            .map_err(|e| {
                pyo3::exceptions::PyRuntimeError::new_err(format!("pipeline error: {e:?}"))
            })?;

        let list = PyList::empty_bound(py);
        for ev in buffered {
            let d = event_to_dict(py, ev)?;
            list.append(d)?;
        }
        Ok(list)
    }
}

/// Serialise one [`PipelineEvent`] into a `{kind, tick_us, channel,
/// intensity, coherence, …}` dict. Common fields come first so dict
/// iteration order is predictable across event kinds.
fn event_to_dict(py: Python<'_>, ev: PipelineEvent) -> PyResult<Bound<'_, PyDict>> {
    let d = PyDict::new_bound(py);
    match ev {
        PipelineEvent::Icon(icon) => {
            d.set_item("kind", "icon")?;
            d.set_item("tick_us", icon.formed_at)?;
            d.set_item("channel", icon.channel)?;
            d.set_item("intensity", icon.magnitude.to_f32())?;
            d.set_item("coherence", 0.0f32)?;
            d.set_item("entropy", icon.entropy.to_f32())?;
        }
        PipelineEvent::Index(idx) => {
            d.set_item("kind", "index")?;
            d.set_item("tick_us", idx.formed_at)?;
            d.set_item("channel", idx.channels[0])?;
            d.set_item("intensity", 0.0f32)?;
            d.set_item("coherence", idx.coherence.to_f32())?;
            d.set_item("channels", idx.channels.to_vec())?;
            d.set_item("delta_us", idx.delta_us)?;
        }
        PipelineEvent::Symbol(sym) => {
            d.set_item("kind", "symbol")?;
            d.set_item("tick_us", sym.formed_at)?;
            d.set_item("channel", sym.channels[0])?;
            d.set_item("intensity", 0.0f32)?;
            d.set_item("coherence", sym.coherence.to_f32())?;
            d.set_item("channels", sym.channels.to_vec())?;
        }
    }
    Ok(d)
}

/// Encode an `f32` in `[-1.0, 1.0]` to its raw Q15 representation.
/// Clamps out-of-range input; matches `umwelt_core::Q15::from_f32`.
#[pyfunction]
fn q15_from_f32(x: f32) -> i16 {
    Q15::from_f32(x).raw()
}

/// Decode a raw Q15 bit pattern to `f32`. Matches
/// `umwelt_core::Q15::to_f32`.
#[pyfunction]
fn q15_to_f32(q: i16) -> f32 {
    Q15::from_raw(q).to_f32()
}

/// Python extension module entry point. Exposed as `umwelt` (per the
/// `[lib].name` in Cargo.toml); `import umwelt` lands here at load.
#[pymodule]
fn umwelt(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Pipeline>()?;
    m.add_function(wrap_pyfunction!(q15_from_f32, m)?)?;
    m.add_function(wrap_pyfunction!(q15_to_f32, m)?)?;
    // Surface the profile dimensions as module attributes so callers
    // can inspect them without constructing a Pipeline.
    m.add("CHANNELS", CHANNELS)?;
    m.add("ICON_WINDOW", ICON_WINDOW)?;
    m.add("ICON_BINS", ICON_BINS)?;
    Ok(())
}

// ── Rust-side tests ───────────────────────────────────────────────────────
//
// These exercise the binding crate's Rust API directly — no Python
// interpreter needed. The PyO3-level surface (import, method dispatch)
// is covered by P7.2's maturin smoke test against a built .so.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn q15_helpers_roundtrip_within_one_lsb() {
        for x in [-1.0f32, -0.5, 0.0, 0.25, 0.5, 1.0] {
            let q = q15_from_f32(x);
            let back = q15_to_f32(q);
            let lsb = 1.0f32 / 32767.0;
            assert!(
                (back - x).abs() <= lsb,
                "roundtrip off by {}: x={}, q={}, back={}",
                (back - x).abs(),
                x,
                q,
                back
            );
        }
    }

    #[test]
    fn q15_helpers_clamp_out_of_range() {
        assert_eq!(q15_from_f32(2.0), i16::MAX);
        assert_eq!(q15_from_f32(-2.0), -i16::MAX);
        assert_eq!(q15_from_f32(f32::NAN), 0);
    }
}
