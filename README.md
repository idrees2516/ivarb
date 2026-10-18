# IVARB — Cross-Exchange Implied-Volatility Arbitrage Platform

**Pure Rust, end to end.** One binary ingests live option order books from eight
crypto exchanges, detects fee-adjusted implied-volatility dislocations,
risk-gates them, executes both legs, hedges the residual delta, manages every
position to convergence, and streams the whole state to a Leptos/WASM
dashboard — no Node, no Python, no browser-side crypto.

```
   deribit ─┐                                                       ┌─→ WS dashboard (Leptos WASM)
   okx      │    ┌─────────┐    ┌──────────┐    ┌──────────┐    ┌────┴───────┐
   bybit    ├───►│ engine  ├───►│ executor ├───►│  server  ├───►│ REST API   │
   binance  │    │ scan+   │    │ risk +   │    │  (Axum)  │    │ /metrics   │
   delta    │    │ rank    │    │ FSM      │    └──────────┘    └────────────┘
   aevo     │    └────┬────┘    └────┬─────┘
   derive   │         └── quant (Black-Scholes, IV solver, fees,
   thalex ──┘             margins, cost sheets) ──┘
```

| | |
|---|---|
| **Venues** | deribit, okx, bybit, binance, delta, aevo, derive, thalex (auto-disable on failure) |
| **Instruments** | ~5,000+ live options across BTC / ETH / SOL |
| **Edge model** | Executable-touch premium truth (forward-basis-proof), fee-adjusted vol points |
| **Execution** | Paper (realistic latency/slippage/partials) + live gateways: Deribit WS, Bybit V5, OKX v5, Binance eapi (triple-gated) + on-chain Uniswap V3 swaps on Base/Arbitrum/Optimism/Ethereum |
| **Research plane** | Agentic research system: consolidated cross-venue book, candles + indicators, IV analytics, liquidity heatmaps, Paradex TradeOptions universe (crypto + legacy underlyings), tool-calling agent (deterministic desk brain, optional OpenAI-compatible LLM brain), market-coherent option builder, **tick-level BBO plane** (Paradex 50 ms order-book snapshots + Derive v3 100 ms tickers) driving stricter execution routing |
| **Tests** | 121 unit/integration tests across quant, engine, executor, research |
| **Runtime footprint** | Single process, ~80 MB RSS, µs-level scan loop |

---

## What it does

1. **Scans** every option chain the venues publish, normalizes contracts to a
   common unit basis, derives IVs with its own Black-Scholes solver when a
   venue omits them, and ranks every cross-venue pair by *executable* edge —
   the premium you would actually pay/receive at the touch, in vol points,
   minus both venues' taker fees.
2. **Filters** with time-aware gates: theta bucket, UTC liquidity regime,
   signal age, quote staleness, moneyness sanity, vega floors. The same 0.8 vp
   signal is a SNIPER at 30 days and MONITOR-only at 1 day.
3. **Executes** the best opportunities automatically (paper mode auto-arms at
   boot): thinner leg first, both legs IOC, the 10-second leg rule with
   auto-unwind, passive CONVERGENCE limits when the dislocation is mean-
   reverting, fractional-Kelly sizing, per-venue capital checks.
4. **Manages** every position to convergence: take-profit, trailing locks
   (spread-level *and* USD-level ratchet), stops, theta budgets, max-hold,
   expiry guard, stale-quote guard — plus residual-delta hedging.
5. **Learns**: a per-route calibration table tracks what each venue pair
   actually banks of its displayed edge and how reliably it fills, and scales
   future firing decisions by the learned multiplier.
6. **Accounts** for every basis point: gross locked vs exit drift vs fees vs
   hedge P&L, capture ratio, profit factor, session ROC on peak capital,
   comprehensive all-in cost sheets and capital requirements per opportunity.

## Repository layout

```
crates/
  protocol/   Shared wire types (serde) — engine ⇄ server ⇄ WASM frontend
  quant/      Black-Scholes, Newton+bisection IV solver, fee math, margin
              formulas, hold-to-expiry analytics, cost sheets (zero deps)
  engine/     Venue connectors (WS/REST), normalizer, scanner/strategy,
              timing tiers, OU/Kalman analytics, hold lab, multi-hop,
              genetic optimizer, sim feed, research chain tap
  executor/   Advantage taker: risk preflight, SNIPER/CONVERGENCE state
              machines, paper broker, venue router (Deribit/Bybit/
              OKX/Binance live gateways + EVM chain gateway), route
              calibration, delta hedger
  research/   Agentic research plane: cross-venue consolidation, candles +
              indicators, IV analytics, liquidity maps, Paradex universe,
              tool-calling agent (desk / OpenAI-compatible LLM brains),
              market-coherent option builder
  server/     Axum HTTP/WS server, REST API, Prometheus /metrics, static
              frontend, JSONL audit writer, graceful shutdown
frontend/     Leptos 0.8 CSR dashboard → trunk → WASM (served by the server)
deploy/       systemd unit
docs/         Architecture and operations documentation (start below)
scripts/      build/rebuild helpers
```

## Quickstart

```bash
# One command — builds if needed, then serves the full stack on :3000
./run.sh                                 # http://localhost:3000

# Rebuild from scratch / run the test suite
./run.sh --rebuild
./run.sh --check
```

`run.sh` starts in **live-feed paper mode**: real venue data when reachable,
automatic simulation fallback when the host cannot reach the exchanges
(`IVARB_SIM=auto` — force one or the other with `IVARB_SIM=always|off`).
For a fully animated demonstration feed:

```bash
IVARB_SIM=always ./run.sh               # clearly banner-labeled SIM feed
```

The taker auto-arms in **paper mode** and starts taking every opportunity
that passes the risk gates — two-leg sniper/convergence cycles **and
multi-leg route cycles** (buy → convert → … → sell, flat at completion).
Simulated fills include latency, depth-aware partial fills, slippage and
fees.

```bash
# Docker (single image: server + dashboard)
docker compose up -d                     # http://localhost:3000
curl -s localhost:3000/healthz           # liveness
curl -s localhost:3000/metrics           # Prometheus
```

Configuration is environment-driven with sane defaults — see
[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for every knob, the risk profiles
(`IVARB_PROFILE=conservative|balanced|aggressive|sniper`), and the live-mode
arming procedure.

## Safety model

- **Paper by default.** No exchange keys, no orders — everything simulated.
- **Live requires three explicit gates**: `IVARB_MODE=live` **and**
  `DERIBIT_API_KEY`/`DERIBIT_API_SECRET` **and** `IVARB_LIVE_CONFIRM=yes`.
  Missing any one keeps the system in paper mode with the reason published to
  the UI. `ivarb-server --live-preflight` validates keys with zero risk.
- **Kill switch** (UI button or `POST /api/exec/cmd`) flattens everything and
  disarms; a daily-loss circuit breaker trips it automatically.
- **Route completeness**: live mode refuses any attempt whose other leg has no
  live gateway — a paper leg inside a live attempt is forbidden by design.

## Documentation

| Document | Contents |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Crate-by-crate internals, threading model, data flow, hot paths, wire protocol |
| [docs/RESEARCH.md](docs/RESEARCH.md) | The agentic research system: consolidated cross-venue data plane, tool-calling agent (desk + LLM brains), the market-coherent option builder, Paradex TradeOptions universe |
| [docs/EXECUTION.md](docs/EXECUTION.md) | The advantage taker: risk gates, state machines, sizing math, exits, hedging, calibration, live path |
| [docs/STRATEGY.md](docs/STRATEGY.md) | Edge model, time-aware tiers, EV, hold-to-expiry lab, multi-hop, genetic optimizer |
| [docs/COSTS-AND-CAPITAL.md](docs/COSTS-AND-CAPITAL.md) | Comprehensive cost sheets, margin formulas, funding rails, capital requirements, profit attribution |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Docker, compose, systemd, env reference, monitoring, ops runbook |
| [docs/API.md](docs/API.md) | REST + WebSocket reference |
| [docs/LIVE-TRADING.md](docs/LIVE-TRADING.md) | Real-venue gateways (Deribit/Bybit/OKX/Binance) + on-chain EVM/Uniswap execution: setup, safety model, verification |

## License

MIT — see [LICENSE](LICENSE).
