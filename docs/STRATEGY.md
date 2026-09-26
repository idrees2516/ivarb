# Strategy Layer — Edge Model, Timing, Labs and Optimizers

How IVARB decides that a spread is an edge, how much of it to expect, when
to be patient, and how the parameters tune themselves.

## Units and conventions

- **Vol points (vp)**: 1 vp = 1% of implied volatility. All edges are
  quoted in vp; a 2.5 vp edge on $120/vega ≈ $300 per unit.
- **Vega**: $ per vp, normalized to one *underlying unit* (contract size
  divided out — a deribit BTC option and a bybit BTC option with different
  contract multipliers compare on the same basis).
- **Fee in vp**: `taker_fee_rate × spot ÷ vega_per_unit` — fees converted
  into the same unit as the edge, so `net_vp = spread_vp − fee_vp` is the
  honest, comparable signal.

## The edge model — executable premium truth

The naive approach — diff the two venues' quoted IVs — is wrong, and the
error is not subtle: venues mark IVs on their own forwards (bybit on the
futures curve ≈ 1.3% over spot; binance with an explicit 5.37% rate),
so mixing venue IVs fabricates 2–4 vp of "edge" that exists nowhere in
tradeable premiums. Verified live: cross-venue premiums agreed within $20
while quoted IVs differed by 2.6 vp.

IVARB therefore computes the edge from **the premiums you would actually
trade**:

```
edge_exec = (sell_bid − buy_ask) ÷ vega   (per unit, both in USD)
net_vp    = edge_exec × 100 − fee_vp_buy − fee_vp_sell
```

with vega computed by IVARB's own Black-Scholes at a consistent
forward/rate. Venue IVs are display context only. A mark-based mode
(`EdgeMode::Mark`) exists for venues without executable quotes and is
gated with a wider threshold multiplier.

Additional integrity gates: IV band 15–250 vp; dynamic moneyness band
(±1.5σ of tenor — 12% OTM is fine at 30 d, dust at 2 d); taker vega floor
$5/vp (wing noise); max spread 25 vp (fantasy quotes); cross-leg price
sanity band 0.5–2.0× (a stale $100 bid on a $9,800 option is not a
liquidity event); staleness windows (45 s stale / 180 s retire by
default).

## Time-aware strategy (tiers)

The same signal means different things at different clock positions. Two
axes:

1. **Theta bucket** (0–2 d, 3–7 d, 8–30 d, 30+ d): vega collapses and
   fees-in-vp explode as expiry approaches, so short buckets demand wider
   gates and shorter patience.
2. **UTC liquidity regime** (EU/US overlap, morning, late US, Asia quiet,
   weekend): thin books mean wider gates and smaller size.

The combination produces the **effective gate** (`net_vp ≥ base × bucket
× regime × age-decay`) and a tier decision:

| Tier | Meaning | Execution |
|---|---|---|
| SNIPER | decisive, decaying edge | both legs IOC, now |
| CONVERGENCE | mean-reverting edge | passive limits, requoted |
| MONITOR | below the effective gate | display only |

Each opportunity carries an **EV** (`fill_probability × net − single-leg
unwind cost − netted theta burn + drift hint`) — the scanner table has an
EV column, and the executor ranks on EV (route-calibrated), not raw
spread.

## Per-opportunity analytics

- **History ring**: spread samples over the opportunity's life → the
  detail panel's chart, plus 1-minute drift estimates per leg.
- **Kalman filter** on the spread level: de-noises the signal for gating.
- **OU process fit** (per opportunity, refreshed periodically): mean-
  reversion speed and stationary sigma produce the convergence hint
  (`conv_hint`, `conv_eta_s`) — the input to the CONVERGENCE tier
  decision and its patience window.

## Hold-to-expiry lab (section 04)

Some arbs are better held to expiry (both legs settle on intrinsic, the
spread is guaranteed by no-arbitrage) than closed on convergence. The lab
tracks near-miss candidates (edges just below the gate) and simulates
holding them: an OU bridge process (Halton quasi-random sampling) prices
the expected settlement value vs the theta burn and margin carry of
waiting, per theta bucket. The detail panel shows the hold-vs-close
decision block for any open candidate.

## Multi-hop arbitrage (section 05)

Two-venue spreads are the special case; the general problem is a directed
graph over (venue × option) nodes with edge weights = executable spreads.
A negative cycle = a closed chain of trades that locks profit. The engine
runs a Bellman-Ford-style negative-cycle search over every strike family
touched since the last pass (chunked: ≤120 families per pass, 250 ms
cadence), and publishes the routes with their locked-in vp and the legs
in execution order. Multi-hop routes with ≥3 legs face wider effective
gates (each additional leg adds fees and leg risk).

## Genetic optimizer (section 06)

The scanner's parameters (gate base, exit fraction, moneyness band, vega
floor, broadcast cadence, participation…) are tunable, and the right
values drift with regime. The GA:

- **Genome**: the engine's tunable parameters.
- **Fitness**: replaying the recorded signal tape (`data/signals.jsonl`)
  through a simulated executor and scoring realized capture net of costs
  and churn.
- **Loop**: own OS thread, crossbeam channels, one generation per
  `IVARB_GA_GEN_MS` (15 s default); status published to the UI
  (generation, best fitness, parameter convergence heatmap).
- **Application**: with `IVARB_GA_APPLY=true`, the best genome's
  parameters are applied live to the engine (bounds-clamped); every
  application is logged and reversible by restart.

Together with the executor's route calibration (per-venue-pair learning)
this closes the loop: the GA tunes *what to look for*, the calibration
tunes *what each route's sightings are worth*, and the paper broker
provides the ground truth for both.
