# dex_aggregator Contract — Deep Reference

> **Currency:** current as of **v2.2.0**. Message types, the `ExecuteMsg`/`QueryMsg` tables,
> state, execution flow, fee/sweep semantics, simulation and errors have all been revised
> against the source. Where this doc and the source disagree, the source wins — say so and fix
> the doc.
>
> Read [v2_2_0_changes.md](v2_2_0_changes.md) first: it explains the fund-safety invariant the
> engine now rests on, and why several things that look like extra work are load-bearing.
> [v2_1_0_changes.md](v2_1_0_changes.md) covers the superseded, never-shipped v2.1.0 and is
> still accurate as background. For flash-arb specifics see
> [flash_route_plan.md](flash_route_plan.md).
>
> **Two deployments, one binary.** The Choice frontend instance is permissionless with
> attacker-controlled route fields; the arb instance is effectively single-caller and cares
> about `SimulateRoute` matching execution. Note that `ExecuteRoute` is permissionless on
> *both* — only `FlashRoute` is gated. Evaluate every change against both.

## Purpose

The `dex_aggregator` contract orchestrates multi-hop, multi-path token swaps across AMM pools and orderbook contracts on Injective. A user submits a **route** — a sequence of **stages**, each containing parallel **splits** — and the contract executes every swap, handles CW20/native asset conversions mid-route, deducts per-pool fees, and pays out the final result with slippage protection.

## Source Files

All source lives in `contracts/dex_aggregator/src/`.

| File | Lines | Role |
|------|-------|------|
| `lib.rs` | 11 | Module declarations. Re-exports `ContractError`. |
| `contract.rs` | 221 | Entry points (`instantiate`, `execute`, `query`, `reply`, `migrate`). Routes each `ExecuteMsg` variant to its handler. |
| `msg.rs` | 480 | Every message type and data structure. Submodules `amm`, `clmm`, `cw20_adapter`, `reflection`. |
| `state.rs` | 205 | Storage keys, the state-machine types (`ExecutionState`, `Awaiting`, `SubmsgReplyState`), and `apply_fee`. |
| `error.rs` | 128 | `ContractError` enum (thiserror). |
| `execute.rs` | 867 | Swap construction, entry validation, balance snapshotting, flash entry points, admin functions. |
| `orderbook_exec.rs` | 1002 | Native Injective spot execution: book walking, order sizing (both modes), order construction, fill decoding. |
| `query.rs` | 922 | Route simulation, config/fee/flash queries, unit tests. |
| `reply.rs` | 1611 | Submessage reply state machine — the most complex file. Drives stage-by-stage execution, fund clamping, fee accrual, residue sweep. |

---

## Message Types (msg.rs)

### Route Structure

A route is expressed as `Vec<Stage>`. Stages execute sequentially; splits within a stage execute in parallel.

```
Route
 └── Stage[]              (sequential)
      └── Split[]          (parallel within a stage)
           ├── percent: u8  (must sum to 100 across splits in a stage)
           └── path: Vec<Operation>  (sequential hops within one split)
                └── Operation
                     ├── AmmSwap(AmmSwapOp)
                     ├── OrderbookSwap(OrderbookSwapOp)
                     └── ClmmSwap(ClmmSwapOp)
```

Percentages are validated on **every** stage (`validate_stages`, shared by `ExecuteRoute`,
`FlashRoute` and `SimulateRoute`), along with empty stages and empty paths. Only the first
stage used to be checked, and only in the executor.

### AmmSwapOp

```rust
pub struct AmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
    #[serde(default)]
    pub ask_asset_info: Option<amm::AssetInfo>,  // None => derived from Pair {}
    #[serde(default)]
    pub max_spread: Option<Decimal>,             // None => 49%, clamped to 49%
}
```

Supports both native and CW20 inputs/outputs.

`ask_asset_info` is the asset this hop produces. It is resolved **before** the hop runs,
because the engine snapshots that denom's balance to bound what the route may later spend and
pay out. Supplied, it is used as-is (free — the arb bot should always supply it); omitted, it
comes from the pool's `Pair {}` query (the side that isn't the offer), one extra query per hop.
Resolution is strict: an op whose output cannot be determined fails the route
(`UnresolvableAskAsset`).

⚠️ It is deliberately **not** read back from the swap event any more. Trusting the reply's
`ask_asset` attribute made the venue the authority on what it had produced — see the
`pool_address` note below.

`max_spread` is **never sent as `None`**. Choice's `assert_max_spread` is a no-op when it
and `belief_price` are both `None`, but Astroport substitutes its own 0.5% default and
asserts on it — and the `Simulation` query never applies that assert, so the pre-fire gate
cannot see the revert. `None` here means `DEFAULT_AMM_MAX_SPREAD` (49%, effectively
unbounded); the route's mandatory `minimum_receive` / `min_profit` is the real guard.

⚠️ `pool_address` is arbitrary — there is **no allowlist**, on either deployment. A caller can
point a hop at a contract they wrote. That contract's events are therefore untrusted input:
until v2.2.0 it could emit `ask_asset=<any denom> return_amount=<the aggregator's balance>`
while transferring nothing and be paid it, for 1 wei of gas. What contains this now is not the
event parsing but the **route-funds clamp** (see *Residue sweep and the route-funds
invariant*): a route may only ever move what it brought in.

### OrderbookSwapOp

```rust
pub struct OrderbookSwapOp {
    pub market_id: MarketId,
    pub target_denom: String,          // native denom this hop must produce
    #[serde(default)]
    pub quantity: Option<FPDecimal>,   // direct mode
    #[serde(default)]
    pub worst_price: Option<FPDecimal>,// direct mode
}
```

**Native tokens only.** Executed *natively* — the contract submits an atomic spot market
order from its own default subaccount; there is no external swap contract. `market_id` +
`target_denom` are sufficient: the offer denom is the market's other side,
`is_buy = (target_denom == market.base_denom)`, and the ticks come from the market.

- **Estimation mode** (`quantity`/`worst_price` omitted): the book is walked to size the
  order. Buys are sized at `worst_price` so reserved margin equals the input; the fill comes
  from cheaper levels and the chain refunds the difference (see *Fee System*). Sells trade
  the base input floored to `min_quantity_tick_size`.
- **Direct mode** (both supplied): no book-walk queries. `direct_order_params` still applies
  three adjustments, each of which can only produce an order no worse than asked for:
  1. price snapped to `min_price_tick_size` in the safe direction — a BUY's bound is a ceiling
     (round down), a SELL's a floor (round up);
  2. quantity floored to `min_quantity_tick_size` — never more than asked, never off-grid;
  3. quantity bounded by what the hop actually holds (BUY costs one atomic-fee-multiplier
     query; SELL none).

  Without (1) and (2) the chain rejects the order outright
  (`must be a multiple of the minimum quantity tick size`) and the **whole route** reverts.
  Without (3) a mid-route leg — whose true input is only known on chain — dies for
  insufficient funds instead of partially filling.

Both modes check `meets_min_notional`. `SimulateRoute` sizes direct-mode hops through the same
`direct_order_params` (`direct_mode_estimate`), so the gate quotes the order that gets
submitted rather than one derived from the book.

A hop that cannot place an order at all (sub-tick, or sub-`min_notional`) is a **graceful
zero-value path**, not an error: the split's allocation is returned by the residue sweep and
the rest of the route proceeds. It used to raise `AmountTooSmall`, which `?`-propagated and
killed the whole route over one dust split the gate had already priced at zero.

### ClmmSwapOp

```rust
pub struct ClmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
    #[serde(default)]
    pub ask_asset_info: Option<amm::AssetInfo>,  // None => derived from GetConfig {}
    #[serde(default)]
    pub minimum_amount_out: Option<Uint128>,     // direct mode
    #[serde(default)]
    pub slippage_bps: Option<u16>,               // None => 50 (0.5%), clamped to 10000
}
```

Native and CW20 both supported. `ask_asset_info` works exactly as on `AmmSwapOp`, resolved
from the pool's `GetConfig {}` when omitted.

- **Estimation mode**: a per-hop `Quote` query, less `slippage_bps`. A zero-output quote
  ends the split gracefully as a zero-value path rather than reverting.
- **Direct mode**: the caller fixes the floor and the `Quote` re-simulation is skipped. An
  unfillable hop reverts the atomic route.

### Asset Abstraction (amm submodule)

```rust
pub enum AssetInfo {
    Token { contract_addr: String },      // CW20
    NativeToken { denom: String },        // bank
}

pub struct Asset {
    pub info: AssetInfo,
    pub amount: Uint128,
}
```

Used throughout to represent any token. The contract checks asset type mismatches between stages and inserts automatic conversions via `cw20_adapter`.

### External Protocol Interfaces (msg.rs submodules)

| Submodule | What it talks to | Key messages |
|-----------|-----------------|--------------|
| `amm` | AMM DEX pools | `AmmPairExecuteMsg::Swap`, `QueryMsg::Simulation` |
| `clmm` | Choice CLMM pools | `ClmmPoolExecuteMsg::SwapExactInput`, `ClmmPoolFlashMsg::Flash`, `ClmmPoolQueryMsg::{Quote, GetConfig}` |
| *(orderbook)* | Injective exchange module — **not a contract** | `MsgCreateSpotMarketOrder` built in `orderbook_exec.rs`; there is no orderbook swap contract any more |
| `cw20_adapter` | Injective CW20 adapter | `ExecuteMsg::RedeemAndTransfer` (native→CW20), `Cw20::Send` to adapter (CW20→native) |
| `reflection` | Tax token contracts | `ExecuteMsg::TaxExemptTransfer`, `ExecuteMsg::TaxExemptSend` |

### ExecuteMsg Variants

| Variant | Auth | Purpose |
|---------|------|---------|
| `ExecuteRoute { stages, minimum_receive }` | Any | Start swap with native token (exactly 1 coin in `funds`) |
| `Receive(Cw20ReceiveMsg)` | Any | CW20 hook entry. Inner msg is `Cw20HookMsg::ExecuteRoute`. Also handles internal conversion receipts (non-hook CW20 receives emit a normalization event). |
| `UpdateAdmin { new_admin }` | Admin | Transfer admin |
| `SetFee { pool_address, fee_fraction }` | Admin | Set/update per-pool fee (decimal fraction of output, e.g. 0.003 = 0.3%; must be < 1) |
| `RemoveFee { pool_address }` | Admin | Remove fee for a pool |
| `UpdateFeeCollector { new_fee_collector }` | Admin | Change fee recipient address |
| `UpdateCw20Adapter { new_cw20_adapter }` | Admin | Repoint the CW20↔native adapter used by every conversion (v2.1.0; previously instantiate-only) |
| `EmergencyWithdraw { asset_info }` | Admin | Withdraw all of a specific asset from the contract |
| `RegisterTaxToken { contract_addr }` | Admin | Register a CW20 as a tax token |
| `DeregisterTaxToken { contract_addr }` | Admin | Deregister a tax token |
| `AuthorizeFlashSigner { signer }` | Admin | Add a signer to the `FlashRoute` allowlist |
| `RevokeFlashSigner { signer }` | Admin | Remove a signer from the allowlist |
| `SetFlashUnrestricted { open }` | Admin | Escape hatch — when true, `FlashRoute` is permissionless |
| `FlashRoute { flash_pool, flash_asset, flash_amount, stages, min_profit }` | Allowlisted signer | Capital-free CLMM flash-arb: borrow, run the cycle, repay principal+fee, keep the surplus. Cycle must end in `flash_asset` and may not route through `flash_pool`. |
| `FlashCallback { fee0, fee1, data }` | The flash pool | Borrower callback, valid only mid-flash (gated on `PENDING_FLASH` **and** `info.sender == flash_pool`) |

### QueryMsg Variants

| Variant | Response type | Purpose |
|---------|--------------|---------|
| `SimulateRoute { stages, amount_in }` | `SimulateRouteResponse { output_amount }` | Simulate route output without executing |
| `Config {}` | `Config { admin, cw20_adapter_address, fee_collector }` | Get contract config |
| `FeeForPool { pool_address }` | `FeeResponse { fee: Option<Decimal> }` | Get fee for specific pool |
| `AllFees { start_after, limit }` | `AllFeesResponse { fees: Vec<FeeInfo> }` | Paginated fee list (default 10, max 30) |
| `IsFlashSigner { signer }` | `IsFlashSignerResponse { authorized }` | Whether `signer` may call `FlashRoute` (allowlisted, or flash unrestricted) |
| `FlashSigners { start_after, limit }` | `FlashSignersResponse { signers, unrestricted }` | Paginated allowlist plus the unrestricted flag |

`SimulateRoute` is the arb bot's **pre-broadcast gate**, not a convenience. It shares
`estimate_single_swap_execution`, `direct_order_params`, `sell_base_quantity`,
`meets_min_notional`, `apply_fee` and `validate_stages` with the executor so a quote cannot
diverge from the fill. Any execution-side adjustment added outside that shared code makes the
gate lie. Known residual gap: splits sharing a venue within one stage (see `Stage`).

---

## State (state.rs)

### Storage Keys

| Key | Type | Purpose |
|-----|------|---------|
| `CONFIG` | `Item<Config>` | Admin address, CW20 adapter address, fee collector address |
| `FEE_MAP` | `Map<&Addr, Decimal>` | Pool address → fee percentage |
| `ACTIVE_ROUTES` | `Map<u64, ExecutionState>` | Master reply ID → in-progress route state |
| `SUBMSG_REPLY_STATES` | `Map<u64, SubmsgReplyState>` | Submessage reply ID → routing info back to parent state |
| `REPLY_ID_COUNTER` | `Item<u64>` | Monotonically incrementing counter for unique reply IDs |
| `TAX_TOKEN_REGISTRY` | `Map<&Addr, bool>` | Token address → registered (always `true`). Presence = tax token. |
| `FLASH_SIGNERS` | `Map<&Addr, ()>` | `FlashRoute` allowlist; presence = authorized. Empty = deny-all. |
| `FLASH_UNRESTRICTED` | `Item<bool>` | Escape hatch: when true the allowlist is bypassed. Absent = false. |
| `PENDING_FLASH` | `Item<PendingFlashCtx>` | Transient mid-flash context; its presence is the `FlashCallback` auth gate. |

### ExecutionState

```rust
pub struct ExecutionState {
    pub plan: RoutePlan,                        // Route + sender + minimum_receive + flash_repayment
    pub awaiting: Awaiting,                     // Current state machine phase
    pub current_stage_index: u64,               // Which stage we're on
    pub replies_expected: u64,                  // Countdown of pending submessage replies
    pub accumulated_assets: Vec<amm::Asset>,    // Outputs collected so far for this stage
    pub pending_swaps: Vec<PlannedSwap>,        // Swaps deferred while conversions complete
    pub pending_path_op: Option<PendingPathOp>, // Deferred next-op for mid-path conversion
    pub legs: Vec<SwapLeg>,                     // Executed venue trades, for the terminal event
    pub entry_balances: Vec<(AssetInfo, Uint128)>,   // Pre-route baseline per touched denom
    pub pending_fees: Vec<(AssetInfo, Uint128)>,     // Orderbook buy price improvement (flash-suppressed)
    pub pending_pool_fees: Vec<(AssetInfo, Uint128)>,// Per-pool FEE_MAP carve (never suppressed)
}
```

`entry_balances` is the important one: it covers the offer denom plus **every op's input and
output** denom, and is what `route_spendable` bounds all outgoing amounts against. The two fee
vectors are deliberately separate — `is_flash` suppresses the orderbook carve but must not
suppress a configured pool fee. Both are paid by `build_residue_sweep` at finalize, never from
the reply that charged them.

### Awaiting Enum (State Machine Phases)

| State | Meaning | Transitions to |
|-------|---------|---------------|
| `Swaps` | Waiting for swap submessage replies | `Swaps` (next stage), `Conversions` (next stage needs conversions), `FinalConversions` (last stage output normalization), or route complete |
| `Conversions` | Waiting for CW20↔native conversions before swaps in a stage | `Swaps` (all conversions done, execute deferred swaps) |
| `FinalConversions` | Waiting for output asset normalization after last stage | Route complete (payout) |
| `PathConversion` | Waiting for a mid-path asset type conversion within a multi-hop path | `Swaps` (conversion done, resume path) |

---

## Execution Flow (execute.rs + reply.rs)

### Entry

1. `ExecuteRoute` or `Receive` hook → `execute_aggregate_swaps_internal`
   (a `Receive` whose hook fails to deserialize **reverts** — `InvalidCw20Hook` — so the CW20
   transfer rolls back and the sender keeps their tokens)
2. Validates: non-zero amount, `validate_stages` (non-empty stages, non-empty splits and paths,
   percentages sum to 100 on **every** stage), and `minimum_receive > 0` (`ZeroMinimumReceive`)
3. Allocates a `master_reply_id` from `REPLY_ID_COUNTER`
4. `snapshot_entry_balances` records the pre-route balance of every denom the route can touch —
   the offer plus every op's input *and* output, with the route's own input subtracted from the
   offer's baseline. Resolving an op's output may cost a `Pair {}` / `GetConfig {}` query unless
   `ask_asset_info` is supplied
5. Creates initial `ExecutionState` with the offer asset in `accumulated_assets`
6. Calls `proceed_to_next_step` to begin stage execution

### Stage Execution Loop (reply.rs: `proceed_to_next_step`)

For each stage:

1. **`plan_next_stage`** — Examines what the next stage's splits need (native vs CW20 inputs), compares against what's accumulated, and produces:
   - `swaps_to_execute: Vec<PlannedSwap>` — the first operation of each split with its calculated amount
   - `conversions_needed: Vec<(Asset, AssetInfo)>` — any CW20↔native conversions required before swaps can proceed

2. **If conversions needed:**
   - Creates conversion submessages via `create_conversion_msg` (CW20→native: `Cw20::Send` to adapter; native→CW20: `RedeemAndTransfer`)
   - Sets `awaiting = Conversions`, stashes `pending_swaps`
   - Waits for all conversion replies → `handle_conversion_reply` → `execute_planned_swaps`

3. **If no conversions needed:**
   - Calls `execute_planned_swaps` directly

4. **`execute_planned_swaps`** — For each planned swap:
   - Clamps the split's input to `route_spendable(offer_denom)`, drawing from a **running
     per-denom budget** so two splits cannot both spend against the same real funds. A
     zero-clamped split is skipped
   - Allocates a unique `submsg_id`
   - Saves `SubmsgReplyState { master_reply_id, split_index, op_index, in_denom, in_amount,
     ob_order_qty, ob_order_price, ob_is_buy }` so replies can be routed and accounted
   - Calls `create_swap_cosmos_msg` to build the correct `CosmosMsg`; `Ok(None)` means the hop
     provably yields nothing and is skipped without burning a reply id
   - Dispatches all as `SubMsg::reply_on_success`

`plan_next_stage` computes the split allocation **once** and derives the conversion sizing from
that same vector. Two independent passes (one using `floor(pct·total/100)` for every split, the
other giving the last split the remainder) differed by the rounding remainder, which stranded
that many atomic units on the wrong side of a native↔CW20 conversion.

### Swap Message Construction (execute.rs: `create_swap_cosmos_msg`)

**AMM swaps:**
- Native input → `WasmMsg::Execute` with `funds` containing the coin
- CW20 input (standard) → `Cw20ExecuteMsg::Send` to pool with inner `AmmPairExecuteMsg::Swap`
- CW20 input (tax token) → `reflection::ExecuteMsg::TaxExemptSend` to pool with inner swap msg

**Orderbook swaps** (native input only; errors on CW20):
- Loads the spot market and checks the offer denom is the side opposite `target_denom`
- Sizes the order via `build_swap_order_msg` — estimation mode walks the book, direct mode goes
  through `direct_order_params` (tick snapping + affordability)
- Builds a `MsgCreateSpotMarketOrder` (`BuyAtomic`/`SellAtomic`) from the contract's own default
  subaccount, with the contract as fee recipient (self-relayer, so the relayer fee share is
  rebated to it)
- Returns `Ok(None)` — a graceful zero-value path — when no order can be placed

**CLMM swaps:**
- Native input → `WasmMsg::Execute` with `funds`; CW20 → `Cw20::Send` (or `TaxExemptSend`) with a
  `SwapExactInput` hook
- Estimation mode runs a `Quote` and applies `slippage_bps`; a zero quote returns `Ok(None)`
- Direct mode passes `minimum_amount_out` straight through, skipping the `Quote`

### Reply Handling (reply.rs: `handle_reply`)

Dispatch logic:
- If `SUBMSG_REPLY_STATES` contains `msg.id` → it's a swap reply → `handle_swap_reply`
- Otherwise, use `msg.id` as `master_reply_id`, dispatch based on `exec_state.awaiting`:
  - `Conversions` → `handle_conversion_reply`
  - `FinalConversions` → `handle_final_conversion_reply`
  - `PathConversion` → `handle_path_conversion_reply`

### Swap Reply Processing (reply.rs: `handle_swap_reply`)

1. Determines the produced **amount**:
   - Orderbook hops decode the typed `MsgCreateSpotMarketOrderResponse` (chain-produced, so
     trustworthy). A BUY's price-improvement surplus is captured here into `pending_fees`
   - AMM/CLMM hops use `parse_amount_from_swap_reply`: `post_tax_amount` from a wasm event whose
     `to == contract_address` (tax tokens) if present, else `return_amount` (AMM) or `amount_out`
     (CLMM); a decimal portion is truncated
   - A zero amount ends the path as a zero-value path

2. Determines the produced **asset** with `get_operation_output` — from the op's
   `ask_asset_info`, else the pool's `Pair {}`/`GetConfig {}`. **Never from the reply.**

3. Records a `SwapLeg` for the terminal `aggregator_swap` event.

4. **If more operations remain in this split's path** (multi-hop):
   - Clamps the carried amount to `route_spendable(output_denom)`; a zero clamp ends the path
   - Checks if the next operation's expected input type matches the received output type
   - If mismatch: sets `Awaiting::PathConversion`, stashes `PendingPathOp`, dispatches conversion
   - If match: creates the next swap message, allocates a new `submsg_id`, dispatches

5. **If path is complete** (last operation in split):
   - Applies fee via `apply_fee` (looks up `FEE_MAP` for the pool, deducts the fraction) and
     **accrues** it into `pending_pool_fees` — it is *not* sent from here (see *Fee System*)
   - Adds the net output to `accumulated_assets`
   - Decrements `replies_expected`
   - If all splits done → increments `current_stage_index` → `proceed_to_next_step`

### Final Stage (reply.rs: `handle_final_stage`)

After all stages complete:

1. Picks the normalization target: `flash_asset` for a flash cycle, else the first accumulated
   asset's type
2. **If uniform:** → `finalize_route`
3. **If mixed types:** dispatches conversion submessages for the non-matching assets (each
   clamped to `route_spendable`) → `Awaiting::FinalConversions`
4. `handle_final_conversion_reply` accumulates the converted amounts → `finalize_route`

`finalize_route` then:
- credits the orderbook buy surplus into the total on a **flash** route (bounded by real
  headroom) so `min_profit` measures the caller's actual edge;
- **bounds the total by `route_spendable(final_asset)`**, erroring with `OutputNotBacked` rather
  than promising more than it holds;
- checks `minimum_receive` (user swap) or `repay + min_profit` (flash);
- disburses — payout to `plan.sender`, or `repay_amount` to the pool plus surplus to the caller;
- appends `build_residue_sweep`, which drains every snapshotted denom.

### Amount Parsing from Replies

**Swap replies** (`parse_amount_from_swap_reply`, AMM/CLMM only — orderbook hops decode the
typed spot-order response instead):
- Priority 1: `post_tax_amount` from a wasm event where `to == contract_address` (tax tokens)
- Priority 2: `return_amount` (AMM) or `amount_out` (CLMM) from wasm events

⚠️ These are **amounts only**. The produced *asset* is resolved before the hop runs; there is no
`ask_asset` event parsing any more. And because an amount from an untrusted venue is still an
untrusted number, everything downstream of it is clamped by `route_spendable`.

**Conversion replies** (`parse_amount_from_conversion_reply`):
- Priority 1: `amount` from `transfer` event where `recipient == contract_address` (native→CW20)
- Priority 2: `amount` from wasm event where `action == "transfer"` (CW20→native)

---

## Fee System

- Fees are configured per pool address in `FEE_MAP` as `Decimal` fractions (must be < 1.0)
- **Charged** at path completion — when the last operation in a split's path returns a result
- Formula: `fee = amount * fee_fraction`, `amount_after_fee = amount - fee`
- **Paid at finalize**, out of `pending_pool_fees`, by `build_residue_sweep`
- Pools with no entry in `FEE_MAP` have zero fee. **`FEE_MAP` is currently empty on both
  mainnet instances**, so no aggregator fee is charged on any route today
- Orderbook hops carry **no** aggregator fee — there is no pool address to key `FEE_MAP` by,
  and the exchange already takes its own trading fee

⚠️ **Do not send the fee from the reply that charged it.** It used to be `add_message`d onto that
reply's response *after* `proceed_to_next_step`. Submessages recurse depth-first, so the entire
rest of the route — including `finalize_route` and its residue sweep — ran first; the sweep saw
the still-unsent fee as residue in a snapshotted denom and paid it to the **user**, and the
queued transfer then found `spendable balance 0`. Any `SetFee` on a pool used anywhere but a
route's last stage was enough to brick it, as was any closed cycle. Accruing and paying both
fee pots from the one place removes the ordering dependency entirely.

### Orderbook buy price-improvement surplus

A buy is sized at `worst_price` so reserved margin equals the input; it fills from cheaper
levels and the chain refunds the difference. The **price improvement on the filled quantity** —
`(order_price − fill.price) × filled` — is protocol revenue, accrued into `pending_fees` **in the
buy hop's offer (quote) denom** and paid at finalize.

Everything *else* in that refund is the user's and returns with the rest of the residue: the
quantity-tick flooring loss (input never committed as margin at all), the `(1 + fee)` sizing
headroom, the relayer-fee rebate, and the margin held against any unfilled base.

⚠️ Measuring the user's share as "the unfilled fraction" — `reserved · (order_qty − filled) /
order_qty` — makes it exactly **zero on every complete fill**, so the whole refund was taken
regardless of how little of the input the order spent. Measured on a single-level fill, where
the order is placed at the resting ask and price improvement is provably zero: 1.002249 USDT of
a 1000.999999 USDT buy to the collector, nothing back to the user. About 10 bps, on an instance
whose `AllFees` reports none. `SubmsgReplyState.ob_order_price` exists to make the real
quantity measurable.

Two further things are easy to get wrong:

- ⚠️ **`ob_buy_surplus` overestimates.** It derives the refund in `FPDecimal` from the decoded
  fill and can land a few atomic units above what the exchange actually returned. Every
  consumer must clamp against real balance — `build_residue_sweep` does so with
  `.min(residue)`, and `finalize_route` bounds its flash credit the same way. Using the raw
  figure makes the payout exceed the balance and reverts the whole route.
- On a **flash** route there is no carve at all: the surplus is credited into the amount
  `min_profit` measures and paid to the caller. A flash caller is an allowlisted signer
  running their own capital, so it is their arb profit, not aggregator revenue.

### Residue sweep and the route-funds invariant

`snapshot_entry_balances` records a baseline per touched denom at entry — the offer denom plus
every operation's input **and output** denom, with the route's own input subtracted from the
offer's baseline. Those baselines carry two invariants, and both are load-bearing.

**1. A route may only move what it brought in.**
`route_spendable(info) = current_balance − entry_baseline` bounds every amount that leaves the
contract:

| Site | What is clamped |
|---|---|
| `execute_planned_swaps` | each split's swap input, from a running per-denom budget |
| `handle_swap_reply` | the amount carried between hops of a multi-hop path |
| `proceed_to_next_step`, `handle_final_stage` | each adapter conversion |
| `finalize_route` | the final payout — errors `OutputNotBacked` rather than over-promising |

It fails closed: an asset with no baseline cannot be bounded, so it cannot be moved
(`UnsnapshottedAsset`). On an honest route every clamp is a no-op, because the contract
necessarily holds at least the tracked output.

Both the dispatch clamp and the payout clamp are needed. Clamping only the payout still lets a
route fabricate an intermediate credit and *spend* it through a real pool on the next stage,
turning the aggregator's idle balance into something the payout clamp then finds legitimately
backed.

**2. A successful route leaves nothing behind.**
`build_residue_sweep` returns `current − baseline` of each denom: the accrued fees
(`pending_pool_fees` always, plus `pending_fees` unless this is a flash route), capped at the
residue, to the fee collector; the remainder to the route's sender. For the **final** asset the
tracked payout is netted out rather than the denom being skipped — skipping it stranded the buy
surplus on every closed cycle, where the buy's offer denom *is* the final asset.

## Indexing — the `aggregator_swap` event

Every completed **user** swap emits exactly one consolidated event so an indexer can
record one row per route, instead of stitching together the underlying pool/market
events. Emitted by `build_swap_event` in `finalize_route` (the non-flash branch;
flash-arb cycles emit `flash_route_complete` instead). On-chain type:
`wasm-aggregator_swap`.

Top-line attributes (field names mirror the legacy
`inj-orderbook-swap-contract` `atomic_swap_execution` event so existing indexer
plumbing maps over):

| attribute | meaning |
|-----------|---------|
| `sender` | route initiator (CW20 sender or native caller) |
| `recipient` | where the output went (= `sender`) |
| `swap_input_denom` / `swap_input_amount` | the route's original offer (denom = bank denom or CW20 address) |
| `swap_final_denom` / `swap_final_amount` | the net output delivered to the user |
| `minimum_receive` | the route's slippage floor (0 if unset) |
| `stage_count` | number of stages in the route |
| `leg_count` | number of executed venue trades |
| `swap_results` | JSON array of per-venue legs (see below) |

`swap_results` is a JSON array of `SwapLeg` objects (one per executed venue trade;
CW20↔native conversions are **not** legs):

```json
[{"kind":"amm","venue":"inj1pool…","offer_denom":"inj","offer_amount":"33000000000000000000",
  "ask_denom":"usdt","ask_amount":"330000000","fee_amount":"0"}]
```

- `kind` — `"amm"`, `"clmm"`, or `"orderbook"`
- `venue` — pool contract address (AMM/CLMM) or spot market id (orderbook)
- `offer_*` / `ask_*` — the leg's input and gross output (before aggregator fee)
- `fee_amount` — aggregator fee taken on the leg, in `ask_denom` (0 for orderbook
  and non-terminal hops; per-pool `FEE_MAP` fee on terminal hops)

Notes:
- Leg order is completion order (parallel splits interleave), but each leg is
  self-describing (`venue` + `offer`/`ask`), so order is irrelevant to indexing.
- A route that produces nothing (all splits zero-filled, `minimum_receive == 0`)
  completes via `aggregate_swap_complete_empty` and emits no `aggregator_swap` event
  — there is no volume to record.

## Tax Token Handling

- Tax tokens are CW20 tokens with transfer taxes (registered in `TAX_TOKEN_REGISTRY`)
- When sending tax tokens to pools: uses `reflection::ExecuteMsg::TaxExemptSend` instead of `Cw20ExecuteMsg::Send`
- When transferring tax tokens to users: uses `reflection::ExecuteMsg::TaxExemptTransfer` instead of `Cw20ExecuteMsg::Transfer`
- Reply parsing checks for `post_tax_amount` attribute first (the actual amount received after tax)

## Simulation (query.rs)

`simulate_route` walks stages and splits exactly as execution would, querying instead of
executing:

- **AMM:** `Simulation { offer_asset }` → `return_amount`; output asset from `Pair {}`
- **CLMM:** `Quote { token_in, amount_in }` → `amount_out`; output asset from `GetConfig {}`
- **Orderbook:** the same estimator the executor uses
  (`estimate_single_swap_execution`), or `direct_mode_estimate` when the op supplies
  `quantity`/`worst_price` — so a direct-mode hop is quoted as the order that will actually be
  submitted, not one re-derived from the book

It mirrors the executor's structure, not just its per-hop maths:

- `validate_stages` runs first, so the gate rejects every shape execution rejects
- stage allocation is identical: one native pile plus one CW20 pile summed into a single
  `total_logical_amount` under adapter identity, with the last split absorbing the remainder.
  Allocating each split from the pile matching its *own* input asset instead both quoted
  multi-denom stages the executor refuses (`MixedAssetsInStage`) and quoted ~zero for any split
  whose input the executor would have produced by a conversion
- two different natives (or CW20s) arriving in one stage is rejected, as in the executor
- the per-pool `FEE_MAP` carve is applied at each split path's terminal hop, via the shared
  `apply_fee`
- outputs are summed only when they are the same asset under **adapter identity** (a CW20 and
  its `factory/<adapter>/<cw20>` wrapper). Summing unconditionally reported 100 USDT plus 5 INJ
  as `105`

Does **not** model: tax-token deductions, the orderbook buy surplus carve (it isn't part of the
quoted output), or two splits sharing one venue within a stage (documented on `Stage`).

## Error Types (error.rs)

| Variant | When |
|---------|------|
| `Std(StdError)` | Propagated from cosmwasm-std |
| `Unauthorized` | Non-admin calls admin function |
| `ZeroAmount` | Offer amount is zero |
| `NoStages` | Empty stages vec |
| `EmptyRoute` | A stage or path has no entries |
| `InvalidPercentageSum` | Split percentages don't sum to 100 |
| `InvalidFunds { sent }` | ExecuteRoute called with != 1 coin |
| `MinimumReceiveNotMet { minimum_receive, actual_receive }` | Final output below threshold |
| `SubmessageFailed { split_index, op_index, contract_addr, error }` | A swap submessage failed |
| `ConversionFailed { awaiting_state, error }` | A CW20↔native conversion failed |
| `NoAmountInReply` | Reply events missing amount attribute |
| `MalformedAmountInReply { value }` | Amount attribute can't be parsed |
| `NoConversionEventInReply` | Conversion reply has no recognizable event |
| `MixedAssetsInStage { kind, first, second }` | Two different natives (or CW20s) arrived in one stage |
| `ZeroMinimumReceive` | `minimum_receive` omitted or zero |
| `InvalidCw20Hook { reason }` | A CW20 `Receive` hook that could not be deserialized — reverts, so the sender keeps their tokens |
| `OutputNotBacked { asset, wanted, available }` | A hop reported an output it did not deliver; the payout would exceed what the route brought in |
| `UnsnapshottedAsset { asset }` | No entry baseline for an asset the route is trying to move — the engine cannot bound it, so it refuses |
| `UnresolvableAskAsset { venue, reason }` | An op's output asset could not be determined (no `ask_asset_info`, and `Pair {}`/`GetConfig {}` failed or didn't contain the offer) |
| `InvalidOrderbookDenom { denom, market_id }` | Offer/target denom isn't a side of the market |
| `OrderResponseDecode { err }` | Spot-order reply could not be decoded |
| `AmountTooSmall` | Retained for API compatibility; **no longer returned** — an unplaceable orderbook hop is now a graceful zero-value path |
| `NoPendingFlash` | `FlashCallback` with no flash in flight (forged or stray) |
| `FlashPoolInCycle` | The cycle routes through the flash-source pool (checked for every venue with an address) |
| `FlashAssetNotInPool` | `flash_asset` is neither token0 nor token1 of the flash pool |
| `FlashProfitNotMet { required, actual }` | Cycle output below `repay_amount + min_profit` |

## Instantiation

```rust
InstantiateMsg {
    admin: String,                  // Admin address
    cw20_adapter_address: String,   // CW20 adapter contract for CW20↔native conversions
    fee_collector_address: String,  // Address that receives collected fees
}
```

Validates all addresses, saves `Config`, initializes `REPLY_ID_COUNTER` to 0.

## Key Implementation Details

- All entry points are parameterized with Injective types: `DepsMut<InjectiveQueryWrapper>`, `Response<InjectiveMsgWrapper>`
- `REPLY_ID_COUNTER` is a global monotonic counter shared across all concurrent routes
- The `master_reply_id` is the ID used for the overall route; individual swap submessages get their own IDs from the counter
- Conversion submessages reuse the `master_reply_id` since they don't need per-swap routing
- `pending_swaps` in `ExecutionState` temporarily holds the planned swaps while conversions complete
- `pending_path_op` holds the next operation in a multi-hop path while a mid-path conversion
  completes, along with its own `split_index`/`op_index` (recovering them by searching the plan
  mis-attributed the resumed hop whenever the same op appeared in two splits)
- The last split in a stage receives the remainder amount (total − already allocated) to prevent
  rounding dust loss, and the conversion sizing is derived from that same allocation vector
- **Nothing a venue reports is trusted as a quantity of value.** Amounts come from events (or,
  for orderbook hops, a chain-signed response); what makes them safe is that every outgoing
  amount is clamped to `route_spendable`. Adding a new disbursement path without that clamp
  reopens a drain of the contract's whole balance by any caller
- `overflow-checks = true` in the release profile, so the `u64` counters and index arithmetic
  fail closed rather than wrapping
