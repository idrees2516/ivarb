#!/usr/bin/env bash
# IVARB — one-command launcher.
#
#   ./run.sh            build if needed, then serve the full stack (engine +
#                       executor + UI) on :3000 — the preview URL just works
#   ./run.sh --rebuild  force a full rebuild (server + WASM frontend)
#   ./run.sh --check    build + run the test suite, then exit
#
# Everything is safe by default: paper mode, auto-armed taker, simulated
# fills with realistic latency/slippage/fees. Going live requires
# IVARB_MODE=live + DERIBIT_API_KEY/SECRET + IVARB_LIVE_CONFIRM=yes.
set -euo pipefail
cd "$(dirname "$0")"

export PATH="$HOME/.cargo/bin:$PATH"
export RUST_LOG="${RUST_LOG:-info}"
export IVARB_SIM="${IVARB_SIM:-auto}"
export IVARB_STATIC_DIR="${IVARB_STATIC_DIR:-frontend/dist}"

NEED_BUILD=0
[[ "${1:-}" == "--rebuild" ]] && NEED_BUILD=1
[[ ! -x target/release/ivarb-server ]] && NEED_BUILD=1
[[ ! -f frontend/dist/index.html ]] && NEED_BUILD=1

if [[ "$NEED_BUILD" == 1 ]]; then
  echo "== building server (release) =="
  cargo build --release -p ivarb-server
  echo "== building frontend (wasm) =="
  (cd frontend && env -u NO_COLOR trunk build --release)
fi

if [[ "${1:-}" == "--check" ]]; then
  echo "== running tests =="
  cargo test --workspace --exclude ivarb-frontend
  echo "ALL GREEN"
  exit 0
fi

echo "== IVARB starting on :3000 (paper mode, auto-armed) =="
exec ./target/release/ivarb-server
