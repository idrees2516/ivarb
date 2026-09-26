# Costs, Capital and Profit Attribution

Every dollar between "the scanner shows 2.5 vp" and "the account is up
$300" is modeled, computed by one shared code path (`ivarb-quant::cost`),
and displayed three places (detail panel, capital table, executor gate) so
the numbers can never disagree.

## The comprehensive cost sheet

For any opportunity, `CostSheet` computes the all-in cost of one
round trip:

| Cost | Model |
|---|---|
| Entry taker fees | both legs: `rate × spot × units` per venue |
| Exit taker fees | both legs again (closing is a round trip) |
| Touch slippage | the paper broker's model: `2 bp + 8 bp × √(size/depth)` per leg, both ways |
| Margin carry | 5%/yr on the short leg's posted margin for the expected hold |
| Theta burn | netted at 0.15 (a hedged arb keeps only the residual of the two legs' decay) over the expected hold |
| Funding round-trip | deposit + withdrawal rail costs for the capital actually used |

The UI renders this as a 12-line sheet: gross locked edge, each cost line,
net after all costs, and the honest verdict — including *negative* nets on
marginal arbs (the sheet refusing to flatter marginal trades is the
feature, not a bug).

## Margin formulas (short leg, per venue)

Posted margin per contract, from venue documentation:

| Venue | Formula (per contract) |
|---|---|
| deribit | `max(0.15·mark + 0.08·spot − OTM, 0.10·spot · factor)` |
| bybit | `max(0.125·mark + 0.05·spot − OTM, 0.05·spot)` |
| default | `max(0.15·mark + 0.06·spot − OTM, 0.10·spot)` |

where `OTM = max(K−S, 0)` for calls and `max(S−K, 0)` for puts (the
classic sign error — inverted OTM — was caught by unit test). A 20%
buffer (`MARGIN_BUFFER`) is applied on top because venues add
concentration and stress add-ons.

## Capital requirements

`CapitalReq` per opportunity, per unit:

```
buy venue:  premium + entry fee                    (funds the long leg)
sell venue: margin × 1.2 buffer + entry fee        (posts the short leg)
total per unit = buy_need + sell_need
ROC = net_after_all_costs ÷ total_per_unit
```

The capital table (section 03) ranks the top opportunities by ROC and
shows the per-venue split with *funded* badges against live balances —
so a $685/unit deribit-short route next to a $410 binance-short route
is visibly a different trade even at identical vp.

## The capital gate

Layer 6 of the executor's preflight enforces segregation: the buy venue's
balance must fund its side, the sell venue's balance must fund its side.
Behavior:

- Well funded → approved at the Kelly/participation size.
- Partially funded → **sized down** to what the balances afford (a $100
  balance on a $685/unit venue still trades 0.14 units).
- Below one hundredth of a unit → refused with
  *"insufficient capital on X: have $…, one unit needs $… — deposit
  funds in the Funding & capital panel"*.

Per scan, a **capital budget** (60% of free equity) keeps dry powder;
within a sweep, ranked-best-first allocation decides who gets it.

## Funding rails (all eight venues)

The funding panel's reference cards — assets, networks, approximate fees
and credit times — are compiled from venue documentation:

| Venue | Assets | Notable rails | Fee ballpark | Credit |
|---|---|---|---|---|
| deribit | BTC, ETH, USDC, USDT | native + Omni/ERC20/Arbitrum | free–$5 | 1–20 conf |
| okx | BTC, ETH, USDT, USDC | Omni, ERC20, TRC20, Arbitrum, Optimism, Polygon | free–$1 | 1–60 conf |
| bybit | BTC, ETH, USDT, USDC | Omni, ERC20, TRC20, Arbitrum, SOL | free–$1 | 1–30 conf |
| binance | BTC, ETH, USDT, USDC, FDUSD | Omni, ERC20, TRC20, BEP20, Arbitrum, Solana | free–$1 | 1–15 conf |
| delta | BTC, ETH, USDT, USDC | Omni, ERC20, TRC20 | free–$5 | 1–30 conf |
| aevo | BTC, ETH, USDC, USDT | Ethereum, Arbitrum, Avalanche | free–$1 | 1–30 conf |
| derive | BTC, ETH, USDC, USDT | Arbitrum, Ethereum | free–$1 | 1–30 conf |
| thalex | BTC, ETH, USDT, USDC | native + ERC20 | free–$3 | 1–20 conf |

The paper treasury implements deposits/withdrawals against these rails
(with overdraft refusal and per-venue lifetime tracking) so capital
behavior — including the "deposit on the right venue" decision the gate
forces — is exercised end-to-end.

## Profit attribution

Every closed trade decomposes:

```
realized = gross_locked − exit_drift − fees − hedge_pnl
```

- **gross_locked**: `(sell_px − buy_px) × units` at entry — what the
  signal promised.
- **exit_drift**: what the market gave/took between entry and exit
  (slippage, convergence shortfall, quote movement).
- **fees**: both legs, both directions.
- **hedge_pnl**: residual-delta hedging results.

Session-level: capture ratio (realized vs signaled edge, also the
calibration loop's input), win rate, profit factor, average trade, ROC on
peak reserved capital, and equity measured against **net deposits** so
funding flows never inflate performance. All of it appears in the profit
attribution panel, `/api/exec`, and `/metrics`.
