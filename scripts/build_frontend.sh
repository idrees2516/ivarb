#!/usr/bin/env bash
# Build the Leptos WASM frontend into frontend/dist.
set -euo pipefail
cd "$(dirname "$0")/../frontend"
export PATH="$HOME/.cargo/bin:$PATH"
env -u NO_COLOR trunk build --release
echo "frontend dist:"
ls -la dist/ | head -10
