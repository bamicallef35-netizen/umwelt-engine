# Umwelt Engine — Notebook examples

Python notebooks that drive the [`umwelt`](../../crates/umwelt-pyo3/) PyO3 extension end-to-end.

## Prerequisites

1. Install maturin into a virtual environment:
   ```bash
   python3 -m venv .venv
   source .venv/bin/activate
   pip install maturin
   ```
2. Build and install the `umwelt` extension:
   ```bash
   cd crates/umwelt-pyo3
   maturin develop
   ```
   After this, `python -c "import umwelt; print(umwelt.Pipeline)"` should succeed.
3. (Optional) Install JupyterLab to open the `.ipynb` files interactively:
   ```bash
   pip install jupyterlab
   jupyter lab examples/notebooks/
   ```

## Notebooks

- **`hyde_park_replay.ipynb`** — synthesises a short NDJSON sensor log matching the `umwelt.sensor_log.v1` schema used by `umwelt replay`, drives it through a small-profile `Pipeline`, and tabulates every Icon / Index / Symbol emission. No external datasets required; runs in seconds.

## Running headless

Every notebook can also be executed without JupyterLab:

```bash
jupyter nbconvert --to notebook --execute examples/notebooks/hyde_park_replay.ipynb \
  --output hyde_park_replay.executed.ipynb
```

Or skip jupyter entirely — the notebook cells are pure Python with no magics and can be pasted into an interpreter.
