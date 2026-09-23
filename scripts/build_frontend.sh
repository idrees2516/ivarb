#!/usr/bin/env bash
# Build the Leptos WASM frontend into frontend/dist.
set -euo pipefail
cd "$(dirname "$0")/../frontend"
NO_COLOR=false ~/.cargo/bin/trunk build --release=true wasm32-unknown-unknown "$@"
echo "frontend dist:"
ls -la dist/ | head -10
