#!/usr/bin/env bash
# Vercel static-frontend build for the IVARB dashboard (Leptos CSR -> WASM).
#
# Vercel's build image ships no Rust toolchain, so this script installs a
# minimal one plus the two helpers pinned to this repo's Cargo.lock
# (trunk 0.21.14, wasm-bindgen-cli 0.2.128), preferring prebuilt musl
# binaries (fast) with a cargo-install fallback (always works, slower).
# Output: frontend/dist/ — served by the "frontend" service in vercel.json.
#
# The trunk build compiles the workspace crates the frontend depends on
# (ivarb-protocol, ivarb-quant) via ../crates path dependencies, so the
# repository must be checked out in full (Vercel clones the whole repo).
set -euo pipefail
cd "$(dirname "$0")"

export RUSTUP_HOME="$HOME/.rustup"
export CARGO_HOME="$HOME/.cargo"
export PATH="$CARGO_HOME/bin:$PATH"

# 1. minimal Rust toolchain
if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
fi
export PATH="$CARGO_HOME/bin:$PATH"
rustup target add wasm32-unknown-unknown
mkdir -p "$CARGO_HOME/bin"

# 2. trunk 0.21.14 — MUST stay pinned; unlocked builds break on upstream
#    lightningcss/cssparser drift.
if ! command -v trunk >/dev/null 2>&1; then
  curl -fsSL https://github.com/trunk-rs/trunk/releases/download/v0.21.14/trunk-x86_64-unknown-linux-musl.tar.gz \
    | tar xz -C "$CARGO_HOME/bin" \
    || cargo install --locked trunk --version 0.21.14 --no-default-features --features rustls
fi

# 3. wasm-bindgen-cli pinned to the workspace Cargo.lock's wasm-bindgen
VER="$(grep -A1 '^name = "wasm-bindgen"$' ../Cargo.lock 2>/dev/null | grep version | head -1 | cut -d'"' -f2)"
VER="${VER:-0.2.128}"
if ! command -v wasm-bindgen >/dev/null 2>&1; then
  curl -fsSL "https://github.com/rustwasm/wasm-bindgen/releases/download/${VER}/wasm-bindgen-${VER}-x86_64-unknown-linux-musl.tar.gz" \
    | tar xz --strip-components=1 -C "$CARGO_HOME/bin" \
    || cargo install --locked wasm-bindgen-cli --version "$VER"
fi

# 4. build (env -u NO_COLOR: trunk 0.21.14's clap breaks when NO_COLOR=1
#    is exported, and Vercel exports it)
env -u NO_COLOR trunk build --release
echo "frontend dist ready: $(pwd)/dist"
