#!/usr/bin/env bash
# Builds PumboBridge for every supported Pumpkin release into dist/ at the
# workspace root:
#   dist/PumboBridge-26.3.wasm -> Pumpkin 0.2.0+26.3-26.51     (Minecraft 26.3)
#   dist/PumboBridge-26.2.wasm -> Pumpkin 0.1.0-dev+26.2-26.45 (Minecraft 26.2)
set -euo pipefail
cd "$(dirname "$0")/../.."
TARGET=wasm32-wasip2
PKG=pumbo-bridge-pumpkin
OUT=pumbo_bridge_pumpkin.wasm
TD="${CARGO_TARGET_DIR:-target}"
mkdir -p dist

cargo build -p "$PKG" --release --target "$TARGET"
cp "$TD/$TARGET/release/$OUT" dist/PumboBridge-26.3.wasm

cargo build -p "$PKG" --release --target "$TARGET" --no-default-features --features mc262 --target-dir "$TD/mc262"
cp "$TD/mc262/$TARGET/release/$OUT" dist/PumboBridge-26.2.wasm

ls -l dist/PumboBridge-*.wasm
