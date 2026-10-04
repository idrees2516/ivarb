# Live Trading on Real Exchanges & Real Chains

This document covers everything needed to move IVARB from paper fills to
**real execution on real venues and real chains**: the venue gateways
(Deribit / Bybit / OKX / Binance), the on-chain EVM gateway (Uniswap V3 on
Base / Arbitrum / Optimism / Ethereum), the safety doctrine that governs
both, and how every address, selector and signature scheme in this stack
was verified against production before being committed.

---

## 1. The safety model (read this first)

Three independent layers decide whether an order can ever leave this
process:

1. **The arming chain** (exchanges): `IVARB_MODE=live` **and** venue
   credentials **and** `IVARB_LIVE_CONFIRM=yes`. Any missing leg keeps the
   whole system in paper mode — a stray `IVARB_MODE=live` alone can never
   arm the book.
2. **The gateway doctrine**: in live mode, a venue without a working
   gateway *refuses* legs (zero fill) instead of simulating them. A mixed
   paper/live route is a lie, so it never executes: `VenueRouter::route_live_ok`
   checks every leg's venue before any route fires, and `live_route_ok`
   does the same for two-leg attempts.
3. **The signer gate** (chains): nothing broadcasts on-chain unless
   `IVARB_CHAIN_SIGNER_KEY` holds a raw private key. Without it the EVM
   gateway still quotes real pool prices through the on-chain `QuoterV2`
   and builds the exact ready-to-broadcast calldata (visible in the side
   panel with hover-to-copy), but no transaction is ever signed or sent.

In paper mode the system is bit-identical to before: the paper broker fills
every leg, deposits/withdrawals move simulated balances, and the auto-taker
arms itself at boot. In live mode nothing auto-arms; the ARM button is the
only way up, and the KILL switch + daily loss limit remain the way down.

---

## 2. Exchange gateways

| Venue | Auth scheme | Order path | Fill resolution | Perp hedge |
|---|---|---|---|---|
| deribit | WS `public/auth` (client credentials) | `private/buy`/`sell` limit IOC (coin-margined px = USD premium ÷ spot) | synchronous `order_state` / `avg_price` | — (paper/refused) |
| bybit | REST HMAC: `hex(hmac(secret, ts + key + recvWindow + body))` | `POST /v5/order/create` `category=option`, `orderType=Market` | create response, then `GET /v5/order/realtime` poll | `category=linear` `BTCUSDT` market |
| okx | REST HMAC: `base64(hmac(secret, iso_ts + METHOD + path + body))` + passphrase | `POST /api/v5/trade/order` `ordType=ioc` limit buffered ±0.5% through the touch | `GET /api/v5/trade/order?ordId=` poll (`accFillSz`/`avgPx`/`fee`) | `BTC-USDT-SWAP` market |
| binance | Query HMAC: `hex(hmac(secret, query))` appended as `&signature=` | `POST /eapi/v1/order` `type=MARKET` | `GET /eapi/v1/order?orderId=` poll (`cumQty`/`avgPrice`) | — (eapi has no perp; refused live, documented) |

Venue-native symbols are derived from the canonical instrument
(`BTC-20261030-85000-C`) per venue convention; contract sizes come from the
engine's live instrument metadata (never assumed), so quantities are always
converted per the venue's own lot size.

### Setup

```bash
# 1. Create API keys on each venue (trade scope, withdrawal OFF).
# 2. Put them in your environment (.env.example lists every variable):
export IVARB_MODE=live
export IVARB_LIVE_CONFIRM=yes
export DERIBIT_API_KEY=...     DERIBIT_API_SECRET=...
export BYBIT_API_KEY=...       BYBIT_API_SECRET=...
export OKX_API_KEY=...         OKX_API_SECRET=...   OKX_API_PASSPHRASE=...
export BINANCE_API_KEY=...     BINANCE_API_SECRET=...
./run.sh
```

At boot (and on every **LIVE PREFLIGHT** press) the router connects every
gateway whose credentials exist, pings the venue, and reports per-venue
health in the side panel's LIVE CONNECTIVITY card. Orders only flow once
the arming chain completes — preflight never places orders.

### Rehearsal

- **Deribit testnet**: `DERIBIT_WS_URL=wss://test.deribit.com/ws/api/v2`
  runs the entire live order path (auth, IOC buys/sells, fills) for free.
- **Smoke test**: the LIVE SMOKE TEST button round-trips one minimum-size
  Deribit IOC order (buy at the ask, immediately sell back at the bid) to
  prove the whole path for the cost of the spread plus fees.
- **Bybit/OKX/Binance testnets**: point `BYBIT_BASE_URL` / `OKX_BASE_URL` /
  `BINANCE_EAPI_BASE` at the venue's demo host to rehearse those paths.

---

## 3. On-chain execution (EVM / Uniswap V3)

The chain gateway speaks plain JSON-RPC over HTTPS — no indexer, no third
party, nothing to trust but the chains themselves.

**Default chains** (all values verified live before commit — see §5):

| chain | chainId | public RPC (override) | WETH | USDC |
|---|---|---|---|---|
| base | 8453 | `mainnet.base.org` (`IVARB_RPC_BASE`) | `0x4200…0006` | `0x8335…2913` |
| arbitrum | 42161 | `arb1.arbitrum.io/rpc` (`IVARB_RPC_ARBITRUM`) | `0x82aF…Bab1` | `0xaf88…5831` |
| optimism | 10 | `mainnet.optimism.io` (`IVARB_RPC_OPTIMISM`) | `0x4200…0006` | `0x0b2C…Ff85` |
| ethereum | 1 | `eth.drpc.org` (`IVARB_RPC_ETHEREUM`) | `0xC02a…Cc2` | `0xA0b8…eB48` |

**Quoting**: `QuoterV2.quoteExactInputSingle` via `eth_call` — the same
code path the router itself executes, so the displayed price is the
executable price (0.05% fee tier, the liquid WETH/USDC pool on every
default chain). The side panel chain chips show the live on-chain ETH
price and gas for all four chains, refreshed on every PREFLIGHT/REFRESH.

**Execution** (`EXECUTE` in the side panel, or `POST /api/exec/cmd` with
`{"type":"chain_swap", "chain":"base", "weth_in":true, "amount":0.05}`):

1. Quote the real pool for the exact input size.
2. Compute the slippage floor (`IVARB_CHAIN_SLIPPAGE_BPS`, default 50).
3. Check the signer's ERC-20 allowance to SwapRouter02; if short, build a
   real `approve` transaction first.
4. Build the `exactInputSingle` calldata (selector `0x04e45aaf`).
5. Sign an EIP-1559 typed transaction (hand-rolled RLP + secp256k1 via
   k256, y-parity from recoverable signing).
6. `eth_sendRawTransaction` — sequentially for approve + swap.

Without `IVARB_CHAIN_SIGNER_KEY` the flow stops after step 2 and returns
the real quote plus the ready-to-broadcast calldata; the UI marks it
`payload ready (no signer)`. With a key set, the swap broadcasts and the
transaction hash appears in the panel and the event log.

**Gas**: quoted once per swap (`eth_gasPrice`), max fee = 1.2× gas price,
priority = 0.1×. Gas limit is a conservative 240k (swap) / 60k (approve).

---

## 4. What runs where (integration map)

```
                       ┌──────────────────────────────────────────────┐
                       │                 VenueRouter                   │
 two-leg attempts ───▶ │  deribit WS │ bybit V5 │ okx v5 │ binance    │──▶ real orders
 multi-leg routes  ───▶ │  (armed? real : paper; ungated venue refuses) │──▶ paper fills
 position close    ───▶ │                                              │
 hedges            ───▶ │  perp: bybit linear / okx swap (live)        │
                       └──────────────────────────────────────────────┘
 chain swap        ───▶  EvmGate ──▶ QuoterV2 eth_call ──▶ SwapRouter02
                        (quotes always real; broadcast only with signer)
```

Everything downstream of the router is unchanged: the sequencer's
dependency ordering, the 10-second rule, mid-flight abandonment, partial
fill sizing, calibration, capital gates and the exit ladder all operate on
the `Fill` abstraction, whatever produced it. That is what makes the live
swap safe to reason about — the strategy code cannot tell (and does not
care) whether a fill came from the simulator or a venue.

---

## 5. Verification (how you can trust the constants)

Every magic number in the gateways was verified against production before
committing, and the tests pin them:

- **Selectors**: `keccak256` anchors (empty-string digest, ERC-20
  `transfer`/`approve`/`allowance`/`balanceOf`) prove the hash
  implementation; the QuoterV2 selector `0xc6a5026a` and SwapRouter02
  `exactInputSingle` selector `0x04e45aaf` were additionally **executed
  against all four chains** during development (see
  `scripts/verify_selectors.py` + `scripts/probe_univ3.py` in the repo
  history; the Rust test `quote_calldata_matches_live_vector` pins the
  exact 328-hex calldata that quoted live).
- **Contracts**: `eth_getCode` confirmed SwapRouter02
  (`0x68b3…Fc45`) on all four chains; the Base QuoterV2
  (`0x3d4e…B76a`) holds a real USDC fee balance (contract identity, not
  just existence).
- **Addresses**: WETH/USDC per chain verified by successful `allowance` /
  `balanceOf` dispatch.
- **Signing**: RLP encoding pinned to the Ethereum wiki spec vectors;
  EIP-1559 signing pinned to the canonical anvil test key #0 (address
  derivation `0xf39F…2266` proves keccak + secp256k1 are wired the
  Ethereum way); determinism + sighash-freshness asserted.
- **Exchange auth**: each venue's exact concatenation order is unit-tested
  (`venue_sign_shapes`), HMAC against RFC 4231 vectors, and each fill
  parser against the recorded response shape of its venue.
- **Doctrine**: `live_mode_refuses_ungated_venues` and the router tests
  assert that a live leg on an ungated venue refuses rather than simulates.

---

## 6. Operations

- **Side panel** (any page, the `‹` button): LIVE CONNECTIVITY card shows
  per-venue chips (green = gateway healthy, amber = credentials present
  but unhealthy, gray = none) and per-chain chips (live ETH price from
  on-chain quotes; hover for block/gas/RPC latency). PREFLIGHT re-checks
  everything read-only; the CHAIN SWAP form executes instantly.
- **Event log**: gateway connects, unhealthy venues, chain probes, swap
  outcomes (with transaction hashes) stream into the same log as every
  other execution event.
- **Metrics**: `/metrics` exposes `ivarb_exec_live_gateways` plus the
  existing executor series.
- **Rate limits**: public RPCs are shared — set `IVARB_RPC_*` to a private
  endpoint for production cadence. Exchange order rates are naturally
  bounded by the sequencer's cooldowns and concurrency caps.

## 7. Honest limitations

- Convergence-mode maker legs remain paper (the passive queue model); all
  taker legs (sniper, routes, closes) go live.
- Deribit perp hedging is not wired (its options are coin-margined; the
  WS path exists but perp sizing in USD needs its own conversion) — on
  deribit-only books, live hedges are refused and logged rather than
  faked. Bybit/OKX perps hedge for real.
- Live venue fee reconciliation is per-fill estimates (venue-reported fees
  preferred when available); end-of-day settlement files are the source of
  truth.
- Public chain status probes run on demand (boot, preflight, refresh), not
  on a fixed loop — deliberate, to stay inside public RPC rate limits.
