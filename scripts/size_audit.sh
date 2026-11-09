#!/usr/bin/env bash
# scripts/size_audit.sh — P8.2 size audit.
#
# Measures the aggregate machine-code footprint of the no_std core
# (umwelt-core + umwelt-sep + transitive no_std dependencies) on
# `thumbv7em-none-eabihf --release`, and asserts the budget from
# Phase P8 budget (<256 KB total .text + .data).
#
# ## Method
#
# The workspace release profile enables fat LTO, which asks rustc to
# emit LLVM bitcode into each .rcgu.o. llvm-size cannot read bitcode,
# so this script builds the audit artefacts under an *isolated* target
# directory (`target/size_audit/`) with a config override that turns
# LTO off and disables embedded bitcode. Rustc then produces native
# ARM ELF objects that llvm-size can sum.
#
# For each rlib cargo emits into the audit target tree, we extract its
# `.rcgu.o` members with llvm-ar and sum `.text + .data` via llvm-size.
# This is a worst-case archive-level sum — no cross-crate inlining, no
# linker dead-strip — so the real linked binary is always smaller than
# what we report.
#
# ## Usage
#
#   bash scripts/size_audit.sh           # text report + exit status
#   bash scripts/size_audit.sh --json    # machine-readable JSON
#
# Exit codes: 0 within budget, 1 tooling error, 2 over budget.

set -euo pipefail

TARGET="thumbv7em-none-eabihf"
BUDGET_KB=256
BUDGET_BYTES=$(( BUDGET_KB * 1024 ))
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AUDIT_DIR="target/size_audit"
cd "$REPO_ROOT"

JSON=0
if [[ "${1:-}" == "--json" ]]; then
    JSON=1
fi

# ── Locate llvm-size / llvm-ar from the rustup toolchain ────────────────
SYSROOT="$(rustc --print sysroot)"
HOST_TRIPLE="$(rustc -vV | awk '/host:/ {print $2}')"
LLVM_BIN="$SYSROOT/lib/rustlib/$HOST_TRIPLE/bin"
LLVM_SIZE="$LLVM_BIN/llvm-size"
LLVM_AR="$LLVM_BIN/llvm-ar"

if [[ ! -x "$LLVM_SIZE" || ! -x "$LLVM_AR" ]]; then
    echo "error: rustup llvm-tools missing; run:" >&2
    echo "    rustup component add llvm-tools-preview" >&2
    exit 1
fi

# ── Build artefacts with LTO + bitcode disabled ──────────────────────────
AUDIT_CONFIG="$(mktemp)"
trap 'rm -f "$AUDIT_CONFIG"' EXIT
cat > "$AUDIT_CONFIG" <<EOF
[target.${TARGET}]
rustflags = ["-C", "lto=off", "-C", "embed-bitcode=no", "-C", "linker-plugin-lto=off"]

[profile.release]
lto = false
EOF

if [[ "$JSON" -eq 0 ]]; then
    printf 'building no_std core for %s (isolated target dir, LTO off)…\n' "$TARGET"
fi

# Separate target tree so the audit build doesn't collide with
# `cargo test` / `cargo check` caches in `target/`.
CARGO_TARGET_DIR="$AUDIT_DIR" cargo build -p umwelt-sep \
    --release --target "$TARGET" --no-default-features \
    --config "$AUDIT_CONFIG" >/dev/null

DEPS_DIR="$AUDIT_DIR/$TARGET/release/deps"
[[ -d "$DEPS_DIR" ]] || { echo "error: $DEPS_DIR not found after build" >&2; exit 1; }

# ── Extract objects + tally sections ────────────────────────────────────
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"; rm -f "$AUDIT_CONFIG"' EXIT

total_text=0
total_data=0
declare -a ROWS=()

for rlib in "$DEPS_DIR"/*.rlib; do
    crate="$(basename "$rlib" .rlib | sed -E 's/^lib//; s/-[0-9a-f]{16,}$//')"
    objdir="$SCRATCH/$crate"
    mkdir -p "$objdir"
    (cd "$objdir" && "$LLVM_AR" x "$REPO_ROOT/$rlib" >/dev/null 2>&1 || true)

    # Collect every .o file llvm-ar emitted.
    objects=()
    while IFS= read -r -d '' f; do
        objects+=("$f")
    done < <(find "$objdir" -maxdepth 1 -name '*.o' -print0)
    [[ "${#objects[@]}" -eq 0 ]] && continue

    crate_text=0
    crate_data=0
    while IFS= read -r line; do
        # llvm-size -B berkeley columns: text data bss dec hex name
        set -- $line
        crate_text=$(( crate_text + $1 ))
        crate_data=$(( crate_data + $2 ))
    done < <("$LLVM_SIZE" -B "${objects[@]}" | tail -n +2)

    ROWS+=("$crate $crate_text $crate_data")
    total_text=$(( total_text + crate_text ))
    total_data=$(( total_data + crate_data ))
done

total=$(( total_text + total_data ))
pct=$(( (100 * total) / BUDGET_BYTES ))

# ── Render report ───────────────────────────────────────────────────────
if [[ "$JSON" -eq 1 ]]; then
    printf '{\n  "target": "%s",\n  "budget_bytes": %d,\n  "total_bytes": %d,\n  "text_bytes": %d,\n  "data_bytes": %d,\n  "percent_of_budget": %d,\n  "crates": [\n' \
        "$TARGET" "$BUDGET_BYTES" "$total" "$total_text" "$total_data" "$pct"
    for i in "${!ROWS[@]}"; do
        read -r name t d <<<"${ROWS[$i]}"
        sep=','
        [[ $i -eq $((${#ROWS[@]} - 1)) ]] && sep=''
        printf '    {"crate": "%s", "text": %d, "data": %d}%s\n' "$name" "$t" "$d" "$sep"
    done
    printf '  ]\n}\n'
else
    printf '\n%-28s %12s %12s\n' 'crate' '.text' '.data'
    printf '%s\n' '------------------------------------------------------'
    for row in "${ROWS[@]}"; do
        read -r name t d <<<"$row"
        printf '%-28s %12d %12d\n' "$name" "$t" "$d"
    done
    printf '%s\n' '------------------------------------------------------'
    printf '%-28s %12d %12d\n\n' 'TOTAL' "$total_text" "$total_data"
    printf 'target:  %s\n' "$TARGET"
    printf 'total:   %d bytes (.text + .data)\n' "$total"
    printf 'budget:  %d bytes (%d KB)\n' "$BUDGET_BYTES" "$BUDGET_KB"
    printf 'use:     %d%% of budget\n\n' "$pct"
fi

if [[ "$total" -gt "$BUDGET_BYTES" ]]; then
    echo "OVER BUDGET: $total > $BUDGET_BYTES" >&2
    exit 2
fi

[[ "$JSON" -eq 0 ]] && printf 'OK — within P8 size budget.\n'
exit 0
