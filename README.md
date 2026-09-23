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

| Variable             | Default            | Meaning                          |
|----------------------|--------------------|----------------------------------|
| `IVARB_UNDERLYINGS`  | `BTC,ETH`          | underlyings to scan              |
| `IVARB_MIN_NET_VP`   | `0.5`              | entry threshold (vp)             |
| `IVARB_EXIT_VP`      | `0.25`             | retire threshold (hysteresis)    |
| `IVARB_STALE_MS`     | `45000`            | quote age → stale flag           |
| `IVARB_RETIRE_MS`    | `180000`           | quote age → remove               |
| `IVARB_VENUES`       | all                | comma list of venue ids to enable |
| `IVARB_SIM`          | `auto`             | `auto` / `always` / `off`        |
| `IVARB_BROADCAST_MS` | `250`              | delta push cadence               |
| `IVARB_DATA_DIR`     | `./data`           | audit log location               |
| `PORT` / `BIND`      | `3000` / `0.0.0.0` | server bind                      |

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
and expiry math.

## Extending to live execution

The engine is signal-only by design (public market data, no keys). To trade it:
add a `VenueExecutor` trait next to `VenueConnector` (auth + order placement per
venue), drive it from the 7-step flow (IOC limits at the touch, 10s legging
timeout, delta-hedge leg), and reuse `Opportunity` directly. The JSONL audit
log already records every signal lifecycle event for post-trade analysis.

## Disclaimer

Signal, not advice. Fee rates are indicative — verify on each venue. Trade only
with capital you can afford to lose.
