#!/usr/bin/env bash
# Builds the Windows executable on Linux (cross-compiled with MinGW).
#   linux/build.sh          diagnostics build, as the fork's README builds it (the
#                           switches A/B picks and per-frame statistics read)
#   linux/build.sh release  release build
# Needs: rustup (the toolchain in rust-toolchain.toml), the
# x86_64-pc-windows-gnu target, and mingw-w64-gcc.
set -euo pipefail
cd "$(dirname "$0")/.."
T=x86_64-pc-windows-gnu
rustup target add "$T" >/dev/null 2>&1 || true
command -v x86_64-w64-mingw32-gcc >/dev/null || { echo "x86_64-w64-mingw32-gcc not found (install mingw-w64-gcc)" >&2; exit 1; }
bash linux/fetch-tables.sh
if [ "${1:-diag}" = diag ]; then args=(--profile diag --features diagnostics); out=diag; else args=(--release); out=release; fi
# VICTORY_NATIVE_CPU=1: code for this machine's CPU only (not portable).
[ -n "${VICTORY_NATIVE_CPU:-}" ] && export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=native"
cargo build "${args[@]}" -p victory --target "$T" --config "target.$T.linker=\"x86_64-w64-mingw32-gcc\""
echo "built: target/$T/$out/victory.exe"
