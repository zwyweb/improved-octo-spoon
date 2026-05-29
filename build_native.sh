#!/usr/bin/env bash
# Build BSX native library for Godot GDExtension (desktop/mobile).
# Requirements: Rust, cross (for cross-compilation)
set -euo pipefail

echo "── BSX Native Build ──"

TARGETS=(
  "x86_64-unknown-linux-gnu     linux.x86_64"
  "x86_64-pc-windows-gnu        windows.x86_64"
  "x86_64-apple-darwin          macos.x86_64"
  "aarch64-apple-darwin         macos.arm64"
  "aarch64-linux-android        android.arm64"
)

OUT="godot/addons/bsx/bin"
mkdir -p "$OUT"

for entry in "${TARGETS[@]}"; do
  TRIPLE=$(echo "$entry" | awk '{print $1}')
  LABEL=$(echo  "$entry" | awk '{print $2}')

  echo "  Building $LABEL ($TRIPLE)..."
  if command -v cross &>/dev/null && [[ "$TRIPLE" != *"$(rustc -vV | grep host | awk '{print $2}')"* ]]; then
    cross build --release --target "$TRIPLE" --no-default-features --features full --lib -p bsx 2>/dev/null \
      || { echo "  skip $TRIPLE (cross failed)"; continue; }
  else
    cargo build --release --target "$TRIPLE" --no-default-features --features full --lib -p bsx 2>/dev/null \
      || { echo "  skip $TRIPLE (cargo failed)"; continue; }
  fi

  EXT="so"
  [[ "$TRIPLE" == *windows* ]] && EXT="dll"
  [[ "$TRIPLE" == *darwin*  ]] && EXT="dylib"
  [[ "$TRIPLE" == *android* ]] && EXT="so"

  SRC="target/$TRIPLE/release/libbsx.$EXT"
  [[ "$TRIPLE" == *windows* ]] && SRC="target/$TRIPLE/release/bsx.$EXT"

  if [[ -f "$SRC" ]]; then
    cp "$SRC" "$OUT/libbsx.$LABEL.$EXT"
    echo "  → $OUT/libbsx.$LABEL.$EXT  ($(du -sh "$SRC" | cut -f1))"
  fi
done

echo "Done."
