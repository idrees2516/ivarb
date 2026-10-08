# The Advantage Taker — Execution Internals

The executor turns scanner signals into risk-gated two-leg positions and
manages them to convergence. This document specifies every layer between
"the scanner shows an edge" and "the P&L is booked": candidate selection,
the six risk-gate layers, position sizing, the two execution state
machines, the exit ladder, delta hedging, the learning loop, and the live
path.

All of it lives in `crates/executor/`:

```
lib.rs      single-writer core: scan, monitor, accounting, publish
risk.rs     preflight gates + capital needs math
machine.rs  SNIPER / CONVERGENCE attempt state machines
paper.rs    fill simulator (latency, depth, slippage, fees, treasury)
calib.rs    route-capture calibration (learning loop)
live.rs     Deribit authenticated gateway + triple env gate
```

## Loop anatomy

The core task runs a `tokio::select!` over: commands (REST/UI), attempt
outcomes, event log writes, live-gateway messages, and three timers:

| Timer | Default | Purpose |
|---|---|---|
| scan | 25 ms tick, event-driven burst | rank candidates, fire attempts |
| monitor | 1 s | MTM, exit rules, hedges, circuit breaker |
| push | 750 ms | publish `ExecSnapshot` if dirty |

**Event-driven burst scanning.** The executor subscribes to the engine's
broadcast bus and marks data fresh whenever a snapshot or batched
opportunity delta arrives (detected by string prefix — zero parsing). The
scan tick fires immediately when data is fresh, with a 300 ms heartbeat
fallback. Candidates are read from the engine's `opps_light` cache (~250 ms
freshness) rather than the 1 s full snapshot — the decision latency tracks
the data, not the timer.

## Candidate selection (scan)

Each scan:

1. **Eligibility** — signal not stale/retired, tier is SNIPER or
   CONVERGENCE, adverse-drift hard skip (both legs running away from
   convergence), per-underlying concentration cap (default 2 — cross-venue
   arbs on the same underlying are one correlation cluster).
2. **Route calibration** — the venue pair's learned multiplier scales its
   displayed EV; quote-flicker pairs (≥8 attempts, >85% aborts) are skipped
   outright.
3. **Ranking by capital efficiency** — `score = ev_vp × route_mult ×
   vega_per_unit ÷ capital_per_unit`. Expected USD profit per USD of
   reserved capital: what a capital-constrained book actually wants to fire
   first. Best pair per instrument (dedup), sorted descending.
4. **Calibrated firing floor** — `ev_vp × route_mult > 0.05` must hold; a
   route that historically banks 40% of displayed edge needs 2.5× the
   displayed EV to clear the same bar.
5. **Capital budget** — at most 60% of free capital may be committed per
   sweep, ranked-best first; dry powder is kept for the next dislocation.
6. **Risk preflight** (below) → approved size → `spawn_attempt`.

## The six risk-gate layers

Evaluated in order; the first failure refuses the attempt with a
human-readable reason (surfaced in the UI log and REST):

1. **Global interlocks** — kill switch, armed flag, daily loss limit
   (realized + unrealized vs the UTC-day anchor).
2. **Capacity** — concurrent attempts, open positions, aggregate vega
   budget.
3. **Signal quality** — tier enabled; `net_vp ≥ gate_vp` (the time-aware
   effective gate); freshness; expiry ≥ 3 h away; per-instrument dedup
   (one position per canonical instrument); per-opportunity cooldown.
4. **Venue health** — both leg venues must be `Live` in the engine
   snapshot (no trading into a dead feed).
5. **Sizing** — `min(max_units, depth × participation, vega headroom ÷
   vega_per_unit, Kelly fraction × max_units)` × **equity compounding
   scale** (`equity ÷ boot equity`, clamped ×0.25–×3.0), floored to 0.01
   units (venue lot granularity).
6. **Capital** — segregated per-venue collateral: the buy venue must fund
   premium + fee, the sell venue must fund buffered short-option margin +
   fee. Sizes *down* to what balances afford; refuses below 0.01 units
   with a deposit-directed reason.

Fractional Kelly: `f = (ev / |net|) clamped to [0.05, 1.0] ×
kelly_fraction` (default 0.25). Equity compounding then scales the clip
with account growth — wins compound, drawdowns de-risk — while layer 6
remains the final authority on what balances can actually fund.

## Execution state machines (`machine.rs`)

### SNIPER — take the dislocation now

For decisive edges with decay drift. Sequence:

1. Fire the **thinner leg first** (less depth = more fragile) as IOC at
   the touch.
2. Immediately fire the second leg IOC.
3. **The 10-second rule**: if leg B is not filled within `leg_timeout_s`,
   cancel and unwind leg A — never hold a naked directional option.
4. Partial fills on leg B: shed leg A's excess at market so the book is
   flat, then proceed with the matched size.
5. Any unrecoverable single-leg state → unwind and report a
   `single_leg_event` (tracked in stats and `/metrics`).

Dust guard: at fire time, quotes that moved beyond a sanity band abort the
attempt before any order leaves (cheap abort beats expensive unwind).

### CONVERGENCE — let it come to you

For mean-reverting dislocations (the OU/Kalman analytics say the spread
oscillates). Passive limit orders on both legs lock ~60% of the signal
edge; 30-second requotes follow the market; a 20-second single-leg grace
window after one side fills, then market-take-or-unwind. Patience and hold
windows scale with the theta bucket.

## The exit ladder (monitor, 1 s)

Exit rules are evaluated on **fresh quotes only** (≤6 s staleness), and
keyed off the *signal* spread rather than the touch-locked entry, so
favorable passive fills never trigger instant churn. In evaluation order:

| Rule | Default | Logic |
|---|---|---|
| Stale quotes | > 25 s | hold through brief staleness; exit only on hard staleness |
| Manual / kill | — | `closing` flag or kill switch |
| Take profit | 25% of signal net | fee-aware target: waits for `net − entry fees − 10 bp` on thin edges |
| Trailing lock | ≥50% converged, gives back >30% | banks progress on the bounce instead of riding to the stop |
| **USD profit ratchet** | peak ≥ $25, gives back 45% | locks material paper profits that theta/exit-drift would round-trip |
| Stop | spread inverts to −1× net | the thesis is wrong |
| Theta budget | 60% of net burned | the clock ate the edge |
| Max hold | bucket-scaled (45 m … 72 h) | recycle the capital |
| Expiry | < 6 h left | manage away from pin/assignment risk |

Exit pricing sells the long leg at its venue's bid and buys back the short
at its venue's ask, with sanity bands (0.6–1.6× entry premium) against
dust/insane marks — the mark falls back to entry price rather than booking
a phantom loss, and a hard P&L clamp (±2× notional + premiums) is the
belt-and-suspenders layer on top.

## Residual-delta hedging

Both legs are the same strike/right, so deltas mostly cancel — but venue
mark conventions and IV differences leave a residual. When `|residual ×
spot| > hedge_threshold_usd` (default $250), the monitor trades the
underlying **on a third venue — deliberately neither option leg** — to
flatten it (`escape_cross_venue_hedge`), preferring the perp books in
liquidity order (bybit → okx → binance → delta → deribit, skipping any
venue that is already carrying a leg). The structure's delta therefore
never depends on a single venue's book: leg A on one exchange, leg B on a
second, the perp hedge on a third. Hedge P&L is tracked
separately (`hedge_pnl_usd`) so attribution stays honest.

## 0DTE escape — uncaptured-arb rapid exit

The exit ladder above assumes the arb *wants* to converge. When it does
not, waiting is just paying theta for a thesis the market rejected. The
escape system flips such positions into **same-day doctrine** — treat them
as if the option expired today: hedge first, get flat fast, salvage what
the structure still holds.

**Detection** (pure function `escape_trigger`, unit-tested). A position
flips when any of:

| Trigger | Default | Meaning |
|---|---|---|
| Spread reversed | ≥ entry + 1.0× signal, after a 10 s grace | the divergence widened — the move went against us (the grace window absorbs post-fill quote bounce) |
| Theta accelerating | ≥ 3× the budget schedule rate | decay is destroying edge faster than convergence can deliver it |
| No convergence | < 25% of signal converged at 35% of the hold window | the edge is not coming to us |

**The ladder** (`escape_step`, one phase per monitor beat, ~900 ms
hedge-fill confirmation dwell between phases, hard flat-by deadline of
45 s):

```
detect ──► hedge Δ on a THIRD venue ──► both legs out at market ──► hedge released, flat
 (phase 0)   (phase 1: full residual       (phase 2: touch/mark/entry        (trade booked
              neutralized before any         ladder, sane-banded prices,      ExitReason::Escaped0dte,
              leg moves)                     hedge P&L booked)                own stats bucket)
```

The order is the whole point: **delta is neutralized before any leg
moves**, so a failed arb can never decay into a directional bet during
its exit. Past the deadline the close is forced at any price — flat
beats price.

**Cross-venue delta neutrality.** The two option legs sit on two venues
by construction; the escape hedge goes to a *third* (perp-capable,
liquidity-ordered, gateway-checked in live mode), and so does the
incremental hold-period hedge. No single venue's book can gap the whole
delta-neutral structure. Sim-feed namespaces (`sim:bybit`) are normalized
so a venue is never picked "again" through a prefix mismatch, and paper
hedges book against the same account fabric as the positions.

**Settings** (UI-toggleable, env-overridable): `escape_enabled` (master
switch, `⟁ 0DTE ESCAPE` in the taker bar), `escape_patience_frac`,
`escape_min_progress_frac`, `escape_reversal_mult`,
`escape_deadline_s`, `escape_cross_venue_hedge`.

**Stats & attribution**: `escape_exits` / `escape_active` /
`escape_salvaged_usd` / `escape_avg_s` — the salvage number is honest
and frequently negative: a rapid exit of a failed capture usually gives
back part of the entry spread. That is the price of not riding a dead
thesis to the stop.

## Multi-leg route cycles (`route.rs`)

Where the two-leg FSMs monetize one contract priced differently on two
venues, route cycles monetize **chains**: buy a call on okx, convert it to
a put on the same venue via put-call parity (perp-hedged), sell the put on
bybit — every hop executable, the whole cycle flat at completion.

**Discovery** (`engine/multihop.rs`) — negative-cycle search over the
(venue × right) graph per strike family, re-scanned every 250 ms on
touched families. Routes are ranked by **expected profit**: raw profit ×
a data-confidence `quality` factor (quote freshness × depth confidence ×
two-sidedness). A fat printed edge resting on stale, thin or one-sided
books ranks below a modest edge on solid books — expectation is what the
taker chases. Hard theoretical bounds (call ≤ spot, put ≤ strike, +5%
headroom) reject venue-unit artifacts at the source; routes whose legs
publish no executable size are dropped (a cycle you cannot size is not an
opportunity).

**Sequencing** (`executor/route.rs`) — the cycle is one atomic batch of
taker fills with contingency at every step:

1. *Feasible hardest-first ordering* — a `convert` needs its source right
   on that venue, so the order respects data dependencies; among legs
   executable now, the thinnest book fires first.
2. *Signal re-validation* — the route must still exist and must not have
   decayed >40% since the scanner printed it.
3. *Generalized 10-second rule* — any leg whose quotes vanish or go stale
   fails the attempt; filled legs unwind in strict reverse order.
4. *Mid-flight abandonment* — before each leg the sequencer re-prices the
   remainder; if projected completion is negative AND worse than the
   unwind cost, the cycle is abandoned mid-route instead of completed.
5. *Partial-fill settlement* — the cycle size is the running minimum of
   filled legs; residual cross-venue imbalance is settled at market, and
   perp hedges carried through conversions are closed flat.

**Gates** (in the executor Core, before any capital moves): routes
enabled, armed, fresh quotes (≤6 s), expectation = profit × quality ×
learned capture ≥ `route_min_profit_usd` (default $2), not on cooldown,
per-venue capital at the sized amount (segregated, sized down to what the
tightest venue affords), and the pre-fire theoretical-bounds check. Live
mode runs no route cycles until every leg's venue has a live gateway — a
paper leg inside a live attempt is forbidden.

**Accounting** — a banked cycle books its round trip into the trade
blotter with per-leg detail (`Trade.legs`), the capture ratio
(realized ÷ signal-at-fired-size) feeds the route calibration EWMA, and
`ExecStats` carries `route_attempts / route_banked / route_profit_usd`.
The UI keeps it simple: one ROUTES toggle in the taker bar, two meters,
per-route quality chips and a single **TAKE THIS ROUTE** button.

## Route-capture calibration (`calib.rs`)

Every venue pair has its own reality — quote flicker, leg latency, depth
shape, settlement quirks. The calibration table replaces belief with
measurement:

- Each closed trade books its **capture ratio** (`realized_vp /
  signal_net_vp`) into a per-route EWMA (α = 0.30).
- Each aborted attempt books into an **abort-rate EWMA**.
- The learned **edge multiplier** = `clamp(capture_ewma, 0.05, 1.15)`
  (neutral below 3 closed trades), halved for abort-dominated pairs
  (≥8 attempts, >85% aborts), floored at 0.05 for systematically inverted
  routes (≥5 closes, negative capture).

The multiplier scales the route's displayed EV everywhere it matters:
scan ranking, the firing floor, and a warning on manual takes (manual is
warned, never blocked — the human keeps the last word). The table is
published in `ExecSnapshot.routes` and rendered as the Route calibration
panel (section 02), showing closed count, capture, abort rate, edge × and
a FLICKER badge for discounted pairs.

## Paper broker (`paper.rs`)

A venue is only as tradeable as its microstructure — the paper broker
simulates it so the whole pipeline (sizing, sequencing, the 10-second
rule, unwinds, hedging, exits) runs end-to-end without keys:

- **Taker fills**: 60–250 ms latency; up to 0.9 × displayed depth at the
  touch; slippage `2 bp + 8 bp × √(size/depth)` against us; beyond depth
  walks a second level 40% worse.
- **Maker fills**: no latency; fills when the opposite touch crosses the
  limit, or probabilistically per second when joined at the touch.
- **Fees**: `taker_rate × spot × units` per venue; maker legs free
  (Deribit/Derive-style economics).
- **Treasury**: per-venue balances with deposit/withdraw (overdraft
  refused), lifetime deposit tracking; equity and P&L are measured
  against *net deposits* so funding never inflates performance.

## The live paths (`live.rs`, `gateways/`)

The original Deribit scaffold grew into a multi-venue + on-chain execution
layer — see **[docs/LIVE-TRADING.md](LIVE-TRADING.md)** for the complete
guide. Summary of what executes for real:

- **Deribit** (WS, triple-gated): **Triple environment gate**: live mode requires `IVARB_MODE=live` *and*
  `DERIBIT_API_KEY`/`DERIBIT_API_SECRET` *and* `IVARB_LIVE_CONFIRM=yes`.
  Any missing piece keeps the system in paper mode with the blockers
  published to the UI. `DERIBIT_WS_URL=wss://test.deribit.com/ws/api/v2`
  rehearses on testnet.
- **Bybit V5 / OKX v5 / Binance eapi** (REST + HMAC): options market/IOC
  orders with per-venue fill polling; Bybit + OKX also hedge live through
  their USDT perps. A live leg on an ungated venue refuses (zero fill) —
  never simulated.
- **Chains** (Base/Arbitrum/Optimism/Ethereum): real Uniswap V3 quotes via
  QuoterV2 `eth_call`; with `IVARB_CHAIN_SIGNER_KEY` the side panel's
  CHAIN SWAP broadcasts real EIP-1559 approve+swap transactions through
  SwapRouter02.
- **`ivarb-server --live-preflight`** validates the full chain (gates,
  credentials, account) and exits — a zero-risk key check.
- **Order discipline**: private `buy`/`sell` IOC only; decimal snapping
  (`snap()` tick rounding at 1e10, `snap_amount()` scale-first rounding
  for 0.1-step amounts) because Deribit rejects non-tick prices — two
  real production bugs, now two regression tests.
- **Route completeness**: a live attempt needs a live gateway on *each*
  leg; with Deribit-only gateways, cross-venue attempts refuse with an
  explanatory event rather than faking the second leg on paper.
- **Smoke test**: a minimum-size order on a chosen instrument exercises
  the authenticated path end-to-end (UI button).

## Risk profiles

`IVARB_PROFILE` selects the operating point at boot (every gate, the kill
switch and the live triple-gate remain in force regardless):

| | conservative | balanced (default) | aggressive | sniper |
|---|---|---|---|---|
| Min edge | 1.0 vp | 0.5 vp | 0.35 vp | 0.8 vp |
| Kelly fraction | 0.15 | 0.25 | 0.40 | 0.30 |
| Concurrent attempts | 1 | 2 | 4 | 2 |
| Max open positions | 3 | 6 | 10 | 6 |
| Vega budget | $2k | $4k | $8k | $4k |
| Daily loss limit | $800 | $1,500 | $4,000 | $1,500 |
| Max participation | 25% | 35% | 50% | 45% |
| Convergence tier | on | on | on | **off** (IOC only) |

## Accounting and attribution

Every closed trade decomposes into:

```
realized = gross_locked − exit_drift − fees − hedge_pnl
```

- **gross_locked** — the premium spread captured at entry (`(sell − buy) ×
  units`): what the signal promised.
- **exit_drift** — what the market gave/took between entry and exit
  quotes: slippage, convergence shortfall, marks moving.
- **fees** — both legs, entry and exit.
- **hedge_pnl** — residual-delta hedging results.

Session stats add capture ratio (realized vs signaled edge), win rate,
profit factor, average trade, and ROC on peak reserved capital — the same
numbers exposed on `/metrics` for external monitoring.
