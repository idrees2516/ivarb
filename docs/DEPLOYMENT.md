# Deployment & Operations

IVARB ships as a single self-contained binary plus a static frontend —
four supported deployment shapes, one ops model.

## 1. Docker (recommended)

```bash
cp .env.example .env       # edit to taste — paper mode needs nothing
docker compose up -d       # builds backend + WASM frontend + runtime layer
```

- Image: multi-stage `Dockerfile` — `rust:1-slim` builder (server +
  trunk-built dashboard) → `debian:bookworm-slim` runtime with the
  binary, the static assets, an unprivileged `ivarb` user, and a
  `/healthz` healthcheck. ~90 MB.
- `docker-compose.yml` adds: restart policy, JSON log rotation,
  `no-new-privileges`, a named volume for the audit data
    (`IVARB_DATA_DIR=/var/lib/ivarb/data`).
- The container is read-mostly; secrets come from the env file (chmod
  600) or your secrets manager — never the compose file, never the
  image.

Verify: `curl -s localhost:3000/healthz` → `{"ok":true,…}`.

## 2. systemd (bare metal / VM)

```bash
cargo build --release --bin ivarb-server
(cd frontend && trunk build --release)
sudo cp deploy/ivarb.service /etc/systemd/system/
sudo mkdir -p /opt/ivarb /var/lib/ivarb
sudo cp target/release/ivarb-server /opt/ivarb/
sudo cp -r frontend/dist /opt/ivarb/www
sudo cp .env.example /etc/ivarb.env && sudo nano /etc/ivarb.env
sudo systemctl daemon-reload && sudo systemctl enable --now ivarb
```

The unit runs as a dedicated user, hardens the sandbox
(`ProtectSystem=strict`, `NoNewPrivileges`, `PrivateTmp`, …), restarts on
failure with backoff, and stops gracefully on SIGTERM (the server traps
it: attempts stop, watches release, audit flushes).

## 3. Ad-hoc

```bash
cargo build --release --bin ivarb-server
(cd frontend && trunk build --release)
IVARB_STATIC_DIR=./frontend/dist ./target/release/ivarb-server
```

## 4. Vercel (multi-service)

The repo root ships a `vercel.json` that deploys the whole system as one
Vercel project with **two services** behind shared routing:

| Service | Root | Build | Serves |
|---|---|---|---|
| `server` | `.` | `runtime: "container"` — the root `Dockerfile` (release build of the axum server; Vercel injects `PORT`, which the server already binds) | `/api/*`, `/ws`, `/healthz`, `/metrics` |
| `frontend` | `frontend/` | `frontend/vercel-build.sh` (installs a minimal Rust toolchain + pinned `trunk` 0.21.14 + `wasm-bindgen-cli` 0.2.128, builds the WASM dashboard) | static `dist/` on every other path |

Top-level rewrites own all public traffic — `/api/(.*)`, `/ws`,
`/healthz` and `/metrics` go to `server`, the catch-all `/(.*)` goes to
`frontend`. The routed service sees the **original request path** (a
request to `/api/exec` reaches the axum server as `/api/exec`), so the
server routes and the browser's same-origin `/ws` + `/api/...` calls work
unchanged.

Deploying:

1. Push this repo to GitHub and **Import Project** on vercel.com (or run
   `vercel` from the repo root). The root `vercel.json` is picked up
   automatically; both services build independently.
2. Set the environment variables from the reference below under
   Project → Settings → Environment Variables as needed (`IVARB_SIM`,
   profiles, live-mode gates). Paper mode needs none.
3. Expect the first build to take ~10–15 minutes per service (Rust
   toolchain + release compile); subsequent builds reuse the cache.

**No service bindings are declared**, by design: the dashboard is a static
WASM bundle that runs in the *browser* and talks to its own origin through
the public rewrites, and the server calls no other service in this
deployment (bindings resolve in server-side functions only — never in
static assets, never during builds). If you later add an SSR/BFF service,
declare `{ "type": "service", "service": "server", "format": "url",
"env": "SERVER_URL" }` on the *calling* service and read `SERVER_URL`.

Notes:

- `vercel dev` runs both services locally with the same routing table.
- `/ws` is a WebSocket upgrade proxied through the rewrite to the
  container service — after the first deploy, confirm the venue strip
  shows live feeds.
- The container's `IVARB_DATA_DIR` audit volume is ephemeral on Vercel
  (no persistent disk); run the JSONL tape on Docker/systemd if you need
  retention.

## Environment reference

Everything is optional; defaults are the tuned paper-mode values.

### Engine

| Var | Default | Meaning |
|---|---|---|
| `IVARB_UNDERLYINGS` | BTC,ETH,SOL | underlyings to scan |
| `IVARB_VENUES` | all 8 | venue enable list |
| `IVARB_MIN_NET_VP` | 0.5 | base signal gate (vol points) |
| `IVARB_EXIT_VP` | 0.25 | retirement threshold |
| `IVARB_STALE_MS` / `IVARB_RETIRE_MS` | 45000 / 180000 | signal freshness / retirement |
| `IVARB_SIM` | auto | `auto` (fallback) · `always` · `off` |
| `IVARB_BROADCAST_MS` / `IVARB_SWEEP_MS` / `IVARB_SNAPSHOT_MS` | 1 / 250 / 1000 | pipeline cadences |
| `IVARB_MULTIHOP_MS` / `IVARB_HOLDS_MS` | 250 / 500 | analytics passes |
| `IVARB_DATA_DIR` | ./data | JSONL audit log directory |
| `BIND` / `PORT` | 0.0.0.0 / 3000 | server bind |

### Executor

| Var | Default | Meaning |
|---|---|---|
| `IVARB_PROFILE` | balanced | `conservative` · `balanced` · `aggressive` · `sniper` (see EXECUTION.md) |
| `IVARB_AUTOARM` | 1 | auto-arm at boot in **paper** mode (live never auto-arms) |
| `IVARB_EXEC_SCAN_MS` / `IVARB_EXEC_MONITOR_MS` / `IVARB_EXEC_PUSH_MS` | 25 / 1000 / 750 | taker cadences (scan is event-driven burst) |

### Live mode (Deribit)

| Var | Meaning |
|---|---|
| `IVARB_MODE=live` | gate 1 of 3 |
| `DERIBIT_API_KEY` / `DERIBIT_API_SECRET` | gate 2 of 3 |
| `IVARB_LIVE_CONFIRM=yes` | gate 3 of 3 — the deliberate human decision |
| `DERIBIT_WS_URL` | default mainnet; `wss://test.deribit.com/ws/api/v2` for testnet rehearsal |

All three gates must hold simultaneously or the system stays in paper
mode with the blockers published to the UI. Validate first:

```bash
./target/release/ivarb-server --live-preflight   # exits 0/1, never orders
```

### Static assets

| Var | Default | Meaning |
|---|---|---|
| `IVARB_STATIC_DIR` | frontend/dist | dashboard directory (Docker: /opt/ivarb/www) |

## Monitoring

### `/metrics` (Prometheus)

Engine health/perf and taker P&L/risk in one scrape — 30+ series
including `ivarb_venues_live`, `ivarb_engine_loop_us_ewma`,
`ivarb_pipe_p99_us`, `ivarb_opps_active`, `ivarb_exec_armed`,
`ivarb_exec_realized_usd`, `ivarb_exec_capture_ratio`,
`ivarb_exec_vega_used_usd`, `ivarb_exec_kill`.

Suggested alerts:

```yaml
- alert: Iv arb venues degraded
  expr: ivarb_venues_live / ivarb_venues_total < 0.6
  for: 5m
- alert: Iv arb engine latency
  expr: ivarb_pipe_p99_us > 5000
  for: 5m
- alert: Iv arb kill switch tripped
  expr: ivarb_exec_kill == 1
- alert: Iv arb capture collapsing
  expr: ivarb_exec_trades_closed > 20 and ivarb_exec_capture_ratio < 0.2
  for: 15m
- alert: Iv arb down
  expr: up{job="ivarb"} == 0 or ivarb_up == 0
  for: 1m
```

### `/healthz`

Liveness probe (`{"ok":true,"ts":…}`) — used by the Docker healthcheck
and compose/systemd watchdogs.

### Logs

Structured `tracing` to stdout (`RUST_LOG` filters; `ivarb_engine=info`
is a good balance). JSONL signal audit under `IVARB_DATA_DIR` — the same
tape the genetic optimizer replays, so it doubles as forensic data.

## Ops runbook

| Situation | Action |
|---|---|
| Markets going crazy, want out | UI: KILL (flattens all, disarms) — or `POST /api/exec/cmd {"Kill":{"reason":"manual"}}` |
| One venue misbehaving | UI: venue strip shows state; restart with `IVARB_VENUES` minus the offender |
| Keys rotated | update env, `ivarb-server --live-preflight`, restart |
| Questionable fill data | `data/signals.jsonl` + the exec event log (both retained) |
| Version upgrade | `git pull && docker compose up -d --build` (or rebuild + `systemctl restart ivarb`) — paper state resets; live positions must be flat first |
| Memory pressure | ~80 MB RSS expected; if leaking, `ivarb_opps_active` and the event-log cap (240) bound the growth — capture `/metrics` history and restart |

## Security notes

- The server binds `0.0.0.0:3000` by default for container use; put it
  behind a reverse proxy with TLS for anything internet-facing, and
  restrict `/api/exec/cmd` (it can arm/disarm/deposit in paper mode).
- Secrets live only in the environment (or your secrets manager); nothing
  is persisted to the data directory except audit logs.
- The Docker image and systemd unit run as unprivileged users with
  `no-new-privileges`.
