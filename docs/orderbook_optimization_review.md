# Orderbook Execution: Optimization Review (arb-competitive)

Scope: the execution + estimation path of `inj-orderbook-swap-contract`
(`swap.rs`, `queries.rs`), evaluated for the merged aggregator.

## Two consumers → two modes (read this first)

The merged contract serves two callers with opposing priorities:

- **Choice dApp swaps** — caller doesn't know the book, wants accurate fills, low
  dust, and an on-chain `SimulateRoute` quote. → **Estimation mode**: walk the book
  for accurate sizing. The estimator is *not* dead weight here — it is the dApp's
  pricing engine and must stay in lockstep with execution.
- **Arb bot** — rebuilds the book off-chain every block, wants minimal latency/gas,
  supplies exact order params. → **Direct mode** (§C): zero estimation queries,
  `minimum_receive` is the net.

This split **supersedes §B for the dApp**: TOB+buffer sizing deliberately under-sizes
and strands dust, which is wrong for UI users. Keep the accurate book walk in
estimation mode; arb uses direct mode instead. §A (sell needs no query to *execute*)
applies to both modes; the book is only needed to *simulate* a sell for a dApp quote.

## The one constraint that drives everything

Injective's only spot market-order primitive (`MsgCreateSpotMarketOrder` →
`SpotOrder { price, quantity, .. }`) is **base-quantity-denominated** with a
`worst_price` bound. There is no "spend exactly this much quote" buy. So:

- **Sell hop:** quantity = the base you already hold. *No price/book data is needed
  to size it.* `worst_price` only bounds slippage.
- **Buy hop:** you must convert a quote input into a base quantity → you need a
  price. This is the *only* place market data is structurally required, and it must
  be a **conservative (upper-bound) average fill price**, or the order can demand
  more quote than you hold and fail.

Market orders are immediate-or-cancel: unfilled quantity is dropped, and the reply
reports the *actual* filled amount. Combined with the end-of-route `minimum_receive`
check, this means **a loose `worst_price` is safe** — a bad fill just produces less
output and the route reverts. We do not need a tight per-hop price bound for safety;
we need it only to sell/buy the intended quantity.

## What the current code does per hop (cost inventory)

`estimate_single_swap_execution` (called in execution *and* simulation) issues, per
hop:

1. `query_spot_market` — market denoms + ticks.
2. `query_market_atomic_execution_fee_multiplier` — fee for buy sizing.
3. `query_spot_market_orderbook` — **full book walk** (heaviest).
4. `query_balance` — margin/funds check.

Then `get_minimum_liquidity_levels` walks levels into a `Vec`, and
`get_average_price_from_orders` folds over them *again*, and
`get_worst_price_from_orders` takes the last — three passes over the levels with
256-bit `FPDecimal` math throughout. A 2-hop round trip ≈ **8 chain queries + heavy
math + per-step state I/O + a JSON-serialized event**.

## Findings, ordered by arb impact

### A. Sell hops need zero market-data queries to size  ★ high impact
The sell quantity is the input balance you already hold. Today the contract still
pulls the full bid book to compute `worst_price`/`average_price`. For arb, drop the
walk: set `worst_price` to a loose lower bound (a configurable slippage off TOB
`best_buy_price`, or the market's min price tick), place the sell, and let
`minimum_receive` reject unprofitable fills. **Removes the heaviest query on every
sell hop.**

### B. Replace the full book walk with TOB on buy hops  ★ high impact
Buys need a price only to size base from quote. Use
`query_spot_market_mid_price_and_tob` (cheap, no level iteration) instead of
`query_spot_market_orderbook`. Size conservatively:
`base = floor( (quote/(1+fee)) / (best_sell_price · (1+slippage)) , qty_tick )`.
The `(1+slippage)` buffer makes `base` an *under*-estimate so the order can never
demand more quote than held; `worst_price = best_sell_price · (1+slippage)` bounds
the fill. Leftover quote (from under-sizing) is the cost — see §F.

### C. Best path for arb: router-supplied per-hop quantity/price  ★★ highest impact
The arb bot already reconstructs the book every block off-chain and knows the exact
optimal `base` quantity and `worst_price` per hop. Add optional fields to the op:

```rust
pub struct OrderbookSwapOp {
    pub market_id: MarketId,
    pub target_denom: String,
    pub quantity: Option<FPDecimal>,    // base to trade; None => derive
    pub worst_price: Option<FPDecimal>, // price bound;   None => derive
}
```

When both are supplied, the contract issues **zero estimation queries** — it places
the atomic order directly and relies on the final `minimum_receive`. The contract
becomes a thin atomic executor + safety net. This is the lowest-latency, lowest-gas,
highest-edge path and is the recommended primary mode for the arb router; §A/§B are
the fallback when quantities aren't supplied (UI swaps, simulation).

Still need `query_spot_market` once per *distinct* market (denoms/ticks + direction)
— cache it across hops within a route (see §E).

### D. Drop the in-execution margin `query_balance`  ★ medium
`estimate_execution_buy_from_source` does a `query_balance` purely to pre-empt an
insufficient-funds error. In execution the order placement enforces funds anyway;
the query only buys a prettier error. Keep it for `is_simulation=true`, drop it for
execution. Saves one query per buy hop.

### E. Cache market metadata across a round trip  ★ medium
A→B→A arb touches each market once or twice; `query_spot_market` (and the fee
multiplier, if still used) are re-issued per hop. Cache `SpotMarket` by `MarketId`
for the duration of one route execution. Saves redundant queries on multi-hop routes
that revisit a market.

### F. Account for sub-tick / under-size dust  ★ medium (correctness)
Conservative sizing (§B) and tick rounding leave small leftover balances of the
intermediate denom in the contract. On a round trip these are *not* the final denom,
so they're stranded (recoverable only via `emergency_withdraw`). Mitigations:
router-supplied exact quantities (§C) minimize it; alternatively sweep known
leftover denoms back to the user at route end. At minimum, document it and prefer §C
for arb.

### G. Single-pass level reduction  ★ low (gas), only matters in fallback mode
If the book walk survives for the fallback path, fuse `get_minimum_liquidity_levels`
+ `get_average_price_from_orders` + `get_worst_price_from_orders` into **one** pass
that accumulates `total_notional`, `total_quantity`, and tracks the last price —
avoiding two extra iterations and the intermediate `Vec`. Round the average to the
price tick once, in the conservative direction.

### H. Strip per-step state + JSON event  ★ low (gas)
`SWAP_OPERATION_STATE` / `STEP_STATE` / `SWAP_RESULTS` and the
`serde_json_wasm`-serialized `atomic_swap_execution` event exist to support
multi-step routing and external observability. In the merged one-market-per-op design
the cross-step accumulators disappear; do **not** reintroduce per-hop `Vec` storage,
and gate or drop the JSON event (arb doesn't read it; the typed reply carries
everything).

### I. Delete unused estimation surface  ★ low
The aggregator only ever calls `SwapMinOutput`. Drop `SwapExactOutput`, the
`*_from_target` estimators, `GetInputQuantity`, full-route `estimate_swap_result`,
and refund logic — dead weight that also pulls extra queries in the exact-output
path.

## Net effect for a 2-hop round-trip arb

| | current | §A/§B fallback | §C router-supplied |
|---|---|---|---|
| chain queries | ~8 | ~3 | ~1–2 (market meta, cached) |
| level-walk passes | 3/hop | 1/buy hop | 0 |
| per-step state writes | yes | minimal | minimal |
| JSON event | yes | dropped | dropped |
| price-bound source | full book | TOB | bot-supplied |
| safety | per-hop worst_price | min_receive | min_receive |

## Gas optimization (whole-contract, not just orderbook)

Gas cost hierarchy in CosmWasm: **storage writes > storage reads > module/contract
queries > 256-bit math > event bytes**. The merged contract's biggest avoidable
costs are in the reply state machine, independent of the orderbook math.

### G1. Stop re-serializing the route plan on every reply  ★★ highest gas impact
`ACTIVE_ROUTES` stores the entire `ExecutionState`, which **embeds the full
`Vec<Stage>` plan**, and `proceed_to_next_step`/`handle_*_reply` load *and re-save*
it on every reply. A 2-hop route re-serializes the whole nested route 3–4×; writes
are the priciest gas op.

Fix: split state into
- `ROUTE_PLAN: Map<u64, RoutePlan>` — written **once** at route start, read-only
  thereafter (never re-saved), and
- `ROUTE_CURSOR: Map<u64, Cursor>` — tiny mutable `{ awaiting, stage, replies_expected,
  accumulated_assets, pending }`, the only thing re-saved per reply.

Reading the plan is unavoidable (we need the next op), but we stop paying the
write each reply.

### G2. Bit-pack reply IDs; delete `SUBMSG_REPLY_STATES` entirely  ★★ high
Today each submessage does a `SUBMSG_REPLY_STATES.save` then a `.remove` on reply
(write + read + delete per submsg; ×N for parallel splits). The reply only hands
back `msg.id: u64` — so pack the context *into the id* instead of storing it:

```
id = (route_nonce << 24) | (kind << 20) | (split_index << 10) | op_index
```

`handle_reply` unpacks `(route_nonce, split, op)` and a `kind` tag (swap vs
conversion vs final) directly — **zero per-submessage storage ops**, and
`REPLY_ID_COUNTER` can go too (the nonce comes from the route key). Indices fit
easily (split/op < 1024). This is a large win for multi-split/multi-hop routes.

### G3. Per-mode query elimination is also a gas win  ★★ high
Each `query_spot_market_orderbook` deserializes a potentially large
`QueryOrderbookResponse` (many price levels) — significant gas, not just latency.
Direct mode (§C) drops it to ~zero; estimation mode uses TOB for the price bound and
caches `SpotMarket` per market (§E). See §A–§E.

### G4. Trim hot-path events  ★ medium
Event attributes are charged per byte. The state machine adds many debug attributes
(`action`, `split_index`, `op_index`, …) on every step, and the orderbook path
JSON-serializes `swap_results` into an event (§H). Drop the JSON event and gate
verbose attributes; keep only what an indexer needs.

### G5. Skip the aggregator fee message when fee is zero  ★ medium
`apply_fee` + `create_send_msg` add a `BankMsg`/`Cw20` transfer per fee-bearing hop.
For arb (operator's own routes) configure no fee → no extra message and no extra
storage read of `FEE_MAP`. Short-circuit when `fee_collector == sender` or fee == 0.

### G6. No-reply "fire-and-check" direct mode  ★ high — **CONFIRMED viable**
If the bot supplies every hop's `quantity`/`worst_price`, the contract emits all
spot orders in one execute (no per-hop reply chaining) and checks the final balance
against `minimum_receive` once at the end — eliminating N reply re-entries and all
intermediate cursor I/O. **Proven** in the test-tube spike (`orderbook_proof_results.md`,
Q3): two atomic orders in one tx with no reply between them settle synchronously —
order 2 spent order 1's proceeds, contract started with zero of the intermediate
asset and round-tripped cleanly. Fall back to chained replies (G1/G2-optimized) only
for estimation mode / when quantities aren't supplied.

### G7. Smaller code = cheaper load  ★ low (free side effect)
Deleting the route registry, exact-output, from-target estimators, and per-step
accumulators (§I, plan §) shrinks the WASM, lowering per-call load cost.

Net: for a 2-hop direct-mode arb the per-reply storage write drops from "full plan"
to "tiny cursor" (G1), per-submessage storage ops go to zero (G2), and queries go to
~1–2 cached reads (G3) — the bulk of the gas the old design spent.

## How this folds into the merge plan

- §C adds two optional fields to `OrderbookSwapOp`; estimation becomes a *fallback*,
  not the hot path. Update `docs/orderbook_merge_plan.md`'s op definition.
- §A/§B/§D/§E rewrite the ported estimator into a lean "size + bound" helper rather
  than a full execution simulator.
- §F/§H are correctness/cleanliness guards to carry into the merged reply handler.
- §I shrinks the ported surface area substantially.

## Verified (test-tube, latest chain — see `orderbook_proof_results.md`)

- ✅ Atomic spot market orders are **immediate-or-cancel**: oversize order filled
  partially (550 of 1000 into a 600-deep book) and reported the actual fill; no
  revert. The §A/§B/§C "loose worst_price + min_receive" safety argument holds.
- ✅ A **loose `worst_price` (5× touch) is accepted**, filling at the real book
  price — no band rejection on a plain market. (Still clamp to the band edge on
  markets that define price bands; not exercised.)
- ✅ **G6 no-reply chaining works** — order 2 spends order 1's proceeds in one tx.
