#!/usr/bin/env bash
# scripts/verify.sh — the one gate every commit passes.
#
# If any check fails the script exits non-zero. CI runs the same script.
#
# Gates, in execution order:
#   1. cargo fmt --all --check         — formatting is not a bikeshed
#   2. cargo check --workspace --all-features
#   3. cargo check -p umwelt-core --no-default-features   (no_std gate)
#   4. cargo check -p umwelt-sep  --no-default-features   (no_std gate)
#   5. cargo test  --workspace
#   6. cargo clippy --workspace --all-targets -- -D warnings
#
# Ordering rationale:
#   - fmt is cheapest and most mechanical, runs first.
#   - check builds come next (fast; catch trivial type errors).
#   - the two no_std gates protect ADR-0008 invariants for the embedded targets.
#   - tests run before clippy so we see logic failures before style ones.
#   - clippy is last because it's the strictest and takes the longest to reason
#     about when red.
#
# Usage:
#   bash scripts/verify.sh
#
# Honors the pinned toolchain from rust-toolchain.toml (ADR-0006 / P0.5.2).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Source cargo env if present; harmless if already active.
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

BOLD=$'\033[1m'
DIM=$'\033[2m'
GREEN=$'\033[32m'
RED=$'\033[31m'
CLR=$'\033[0m'

step() {
    printf '%s▸ %s%s\n' "$BOLD" "$1" "$CLR"
}
ok() {
    printf '  %s✓ %s%s\n' "$GREEN" "$1" "$CLR"
}
fail() {
    printf '  %s✗ %s%s\n' "$RED" "$1" "$CLR" >&2
    exit 1
}

run() {
    local label="$1"
    shift
    step "$label"
    printf '  %s$ %s%s\n' "$DIM" "$*" "$CLR"
    if "$@"; then
        ok "$label"
    else
        fail "$label"
    fi
}

start_ts=$(date +%s)

run "fmt (cargo fmt --all --check)"          cargo fmt --all --check
run "check (workspace, all-features)"        cargo check --workspace --all-features
run "check (umwelt-core, no_std)"            cargo check -p umwelt-core --no-default-features
run "check (umwelt-sep,  no_std)"            cargo check -p umwelt-sep  --no-default-features

# P8.1 — embedded target gate. The `rust-toolchain.toml` pins
# `thumbv7em-none-eabihf` so rustup installs it automatically; if the
# target is missing (e.g. first run on a fresh machine before rustup
# has reconciled), we surface a guidance line instead of hard-failing
# the gate. Cross-compiling to a real ARM Cortex-M4F target catches
# no_std regressions that the host-triple no_std gates above cannot
# (like accidental `std::` imports hidden behind host libc).
if rustup target list --installed 2>/dev/null | grep -q '^thumbv7em-none-eabihf$'; then
    run "check (umwelt-core, thumb ARM)"     cargo check -p umwelt-core --no-default-features --target thumbv7em-none-eabihf
    run "check (umwelt-sep,  thumb ARM)"     cargo check -p umwelt-sep  --no-default-features --target thumbv7em-none-eabihf
else
    printf '  %s(skipped: thumbv7em-none-eabihf target not installed — run `rustup target add thumbv7em-none-eabihf` to enable)%s\n' "$DIM" "$CLR"
fi

run "test  (workspace)"                      cargo test  --workspace
run "clippy (workspace, -D warnings)"        cargo clippy --workspace --all-targets -- -D warnings

end_ts=$(date +%s)
printf '\n%s✓ all gates green in %ss%s\n' "$GREEN" "$((end_ts - start_ts))" "$CLR"
