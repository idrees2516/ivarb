# API Reference

The server exposes one WebSocket, a REST surface, a Prometheus endpoint
and a liveness probe. All JSON is serde-generated from
`ivarb-protocol` — the same types the WASM frontend consumes.

## REST

| Method | Path | Returns |
|---|---|---|
| GET | `/api/snapshot` | Full `Snapshot`: venues, spots, opportunities (with history), holds, multihop, stats |
| GET | `/api/opps` | `Vec<Opportunity>` — active opportunities |
| GET | `/api/venues` | `Vec<VenueStatus>` — connection states, fees, contract sizes, lag |
| GET | `/api/holds` | `Vec<HoldCandidate>` — hold-to-expiry lab candidates |
| GET | `/api/multihop` | `Vec<MultiHopRoute>` — live multi-hop routes |
| GET | `/api/ga` | `GaStatus` — genetic optimizer state |
| POST | `/api/ga/cmd` | control the optimizer (`GaCommand`) |
| GET | `/api/exec` | `ExecSnapshot` — taker state (below) |
| POST | `/api/exec/cmd` | control the taker (`ExecCommand`, below) |
| GET | `/api/research/pulse` | research one-glance state + per-venue stats (see docs/RESEARCH.md) |
| GET | `/api/research/chain?und=BTC[&expiry=&right=&window=&limit=]` | consolidated cross-venue option chain (venue quotes carry `tick_age_ms` when a 50 ms-class feed covers the instrument) |
| GET | `/api/research/ticks?und=BTC[&limit=]` | **tick-level BBO plane**: fresh Paradex `order_book @15@50ms` snapshots + Derive v3 `ticker_slim @100ms` quotes — bid/ask + sizes, spread, IV, venue seq, exchange ts, per-quote age (ms) + per-feed health (connected / subs / frames / last-frame age) |
| GET | `/api/research/candles?und=BTC&interval=1h` | underlying candles + EMA/BB/RSI/ATR overlays + readout |
| GET | `/api/research/iv?und=BTC` | IV term structure, skew, percentile, cross-venue divergences |
| GET | `/api/research/liquidity?und=BTC` | strike × expiry resting-depth heatmap |
| GET | `/api/research/venues` | per-venue research stats (instruments, ATM IV, spread, OI, volume) |
| GET | `/api/research/expiries?und=BTC` | listed expiries `(yyyymmdd, iso, dte)` |
| GET | `/api/research/status` | research-plane health (digest, enrichment, klines, paradex — `paradex_quotes`, `paradex_ws`; tick plane — `paradex_book` + `paradex_book_subs` (50 ms book feed), `derive_ws` + `derive_subs` (v3 ticker feed), `tick_markets` + `tick_note`; llm) |
| POST | `/api/research/agent` | `{"query":"…"}` or `{und,view,horizon_days,risk}` → `AgentRun` (tool trace + grounded answer + structure cards) |
| POST | `/api/research/suggest` | `{und,view,horizon_days,risk}` → `SuggestionCard` (direct builder call) |
| GET | `/healthz` | `{"ok":true,"ts":…}` |
| GET | `/metrics` | Prometheus text exposition |
| GET | `/*` | static dashboard (`index.html` + WASM assets) |

## WebSocket `/ws`

On connect the server sends the full snapshot, then forwards engine
broadcasts. Server → client messages (internally tagged, snake_case):

```jsonc
{"type":"snapshot","snapshot":{ … Snapshot … }}
{"type":"opps","items":[ … Opportunity … ]}        // batched upserts; retired items removed
{"type":"venues","items":[ … VenueStatus … ]}
{"type":"stats","stats":{ … EngineStats … }}
{"type":"spots","items":[ … SpotQuote … ]}
{"type":"exec","exec":{ … ExecSnapshot … }}         // taker state, ≤750 ms cadence
{"type":"holds","items":[ … HoldCandidate … ]}
{"type":"multihop","items":[ … MultiHopRoute … ]}
{"type":"ga","status":{ … GaStatus … }}
```

Client → server: `{"type":"refresh"}` forces a full snapshot resend.
Lagged clients are automatically resynced with a fresh snapshot; the
server pings every 45 s of silence to keep proxies open.

## Key shapes (abridged)

### Opportunity

```jsonc
{
  "id": "ETH-20261016-2600-C:okx>binance",  // canonical instrument : buy>sell
  "underlying": "ETH", "expiry": "2026-10-16", "strike": 2600.0, "right": "Call",
  "buy_venue": "okx", "sell_venue": "binance",
  "iv_buy_vp": 55.0, "iv_sell_vp": 58.0,       // display context only
  "iv_spread_vp": 3.0, "fee_vp": 0.5, "net_vp": 2.5,   // the honest signal
  "edge_mode": "Exec",                          // Exec (touch) | Mark
  "vega_vp_usd": 120.0, "est_usd": 300.0,
  "contract_size": 1.0, "max_units": 5.0,
  "buy_px_usd": 164.0, "sell_px_usd": 180.0,    // executable touch premiums
  "spot": 2600.0, "t_years": 0.15,
  "drift_buy_vp_1m": -0.1, "drift_sell_vp_1m": 0.2,
  "tier": "Sniper", "bucket": "8-30d", "dte": 20.0,
  "gate_vp": 0.5, "ev_vp": 1.5, "theta_vp_day": -0.8,
  "patience_s": 30.0, "hold_s": 3600.0,
  "conv_hint": false,
  "stale": false, "retired": false
}
```

### ExecSnapshot

```jsonc
{
  "t_ms": 0,
  "stats": {
    "armed": true, "mode": "Paper", "kill": null,
    "attempts_total": 21, "attempts_active": 2, "positions_open": 5,
    "vega_used_usd": 812.0,
    "realized_usd": 240.5, "unrealized_usd": 88.2, "fees_usd": 12.4,
    "fills_total": 36, "single_leg_events": 1,
    "capture_ratio": 0.62, "trades_closed": 13,
    "equity_usd": 2000320.0, "net_deposits_usd": 2000000.0,
    "capital_reserved_usd": 4210.0, "capital_free_usd": 1995790.0,
    "roc_session_pct": 1.64,
    "gross_locked_usd": 1240.0, "exit_drift_usd": -310.0, "hedge_pnl_usd": 22.8,
    "wins": 9, "losses": 4, "profit_factor": 2.9, "avg_trade_usd": 18.5
  },
  "settings": { … ExecSettings … },
  "regime": { "utc_hour": 14, "label": "EU/US overlap", "gate_mult": 1.0, "size_mult": 1.0, "weekend": false },
  "positions": [ … Position … ],
  "trades": [ … Trade … ],
  "events": [ … ExecEvent … ],
  "balances": [["aevo", 250000.0], …],
  "deposits_by_venue": [["okx", 50000.0], …],
  "live": { "requested": false, "keys_present": false, "confirmed": false, "session_up": false, "route_ready": false, "live_gateways": 0, "blockers": [ … ] },
  "routes": [ { "route": "okx>binance", "attempts": 9, "aborts": 2, "closed": 5, "capture_ewma": 0.41, "abort_ewma": 0.22, "edge_mult": 0.41, "flicker": false } ]
}
```

### ExecCommand (POST /api/exec/cmd)

```jsonc
{"type":"arm","armed":true}
{"type":"kill","reason":"manual"}          // flatten + disarm
{"type":"reset_kill"}
{"type":"update_settings","settings":{ … ExecSettings … }}
{"type":"take","opp":"ETH-20261016-2600-C:okx>binance","units":null}  // units=null → risk-sized
{"type":"close","position":7}
{"type":"flatten_all"}
{"type":"deposit","venue":"okx","amount_usd":50000.0}
{"type":"withdraw","venue":"okx","amount_usd":10000.0}
{"type":"live_preflight"}
{"type":"live_smoke_test","instrument":"BTC-27JUN25-65000-C"}
```

Manual takes bypass the armed flag but respect every risk gate and the
live route-completeness rule; if the route's calibration multiplier is
below 0.85 a warning event is logged (never a block — the human decides).

## `/metrics`

Prometheus text format, ~30 gauge series prefixed `ivarb_` (engine) and
`ivarb_exec_` (taker). Full list and suggested alert rules in
[DEPLOYMENT.md](DEPLOYMENT.md).

## Conventions

- All timestamps are epoch milliseconds (`t_ms`, `recv_ms`, …).
- Money is USD; edges are vol points (vp); vega is $/vp per underlying
  unit.
- Venue ids: `deribit, okx, bybit, binance, delta, aevo, derive, thalex`.
- Canonical instrument id: `UND-yyyymmdd-strike-R` (e.g.
  `BTC-20260926-85000-C`); opportunity id appends `:buy>sell`.
