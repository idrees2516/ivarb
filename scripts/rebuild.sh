#!/usr/bin/env bash
# IVARB dev helper — rebuild everything and restart the platform dev server.
set -euo pipefail
cd "$(dirname "$0")"

# frontend → frontend/dist
( cd frontend && NO_COLOR=false trunk build --release=true )

# server (release)
cargo build --release -p ivarb-server

# restart via the platform dev script
if [ -f /home/z/my-project/.zscripts/dev.pid ]; then
  kill "$(cat /home/z/my-project/.zscripts/dev.pid)" 2>/dev/null || true
  sleep 1
fi
( cd /home/z/my-project && setsid bash .zscripts/dev.sh < /dev/null >> ivarb/data/server.log 2>&1 & )
echo "restarted; log: ivarb/data/server.log"
