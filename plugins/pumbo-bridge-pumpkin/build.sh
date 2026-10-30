#!/usr/bin/env bash
# Builds PumboBridge for every supported Pumpkin release into dist/ at the
# workspace root:
#   dist/PumboBridge-26.3.wasm  -> Pumpkin 0.2.1+26.3-26.51     (Minecraft 26.3)
#   dist/PumboBridge-0.2.0.wasm -> Pumpkin 0.2.0+26.3-26.51     (Minecraft 26.3)
#   dist/PumboBridge-26.2.wasm  -> Pumpkin 0.1.0-dev+26.2-26.45 (Minecraft 26.2)
set -euo pipefail
cd "$(dirname "$0")/../.."
TARGET=wasm32-wasip2
PKG=pumbo-bridge-pumpkin
OUT=pumbo_bridge_pumpkin.wasm
TD="${CARGO_TARGET_DIR:-target}"
mkdir -p dist "$TD"

cargo build -p "$PKG" --release --target "$TARGET"
cp "$TD/$TARGET/release/$OUT" dist/PumboBridge-26.3.wasm

# Pumpkin 0.2.0 rejects a build against api 0.2.1 (and 0.2.1 one against
# 0.2.0), and Cargo holds only one of the two, so the lockfile is pinned to
# 0.2.0 for this build and put back afterwards.
cp Cargo.lock "$TD/Cargo.lock.keep"
trap 'cp "$TD/Cargo.lock.keep" Cargo.lock' EXIT
cargo update -q -p pumpkin-plugin-api@0.2.1 --precise 0.2.0+26.3-26.51
PUMBO_PUMPKIN=0.2.0 cargo build -p "$PKG" --release --target "$TARGET" --locked --target-dir "$TD/mc263-020"
cp "$TD/mc263-020/$TARGET/release/$OUT" dist/PumboBridge-0.2.0.wasm
cp "$TD/Cargo.lock.keep" Cargo.lock

cargo build -p "$PKG" --release --target "$TARGET" --no-default-features --features mc262 --target-dir "$TD/mc262"
cp "$TD/mc262/$TARGET/release/$OUT" dist/PumboBridge-26.2.wasm

# Each file names the api it was built against in its panic locations; a file
# with the wrong api would not load on its Pumpkin.
for f in 26.3:0.2.1+26.3-26.51 0.2.0:0.2.0+26.3-26.51 26.2:0.1.0-dev+26.2-26.45; do
  if ! grep -qaF "pumpkin-plugin-api-${f#*:}/" "dist/PumboBridge-${f%%:*}.wasm"; then
    echo "dist/PumboBridge-${f%%:*}.wasm is not built against pumpkin-plugin-api ${f#*:}" >&2
    exit 1
  fi
done
ls -l dist/PumboBridge-*.wasm
