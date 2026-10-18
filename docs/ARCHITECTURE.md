# Architecture

This document explains how IVARB is built: every crate's responsibilities,
the concurrency model, the data flow from exchange socket to browser frame,
and the design decisions that keep a full-book arbitrage engine inside a
single-digit-megabyte Rust binary with microsecond scan loops.

## The one-paragraph version

One process hosts five cooperating layers. **Venue connectors** (in the
engine crate) own their sockets/REST pollers and stream normalized quotes
into a single mpsc channel. The **engine core** task is the only writer of
shared state: it sweeps the quote tables, evaluates every cross-venue pair,
publishes wait-free snapshots through ArcSwap, and fans out deltas on a
broadcast bus. The **executor** core (also single-writer) ranks those
opportunities, risk-gates them, and spawns short-lived attempt tasks that
drive two-leg state machines against a paper broker or the Deribit live
gateway. The **server** (Axum) bridges everything to HTTP/WS and serves the
compiled **Leptos WASM** dashboard. The **quant** crate provides the pricing
math with zero dependencies so the browser can run the exact same calculator
as the engine.

## Crate map and dependency direction

```
            ┌───────────┐
            │ protocol  │  serde wire types only — no logic, no I/O
            └─────┬─────┘
       ┌──────────┼───────────────┐
       ▼          ▼               ▼
  ┌────────┐  ┌────────┐    ┌─────────┐
  │ quant  │  │ engine │    │ executor│   engine and executor both lean on
  └────┬───┘  └───┬────┘    └────┬────┘   quant; executor imports engine only
       │          │              │        for watch-bridge types
       └──────────┴──────┬───────┘
                         ▼
                    ┌────────┐        ┌──────────┐
                    │ server │───────►│ frontend │  (frontend also uses protocol)
                    └────────┘        └──────────┘
```

- **`ivarb-protocol`** — every type that crosses a process or thread
  boundary: `Opportunity`, `VenueStatus`, `ExecSnapshot`, `Trade`,
  `ServerMsg`/`ClientMsg`/`ExecCommand`, the calibration table. Serde derives
  only; compiled into the WASM frontend so server and browser agree on the
  wire format by construction.
- **`ivarb-quant`** — Black-Scholes price/vega/theta/delta (f64, no deps),
  a hybrid Newton+bisection implied-vol solver, taker-fee-to-vol-points
  conversion, per-venue short-option margin formulas, hold-to-expiry
  analytics (OU process, Halton RNG), and the comprehensive `CostSheet`
  model. Zero dependencies = usable from WASM without bloat.
- **`ivarb-engine`** — the market-data brain: 8 venue connectors, universe
  refresh, quote normalization, the scanner/strategy layer, time-aware
  tiering, OU/Kalman per-opportunity analytics, multi-hop negative-cycle
  search, hold-to-expiry lab, genetic optimizer (own OS thread), sim feed.
- **`ivarb-executor`** — the advantage taker: risk preflight (6 layers),
  SNIPER/CONVERGENCE state machines, paper broker with realistic fills,
  route-capture calibration, Deribit authenticated live gateway.
- **`ivarb-server`** — Axum router: `/ws` hub, REST API, Prometheus
  `/metrics`, static assets, JSONL audit writer, SIGTERM graceful shutdown.
- **`frontend`** — Leptos 0.8 CSR compiled by `trunk` to `wasm32`; renders
  the dashboard from the same protocol types.

## Concurrency model — "single writer, many readers"

Every shared structure follows the same pattern, which eliminates lock
contention from all hot paths:

| State | Owner (only writer) | Readers |
|---|---|---|
| Venue quotes, opportunities, stats | engine core task | ArcSwap snapshots (UI, executor, REST) |
| Positions, trades, risk budgets | executor core task | ArcSwap `ExecSnapshot` |
| Paper broker balances | `PaperBroker` (internal Mutex, cold path) | executor core |
| Optimizer status | GA thread (crossbeam) | ArcSwap `GaStatus` |

- Writers mutate plain owned state, then `store()` an immutable `Arc` —
  readers `load()` wait-free and never see torn state.
- The broadcast bus carries pre-serialized JSON strings (`Arc<String>`), so
  each engine update is serialized exactly once regardless of client count.
- Attempt FSMs are the exception: they run as short-lived tokio tasks and
  report back over a bounded channel, keeping the core's invariants intact.

## Data flow: quote → decision → order

```
 venue WS/REST poll (own task per venue, supervised reconnect)
   │  VenueMsg { inst, bid/ask/mark, sizes, iv?, ts }
   ▼
 engine core  ── sweep (default 250 ms, IVARB_SWEEP_MS)
   │  1. update quote tables (rustc-hash maps)
   │  2. for each strike family: pair best bid/ask across venues
   │  3. eval_dir: executable edge = premium diff ÷ one consistent vega
   │  4. gates: IV band, moneyness, vega floor, spread sanity, staleness
   │  5. hysteresis: first-seen → active → retired (with history ring)
   ▼
 broadcast bus ── Opps deltas (IVARB_BROADCAST_MS=1) + full snapshots (1 s)
   │                                   │
   ▼                                   ▼
 browser (WS)                    executor core ── burst scan
                                   │  candidates from opps_light (250 ms cache)
                                   │  rank: ev × route_mult × vega ÷ capital
                                   │  risk preflight (6 layers, below)
                                   ▼
                                 attempt task ── SNIPER or CONVERGENCE FSM
                                   │  paper broker fills (60–250 ms latency,
                                   │  depth partials, slippage) or Deribit IOC
                                   ▼
                                 position ── monitor (1 s): MTM, exits,
                                            delta hedge, theta budget
```

### The millisecond pipeline

Stage 3 added telemetry-driven latency work: every quote carries a venue
timestamp, and the engine reports `pipe_p50/p99/p999` (quote-to-snapshot) and
`wire_p50/p99` (arrival-to-broadcast) percentiles from HDR-style ring
buffers. The executor consumes `opps_light` — a light opportunity cache the
engine refreshes every analytics pass — and its scan tick (default 25 ms,
`IVARB_EXEC_SCAN_MS`) fires *immediately* when fresh engine news lands on
the broadcast bus (detected by a zero-parse string prefix check), so the
decision latency tracks the data, not a fixed timer.

## The engine crate in detail

### Venue connectors (`crates/engine/src/venues/`)

Each connector implements a small trait surface: discover the instrument
universe, subscribe/poll quotes, map to the normalized `VenueMsg`. The
framework provides supervised reconnects with exponential backoff and a
`Fatal` state for permanent configuration errors (auto-disabled with a note
published to the UI).

Venue-specific realities that had to be reverse-engineered and are now
encoded per connector:

- **Deribit** killed public WS tickers (err 13778) → REST book-summary
  polling; coin-margined premiums converted to USD per unit.
- **OKX** dropped option family subscriptions → per-instId `tickers` after
  an index-price wait; USD options are inverse (premiums in BTC).
- **Bybit** marks IVs on its own forward (≈ futures) — the scanner never
  mixes venue IVs (see "premium truth" below); margin token needs the
  trailing `-USDT` stripped.
- **Binance** unified its options market streams onto the futures host
  (`wss://fstream.binance.com/public`, `nbstream.binance.com` is dead) →
  top-5 books with sizes @100 ms (`{sym}@depth5@100ms`) sharded across
  connections (200-stream cap), all-underlying index via `!index@arr` on the
  `/market` root, and a gentle 60 s REST `/mark` for venue mark IVs. The old
  5 s `/mark`+`/ticker` polling burned ~600 request-weight/min against
  eapi's 400/min IP budget (the whole-market `/ticker` alone is weight 40),
  earning permanent 429 → 418 auto-bans; executable bid/ask IVs are now
  Black-Scholes solved from the 100 ms book instead.
- **Delta** right-first symbols (`C-BTC-83400-260926`) with DDMMYY dates
  and dict-wrapped responses.
- **Aevo** `/markets` needs a long timeout; per-instrument order books
  polled for the nearest expiries, ±5% ATM.
- **Derive** serves its chain RPC at `api.lyra.finance` — batched
  per-(currency × expiry) `get_tickers`, compact keys, 10-minute universe
  refresh.
- **Thalex** unreachable from this host — connector compiles, auto-disables.

### Premium truth (why venue IVs are display-only)

Venues quote IVs on their own forwards (bybit on futures ≈ 1.3% over spot,
binance with an explicit 5.37% rate, deribit near index). Naively diffing
venue IVs fabricates 2–4 vp "edges" that do not exist in tradeable
premiums — verified live: premiums agreed within $20 while quoted IVs
differed 2.6 vp. The strategy layer therefore computes the executable edge
as the **touch premium difference divided by one consistent vega**
(`eval_dir`), and venue IVs are demoted to display context. A cross-leg
price sanity band (0.5–2.0×) rejects stale/dust quotes.

### Scanner and strategy

- Incremental pair evaluation — O(#venues) per strike family per sweep, not
  O(n²): for each (underlying, expiry, strike, right) family the engine
  keeps the best bid and best ask across venues and evaluates only the
  crossing pairs.
- Every candidate carries a `HistoryPt` ring (spread over time), 1-minute
  drift estimates per side, a Kalman-filtered spread level and OU
  mean-reversion parameters — feeding EV, patience and convergence hints.
- Hysteresis lifecycle: opportunities must persist to be shown; they retire
  (and get tombstoned for cooldown) when the edge decays below the exit
  threshold.
- Sim feed fallback (`IVARB_SIM=auto`) keeps the whole pipeline functional
  (and demo-able) when no venue is reachable.

## The executor crate in detail

See [EXECUTION.md](EXECUTION.md) for the full treatment: the six risk-gate
layers, the SNIPER/CONVERGENCE state machines, fractional-Kelly sizing with
equity compounding, the exit ladder, residual-delta hedging, route-capture
calibration, and the Deribit live gateway with its triple environment gate.

## The server crate

- **Axum** router with two states: `AppState` (snapshot ArcSwaps + command
  senders). Every REST handler is a wait-free `ArcSwap::load()` + clone.
- **`/ws`**: sends the full snapshot on connect, then forwards broadcast
  frames; lagged clients are resynced with a fresh snapshot instead of
  being dropped; 45-second heartbeat pings keep proxies from idling out.
- **`/metrics`**: Prometheus text exposition — engine health/perf
  (venues live, loop µs, pipe percentiles, msg rate) plus taker P&L/risk
  (realized/unrealized, capture ratio, vega used, kill/armed states) —
  designed for 15–30 s scrapes and the alert rules in DEPLOYMENT.md.
- **Graceful shutdown**: SIGTERM/SIGINT flip a `watch` channel; engine,
  executor and GA observe it and exit cleanly; the JSONL audit writer
  flushes on drop.
- **JSONL audit**: every signal the engine logs (rate-limited) is appended
  to `data/signals.jsonl` — the same tape the genetic optimizer replays.

## The frontend

Leptos 0.8 client-side rendering compiled to WASM via trunk (~1.4 MB
gzipped). State flows one way: WS messages → reactive signals → views.
The calculator and the detail-panel cost sheets call the same `ivarb-quant`
functions the engine uses — the browser's "comprehensive cost" block and
the executor's capital gate can never disagree, because they are the same
code.

Sections: 00 topbar/venue strip, 01 scanner table (sortable, tier/EV/drift
chips, detail panel with history chart and execution plan), 02 advantage
taker (control bar, risk meters, positions, trades, execution log, route
calibration, strategy matrix), 03 funding & costs (treasury, deposit form,
funding rails, capital table by ROC, profit attribution), 04 hold-to-expiry
lab, 05 multi-hop, 06 genetic optimizer, 07 risks.

## Performance notes

- `rustc-hash` (`FxHashMap`) everywhere on hot paths; `mimalloc` as the
  global allocator.
- Engine scan loop: ~50 µs EWMA at ~3k msg/s ingestion.
- No allocation on the read path: readers clone `Arc`s, not vectors
  (REST handlers that return vectors clone once at the edge).
- The GA optimizer runs on a dedicated OS thread with crossbeam channels so
  generation fitness replays never stall the async runtime.
- Venue connectors parse with `serde_json` into stack-friendly structs;
  string interning (`key.rs`) gives `InstKey` a u64 hash identity for quote
  tables.

## Known limitations (honest engineering)

- Live execution is implemented for **Deribit** only; other venues' live
  gateways are future work (cross-venue live routes therefore refuse rather
  than fake a paper leg).
- Venue margin formulas encode public documentation defaults; live-mode
  users should verify against their actual account tier.
- The calibration table is in-memory (resets on restart); persistence is a
  natural next step.
