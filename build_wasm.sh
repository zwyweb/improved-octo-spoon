#!/usr/bin/env bash
# BSX WASM — ULTRA FAST RUNTIME BUILD

set -euo pipefail

echo "── BSX WASM ULTRA FAST ──"

command -v wasm-pack >/dev/null || { echo "❌ wasm-pack not found"; exit 1; }
command -v wasm-opt  >/dev/null || { echo "❌ wasm-opt not found"; exit 1; }

RUSTC_VER=$(rustc -V)
echo "  rustc: $RUSTC_VER"

IS_NIGHTLY=0
if echo "$RUSTC_VER" | grep -q "nightly"; then
    IS_NIGHTLY=1
    echo "  ✓ nightly detected — enabling MIR opts"
fi

# ───────────────────────────────────────────────────────────────────────────────
# RUSTFLAGS
#
# NOTE: target-feature (+simd128 etc.) is intentionally set in
# .cargo/config.toml [target.wasm32-unknown-unknown], NOT here.
# Setting target-feature in env RUSTFLAGS also affects host proc-macro /
# build-script compilation, which silently breaks cross-compilation.
# ───────────────────────────────────────────────────────────────────────────────
export RUSTFLAGS="-C opt-level=3 -C codegen-units=1 -C debuginfo=0 -C panic=abort"

# Nightly-only: level-4 MIR opts (inlining, const-prop, copy-prop improvements).
if [ "$IS_NIGHTLY" = "1" ]; then
    export RUSTFLAGS="$RUSTFLAGS -Z mir-opt-level=4"
fi

echo "  RUSTFLAGS: $RUSTFLAGS"
echo "  Building wasm (release + LTO)..."

wasm-pack build bsx \
  --target web \
  --release \
  --no-default-features \
  --features wasm \

WASM_IN="bsx/pkg/bsx_bg.wasm"
WASM_TMP="bsx/pkg/bsx_bg.opt.wasm"

echo "  Pre-opt size:"
du -h "$WASM_IN"

# ───────────────────────────────────────────────────────────────────────────────
# WASM-OPT: Speed-focused multi-pass
#
#   -O4                        — max binaryen level (было -O3)
#   --enable-simd              — MUST match .cargo/config.toml target-feature
#   --enable-bulk-memory       — fast memset/memcpy
#   --enable-nontrapping-float-to-int
#   --precompute-propagate     — constant propagation through calls
#   --local-cse                — CSE for locals
#   --coalesce-locals          — reduce register pressure
#   --simplify-globals         — DCE + fold globals
#   --fast-math                — float reassociation / strength-reduction
#   --converge                 — re-run until stable
# ───────────────────────────────────────────────────────────────────────────────
echo "  Running wasm-opt (speed mode, -O4 + extra passes)..."

wasm-opt "$WASM_IN" \
  -O4 \
  --fast-math \
  --enable-simd \
  --enable-bulk-memory \
  --enable-nontrapping-float-to-int \
  --enable-sign-ext \
  --enable-mutable-globals \
  --enable-reference-types \
  --enable-multivalue \
  --precompute-propagate \
  --local-cse \
  --coalesce-locals \
  --simplify-globals \
  --converge \
  -o "$WASM_TMP"

mv "$WASM_TMP" "$WASM_IN"

echo "  Final size:"
du -h "$WASM_IN"

echo "── DONE ──"
echo "✔ ultra-fast wasm ready"
