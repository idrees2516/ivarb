# IVARB — Cross-Exchange IV Arbitrage Scanner

A pure-Rust implementation of the IV arbitrage scanner: live market data from
six options exchanges, fee-adjusted divergence detection, and a WASM-compiled
dashboard — no JavaScript application code anywhere.

```
┌─────────────────────────────  cargo workspace  ─────────────────────────────┐
│                                                                             │
│  crates/protocol   shared wire types (serde only, wasm-safe)                │
│  crates/quant      Black-Scholes · implied-vol solver · fee math (no deps)  │
│  crates/engine     venue connectors · arb strategy · history · sim feed     │
│  crates/server     Axum: /ws hub · REST · static · JSONL audit log          │
│  frontend          Leptos CSR → trunk → wasm32 → dist/ (served by server)   │
│                                                                             │
│  exchange WS/REST ──▶ connectors ──mpsc──▶ engine core ──▶ broadcast ──▶ ┐  │
│                            (supervised)    (single writer)               │  │
│                             ▲ ArcSwap snapshots ◀── 30s ──▶ REST/WS ◀───┘  │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Quick start

```bash
# toolchain
rustup target add wasm32-unknown-unknown
cargo install trunk wasm-bindgen-cli   # wasm-bindgen-cli must match Cargo.lock

# build frontend (→ frontend/dist)
cd frontend && trunk build --release

# run (serves UI + API on :3000)
cd .. && cargo run --release -p ivarb-server
```

The platform sandbox runs it through `.zscripts/dev.sh` (already configured).

## Endpoints

| Route           | What                                                        |
|-----------------|-------------------------------------------------------------|
| `/`             | the Leptos WASM dashboard                                   |
| `/ws`           | live feed: snapshot on connect, then incremental deltas      |
| `/api/snapshot` | full engine snapshot (venues, spots, opportunities, history) |
| `/api/opps`     | active opportunities                                        |
| `/api/venues`   | venue connection statuses                                   |
| `/healthz`      | liveness                                                    |
| `data/signals.jsonl` | append-only audit log of new/update/retire events      |

## The math

Vol points (vp) = 1% of IV. Vega is quoted in **USD per vol point per contract**.

```
fee_vp_leg  = (taker_fee_rate × spot) / vega_vp_usd
net_vp      = (iv_sell_bid − iv_buy_ask) − fee_vp_buy − fee_vp_sell
est_usd     = net_vp × vega_vp_usd            (per buy-venue contract)
total_usd   = est_usd × min(size_buy, size_sell)   (when sizes are published)
```

Signal gate: enter at `net_vp ≥ 0.5` (configurable), retire below `0.25`
(hysteresis), mark stale after 45s of quote silence, retire after 180s.

Sanity filters reject quote garbage before signaling: IV band 15–250 vp,
moneyness within ±12% of spot, vega ≥ $1/vp, spreads > 25 vp treated as stale
quotes.

## Venue support (verified live from this host)

| Venue   | Transport                                            | Notes                                                                 |
|---------|------------------------------------------------------|-----------------------------------------------------------------------|
| deribit | REST `book_summary_by_currency` poll (3s)            | Deribit's public WS tickers now require auth (err 13778); the poll carries native mark IV + bid/ask premiums (coin-margined, normalized to USD) |
| okx     | WS `tickers` per instrument + `index-tickers`        | 2026 API dropped `opt-tickers`; USD families are inverse (premiums in BTC → normalized); IVs derived by the engine solver |
| bybit   | WS `tickers.{symbol}` snapshot channel              | native markIv/bidIv/askIv; contract = 1 BTC (verified via deep-ITM parity) |
| binance | REST `eapi/v1/mark` poll (5s)                        | native markIV/bidIV/askIV (fractions, normalized)                     |
| delta   | REST `/v2/tickers` poll (5s)                         | native quote IVs + sizes; `contract_value` parsed per product          |
| aevo    | REST `/markets` poll (~10s)                          | 1.5 MB snapshot; slow over HTTP/1.1 — generous timeout applied         |
| derive  | experimental — auto-disabled (endpoint shape changed, 404) | re-enable where reachable                                             |
| thalex  | experimental — auto-disabled (unreachable from host)  | re-enable where reachable                                             |

If no venue is reachable, `IVARB_SIM=auto` (default) activates a realistic
simulation feed after 30s so the full stack stays demonstrable. `IVARB_SIM=always`
forces it. The UI shows a SIM banner whenever simulated data is in use.

## Configuration (env)

| Variable             | Default              | Meaning                                        |
|----------------------|----------------------|------------------------------------------------|
| `IVARB_UNDERLYINGS`  | `BTC,ETH,SOL`        | underlyings to scan (any venue-listed symbol)  |
| `IVARB_MIN_NET_VP`   | `0.5`                | entry threshold (vp)                           |
| `IVARB_EXIT_VP`      | `0.25`               | retire threshold (hysteresis)                  |
| `IVARB_STALE_MS`     | `45000`              | quote age → stale flag                         |
| `IVARB_RETIRE_MS`    | `180000`             | quote age → remove                             |
| `IVARB_VENUES`       | all                  | comma list of venue ids to enable              |
| `IVARB_SIM`          | `auto`               | `auto` / `always` / `off`                      |
| `IVARB_BROADCAST_MS` | `1`                  | WS delta push cadence (ms)                     |
| `IVARB_SWEEP_MS`     | `250`                | housekeeping cadence                           |
| `IVARB_SNAPSHOT_MS`  | `1000`               | full snapshot republish                        |
| `IVARB_MULTIHOP_MS`  | `250`                | multi-hop + analytics pass (chunked ≤120 fam.) |
| `IVARB_HOLDS_MS`     | `500`                | per-opp hold-to-expiry stats refresh           |
| `IVARB_GA_ENABLED`   | `true`               | genetic optimizer on/off                       |
| `IVARB_GA_APPLY`     | `true`               | best genome drives live gates                  |
| `IVARB_GA_GEN_MS`    | `15000`              | ms per GA generation                           |
| `IVARB_EXEC_SCAN_MS` | `1`                  | advantage-taker scan loop                      |
| `IVARB_DATA_DIR`     | `./data`             | audit log location                             |
| `PORT` / `BIND`      | `3000` / `0.0.0.0`   | server bind                                    |

## Performance design

- **Single-writer engine core** — connectors push over mpsc; the hot path is
  lock-free for readers (ArcSwap snapshots, broadcast fan-out).
- **Incremental detection** — a tick for (venue, instrument) re-evaluates only
  that instrument against the other venues: O(#venues) per update, no rescans.
- **Burst draining** — channel bursts are processed in batches to amortize
  wakeups (measured engine loop: ~58 µs EWMA at ~3k msg/s across 4.7k
  instruments).
- **Compact keys** — `(u16 und, u32 yyyymmdd, u64 strike×1e4, u8 right)` with
  FxHash; strike quantization makes cross-venue matching exact.
- **One IV model everywhere** — venue-native IVs when published; otherwise the
  engine derives them from executable prices with its own Newton+bisection
  solver, so every pair is compared apples-to-apples.
- **Executable-edge preference** — signals use the ask IV you would buy at and
  the bid IV you would sell at (`EXEC` mode); mark-based fallback is flagged.
- **mimalloc** global allocator on the server.
- **Millisecond pipeline telemetry** — quote-arrival → signal-computed and
  signal → serialized-WS-frame latencies are recorded into fixed 1 024-sample
  rings; the dashboard shows live p50/p99/p999 (typical: pipe p50 ≈ 30 µs,
  p99 ≈ 1 ms; wire p50 ≈ 15 µs at the 1 ms broadcast cadence).
- **Chunked analytics** — the multi-hop/hold/GA-tape pass processes at most
  120 strike families per 250 ms tick so the synchronous analytics never
  blocks the engine loop (routes from un-scanned families survive the pass).

## Hold-to-expiry lab (when the arb fails but the option is cheap)

Divergences below the fee-adjusted gate are not discarded: if the cheap leg's
ask still implies a lower vol than the cross-venue consensus, buying and
holding it to expiry can be a positive-EV outright long-vol position. The
engine tracks these near-misses (`/api/holds`, WS `holds` frames) and the lab
section prices the strategy end-to-end: premium outlay, entry fee, theta per
day (now / at half-life / near expiry), extrinsic as the maximum quiet-case
bleed, vol edge in vp, expected value per contract, break-even spot at expiry,
P(terminal P&L > 0) and a 4 000-path antithetic Monte-Carlo terminal P&L
distribution (p05/p50/p95/CVaR05) — computed in the browser from the same
Rust quant crate compiled to WASM. Every active scanner row also carries its
own hold view (with OU convergence ETA and Kalman spread-noise σ) in the
detail panel.

## Multi-hop arbitrage

Two-leg arbitrage needs a tradable bid on the rich venue and ask on the cheap
venue of the same option. Multi-hop removes that restriction: the engine
builds an inventory walk over (venue × call/put) leg templates — buy at an
ask, sell at a bid, convert call↔put on one venue via put-call parity with a
perp hedge — and records every combination that is flat at expiry (cross-venue
longs and shorts of the same right settle against each other) with positive
cash flow after every fee. Re-scanned every 250 ms on touched strike families
(`/api/multihop`, WS `multihop` frames); leg fees carry a GA-evolvable buffer.

## Genetic strategy optimizer

Twelve strategy parameters (entry-gate multiplier, exit fraction, patience,
max hold, Kelly fraction, fee buffer, netted-theta factor, staleness cap,
time-of-day gate shape, hold-lab EV gate, multi-hop fee buffer) form a genome.
Every signal episode is taped; a genetic algorithm (population 48, elites 3,
blend crossover, decaying Gaussian mutation, tournament selection) replays the
last 8 000 episodes per individual each generation and evolves for risk-adjusted
capture — the replay models fill probability, single-leg risk, theta burn, and
refuses to bank "convergences" inside the quote-noise band (phantom collapse →
forced unwind). The best genome can drive the live entry gates (`/api/ga`,
`POST /api/ga/cmd`: `pause` / `resume` / `set_applied` / `reset`).

## Strategy layer (capturing value)

- Fee-adjusted net edge with per-venue taker rates (indicative defaults,
  overridable per venue in `config.rs`).
- Hysteresis entry/exit to avoid flapping; stale-quote retirement.
- Liquidity-aware capacity: `total_usd = net × vega × min(quoted sizes)`.
- Lead-lag diagnostics per opportunity (which venue repriced last, 1-minute IV
  drift per leg) — the laggard is the side more likely to revert.
- Quote-garbage rejection (IV band, moneyness band, vega floor, max spread).

## Tests

```bash
cargo test --workspace
```

Covers BS pricing references, put-call parity, vega conversions, the spec's
worked fee example (0.03% × $65k / $1k = 0.0195 vp), IV-solver roundtrips
across 1%–300% vol, cross-venue symbol parsing (Deribit/OKX/Bybit/Binance/
Delta/Thalex/Derive formats incl. the `-USDT` margin suffix and DDMMYY dates),
expiry math, hold-to-expiry lab gates, multi-hop route detection (direct,
conversion, fee-dominated, stale), OU half-life recovery, Kalman filtering,
latency-histogram percentiles, and the GA replay model (high gates avoid junk
episodes).

## Extending to live execution

The engine is signal-only by design (public market data, no keys). To trade it:
add a `VenueExecutor` trait next to `VenueConnector` (auth + order placement per
venue), drive it from the 7-step flow (IOC limits at the touch, 10s legging
timeout, delta-hedge leg), and reuse `Opportunity` directly. The JSONL audit
log already records every signal lifecycle event for post-trade analysis.

## Disclaimer

Signal, not advice. Fee rates are indicative — verify on each venue. Trade only
with capital you can afford to lose.
