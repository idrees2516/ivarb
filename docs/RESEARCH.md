# The Agentic Research System

Section **R** of the UI — a second data plane beside the arb scanner. Where
the scanner hunts mispricings between venues, the research system answers a
different question: **"given everything every venue is saying right now, what
option structure actually makes sense?"**

It ships in three layers: a consolidated live data plane, an analysis layer
(candles / indicators / vol structure / liquidity), and a tool-calling agent
that turns natural-language questions into grounded answers and concrete,
struck-from-live-quotes structures.

## Architecture

```
engine venues (deribit okx bybit binance delta aevo derive …)
        │  (existing live quote books, per venue)
        ▼
┌─────────────────────────────────────────────────────────────────┐
│ engine research tap  (every 4 s)                                │
│   bounded near-ATM digest: DTE 0–90, ±22% log-moneyness,        │
│   ≤5 expiries × ≤26 strikes × every venue                       │
└─────────────────────────────────────────────────────────────────┘
        │ ChainDigest (ArcSwap, wait-free)
        ▼
┌─────────────────────────────────────────────────────────────────┐
│ ivarb-research crate — the research engine                      │
│                                                                 │
│  • REST enrichment (30 s): Deribit book-summary, Bybit tickers, │
│    Binance eapi ticker, OKX tickers → OI + 24h volume per venue │
│  • klines (60 s): Binance spot candles 15m/1h/4h/1d × BTC/ETH/  │
│    SOL → EMA/BB/RSI/ATR/realized-vol overlays                   │
│  • books (25 s): Deribit/Binance/OKX depth for top strikes →    │
│    liquidity heatmap + best-venue routing                       │
│  • Paradex public WS (live): SBE markets_summary.ALL —          │
│    unauthenticated quotes+IVs+greeks+OI, ~9.5k markets          │
│    (crypto + legacy underlyings; universe REST every 10 min)    │
│  • consolidation (6 s): digest + enrichment → one cross-venue   │
│    book: consensus IV (median), best bid/ask routing, OI,       │
│    ATM term structure, skew, IV percentile, VR premium          │
│  • agent: 7 tools over the book, two interchangeable brains     │
└─────────────────────────────────────────────────────────────────┘
        │ REST  /api/research/*
        ▼
   Leptos WASM frontend — Section R
   chat · pulse · chains · charts · vol · liquidity · builder
```

Everything fails soft: a dead endpoint only means fewer numbers, never a
broken panel. If **no** live venue is reachable and the engine fallback is
dead too, a clearly-labeled synthetic book keeps the whole UI demonstrable
(`SIM DATA` badge — same philosophy as the arb scanner's sim feed).

## The agent (Vercel-AI-SDK style, in Rust)

A tool-calling loop with a registry of seven tools:

| tool | what it returns |
|------|-----------------|
| `market_pulse` | spot, 24h change, venues live, instruments, ATM IV, IV percentile, realized vol, put/call OI |
| `candles_indicators` | EMA trend, RSI-14, ATR-14, Bollinger position, realized vol |
| `option_chain` | nearest-expiry consolidated chain rows with best books |
| `iv_structure` | term structure, skew smile, VR premium, cross-venue divergences |
| `liquidity_map` | strike × expiry resting depth + best venue per leg |
| `venue_compare` | per-venue instruments / IV / spread / OI / volume |
| `build_structure` | a SuggestionCard struck from the live chain |

Two brains, selected at boot:

- **desk** (default, zero-config): a deterministic analyst. It parses the
  query (underlying, view, horizon, risk), plans the tool sequence, executes
  them against the live book, and composes an answer in which **every number
  comes from a tool result** — nothing is invented, ever.
- **llm** (optional): any OpenAI-compatible `/chat/completions` endpoint with
  function calling. Configure with:

  ```bash
  IVARB_LLM_BASE_URL=https://api.openai.com/v1   # or any compatible host
  IVARB_LLM_API_KEY=sk-...
  IVARB_LLM_MODEL=gpt-4o-mini
  IVARB_LLM_MAX_STEPS=6
  ```

  The LLM plans and narrates; the **tools still execute locally** against
  the live book, so answers stay grounded even with a generic model.

## The option builder ("create options in coherence with the market")

`build_structure` turns a (view, horizon, risk) request into a concrete
ticket:

1. **Regime read** — rich / cheap / fair vol from the variance risk premium
   and IV percentile.
2. **Strategy matrix**:

   | view \ vol | cheap | fair | rich |
   |-----------|-------|------|------|
   | bullish   | bull call spread | bull call spread | put credit spread |
   | bearish   | bear put spread | bear put spread | call credit spread |
   | range     | long call fly | iron condor | iron condor |
   | big move  | long straddle | long straddle | long strangle |

3. **Strike picking** — delta targets by risk appetite (0.18/0.25/0.35),
   liquidity-weighted, walking inward until a listed wing exists.
4. **Venue routing** — each leg goes to the venue with the best side
   (lowest ask for buys, highest bid for sells, sizes at the touch preferred).
5. **Numbers** — net debit/credit, max profit/loss and breakevens from a
   payoff grid, greeks from the same Black-Scholes core the arb engine uses,
   per 1 unit of underlying.
6. **Coherence score (0–100)** — does the structure *fit* the market?
   Premium stance vs vol regime (+15), trend vs view (+10), term slope vs
   vega stance (+5), leg liquidity (+10), RSI overextension (−10) — with a
   plain-language rationale citing the live numbers.

Cards appear in the chat, in the Builder tab, and both stay in sync. The
Builder's "Ask the agent to explain" button runs the full agent on the same
parameters.

## REST surface

| endpoint | purpose |
|----------|---------|
| `GET /api/research/pulse` | one-glance state + per-venue stats |
| `GET /api/research/chain?und=BTC[&expiry=&right=]` | consolidated chain (tick-fresh BBO where a 50 ms-class feed covers the instrument; ⚡n badges in the UI) |
| `GET /api/research/ticks?und=BTC[&limit=]` | **tick-level BBO plane** — every fresh Paradex 50 ms book quote and Derive v3 ticker for one underlying, with true exchange timestamps, per-quote ages, spreads and venue sequence numbers |
| `GET /api/research/candles?und=BTC&interval=1h` | candles + overlays + readout |
| `GET /api/research/iv?und=BTC` | term structure, skew, divergences |
| `GET /api/research/liquidity?und=BTC` | strike × expiry depth heatmap |
| `GET /api/research/venues` | venue stats |
| `GET /api/research/expiries?und=BTC` | listed expiries helper |
| `GET /api/research/status` | research-plane health (incl. `paradex_book`, `derive_ws`, `tick_markets`) |
| `POST /api/research/agent` | `{query}` or `{und,view,horizon_days,risk}` → AgentRun |
| `POST /api/research/suggest` | direct builder call → SuggestionCard |

## The tick-level BBO plane (50 ms class, probed live 2026-10-18)

Cross-venue routing decisions used to wait for the slowest feed on the path:
Paradex's 1.2 s market summaries and Derive's 4 s REST sweeps. The tick
plane replaces both with exchange-fresh best-bid/offer:

| feed | channel | cadence (measured) |
|------|---------|--------------------|
| **Paradex 50 ms book** | `order_book.{market}.snapshot@15@50ms` on the public unauthenticated SBE WebSocket | full ≤15-level **API-book snapshots** (RPI excluded) on every change, exchange-throttled to one frame per 50 ms per market — BTC-PERP ticks ~10 Hz, option books tick when quotes move |
| **Derive v3 ticker** | `ticker_slim.{instrument}.100` on `wss://api.derive.xyz/v3/ws` | best bid/ask **+ sizes**, mark, index, the venue's own IV surface (`option_pricing.i/bi/ai`) and 24 h stats on the exchange's 100 ms interval |

How it is wired:

- Both feeds write into one shared `TickPlane` keyed by canonical instrument
  (venue → canon → `TickBbo`), each quote carrying the exchange timestamp and
  a local receive time. Subscriptions are **reconciled incrementally** — the
  research loop recomputes the desired near-ATM market set every 45 s from
  the engine digest and each feed task subscribes new / unsubscribes gone
  markets with no reconnect and no data gap (123–150 markets per feed in
  production).
- **Stricter execution routing**: the consolidation loop overlays tick-fresh
  BBO onto the venue rows *before* best-bid / best-ask routing runs, so route
  decisions use 50 ms-class books wherever they exist. The chain browser
  badges routed rows with ⚡n (n = tick-fresh venues on that strike).
- **Derive v3 everywhere**: the same WS feed both overlays the engine's
  derive rows and *synthesizes* rows for strikes the digest's 26-nearest
  buckets crowd out (Derive's coarse round-strike grid), so Derive's chain
  stays complete on its own grid even when the engine venue hiccups. The
  engine's Derive connector itself was repointed to the v3 REST API
  (`api.derive.xyz/v3`) after the old `api.lyra.finance` v2 host began
  serving 503s on 2026-10-18 — same compact-key ticker payload, so the
  scanner's derive venue was restored unchanged.
- The UI's **Ticks 50ms** tab polls `/api/research/ticks` every 800 ms while
  open: feed chips (subs / frames / last-frame age), a live per-venue table
  (bid × size / ask × size / spread / IV / age in ms / venue seq), BUY chips
  on the lowest ask and SELL chips on the highest bid per strike — the
  exact inputs the router sees.

## Data sources (probed live 2026-10-13; tick feeds 2026-10-18)

| source | transport | what we take |
|--------|-----------|--------------|
| Deribit | public REST | chain quotes via engine, OI + volume via book-summary, order books |
| Bybit | public REST | chain quotes via engine, OI + volume via tickers |
| OKX | public REST | chain quotes via engine, volume via tickers, order books |
| Binance (eapi) | **public WS** `fstream.binance.com` | chain quotes via engine: **top-5 books @100 ms with sizes** (`{sym}@depth5@100ms`, `/public` root) + **all-venue index** (`!index@arr`, `/market` root); mark IVs via a gentle 60 s REST `/mark` (the old 5 s `/mark`+`/ticker` polling burned ~600 weight/min against the 400/min IP budget → permanent 418 auto-bans; `nbstream.binance.com` is dead — 404); bid/ask IVs are BS-solved from the live book, volume via ticker, order books via REST |
| Delta / Aevo | via engine | chain quotes |
| Derive v3 | public WS `ticker_slim` + engine REST v3 | tick-level BBO + sizes + venue IVs (WS), chain quotes via engine (`api.derive.xyz/v3`) |
| Paradex (TradeOptions) | public WS (SBE), unauthenticated | whole-exchange summaries via `markets_summary.ALL` (~9.5k markets incl. **legacy underlyings** TSLA/NVDA/US100/XAU…, ~8k frames/s) + **50 ms order-book snapshots** via `order_book.{market}.snapshot@15@50ms` for the near-ATM routing set |
| Binance spot | public REST | underlying candles |

### Unit conventions (verified, not assumed)

- Binance eapi prices are **per 1 unit of underlying** (BS-checked live:
  markPrice 645.6 at markIV 24.5% ≈ Black-Scholes 650). A historical bug
  divided them by the 0.01 lot quantum, inflating premiums 100× — fixed in
  the engine connector; this also explains the historical 76–100% binance
  sell-leg abort rates.
- Deribit OI is in underlying units (1 contract = 1 BTC).
- Bybit USDT-option contracts cover 1 BTC (verified via deep-ITM parity,
  carried over from the executor work).
- OI/volume USD marks are **estimates** (venue contract sizes) and are
  labeled as such in the UI.

## Honest limitations

- IV percentile starts *cross-sectional* (vs today's chain) and becomes
  time-series (own 24 h trailing window) after ~30 minutes of uptime.
- Liquidity depth is "within 2% of the touch" where books are reachable,
  else the quoted touch size — indicative, not settlement-grade.
- The desk brain is deterministic; it plans well for research-style queries
  but is not a general conversationalist — that's what the LLM brain is for.
- Builder prices are quotes at the touch; fees, slippage beyond the touch
  and margin are excluded (the exec-plane calculator covers those).
- Paradex's REST market data (bbo/orderbook/mark-price) is JWT-gated, and
  their JWTs expire every 5 minutes by design (non-extendable, per their
  docs). We therefore read Paradex through the **public WebSocket**
  (`markets_summary.ALL`, SBE binary): unauthenticated, reconnects with
  backoff, and carries venue-published IVs directly — one feed, no secrets.
  If the feed drops, quotes pause (marked stale) and resume automatically.
