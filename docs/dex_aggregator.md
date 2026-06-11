# dex_aggregator Contract — Deep Reference

## Purpose

The `dex_aggregator` contract orchestrates multi-hop, multi-path token swaps across AMM pools and orderbook contracts on Injective. A user submits a **route** — a sequence of **stages**, each containing parallel **splits** — and the contract executes every swap, handles CW20/native asset conversions mid-route, deducts per-pool fees, and pays out the final result with slippage protection.

## Source Files

All source lives in `contracts/dex_aggregator/src/`.

| File | Lines | Role |
|------|-------|------|
| `lib.rs` | 9 | Module declarations. Re-exports `ContractError`. |
| `contract.rs` | 158 | Entry points (`instantiate`, `execute`, `query`, `reply`). Routes each `ExecuteMsg` variant to the appropriate handler. |
| `msg.rs` | 268 | Every message type and data structure. Contains submodules `amm`, `orderbook`, `cw20_adapter`, and `reflection`. |
| `state.rs` | 64 | All storage keys and the core state-machine types (`ExecutionState`, `Awaiting`, `SubmsgReplyState`). |
| `error.rs` | 64 | `ContractError` enum (thiserror). |
| `execute.rs` | 380 | Swap construction, admin functions, and the main `execute_aggregate_swaps_internal` entry. |
| `query.rs` | 576 | Route simulation, config/fee queries, and unit tests. |
| `reply.rs` | 887 | Submessage reply state machine — the most complex file. Drives stage-by-stage execution. |

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
                     └── OrderbookSwap(OrderbookSwapOp)
```

### AmmSwapOp

```rust
pub struct AmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
}
```

Supports both native and CW20 inputs/outputs. The output (ask) asset is not
declared on the op: during execution it's read from the pair's swap event
(`ask_asset` attribute), and during `SimulateRoute` it's derived from the pair's
`Pair {}` query (the side that isn't the offer).

### OrderbookSwapOp

```rust
pub struct OrderbookSwapOp {
    pub swap_contract: String,
    pub offer_asset_info: amm::AssetInfo,
    pub ask_asset_info: amm::AssetInfo,
    pub min_quantity_tick_size: Uint128,
}
```

**Native tokens only** for both input and output. Amounts are rounded down to the nearest `min_quantity_tick_size` before execution. The contract queries `GetOutputQuantity` on the orderbook contract, applies 0.5% slippage, and submits `SwapMinOutput`.

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
| `orderbook` | Orderbook DEX contracts | `OrderbookExecuteMsg::SwapMinOutput`, `QueryMsg::GetOutputQuantity` |
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
| `EmergencyWithdraw { asset_info }` | Admin | Withdraw all of a specific asset from the contract |
| `RegisterTaxToken { contract_addr }` | Admin | Register a CW20 as a tax token |
| `DeregisterTaxToken { contract_addr }` | Admin | Deregister a tax token |

### QueryMsg Variants

| Variant | Response type | Purpose |
|---------|--------------|---------|
| `SimulateRoute { stages, amount_in }` | `SimulateRouteResponse { output_amount }` | Simulate route output without executing |
| `Config {}` | `Config { admin, cw20_adapter_address, fee_collector }` | Get contract config |
| `FeeForPool { pool_address }` | `FeeResponse { fee: Option<Decimal> }` | Get fee for specific pool |
| `AllFees { start_after, limit }` | `AllFeesResponse { fees: Vec<FeeInfo> }` | Paginated fee list (default 10, max 30) |

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

### ExecutionState

```rust
pub struct ExecutionState {
    pub plan: RoutePlan,                        // Immutable route + sender + minimum_receive
    pub awaiting: Awaiting,                     // Current state machine phase
    pub current_stage_index: u64,               // Which stage we're on
    pub replies_expected: u64,                  // Countdown of pending submessage replies
    pub accumulated_assets: Vec<amm::Asset>,    // Outputs collected so far for this stage
    pub pending_swaps: Vec<PlannedSwap>,        // Swaps deferred while conversions complete
    pub pending_path_op: Option<PendingPathOp>, // Deferred next-op for mid-path conversion
}
```

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
2. Validates: non-zero amount, non-empty stages, first stage split percentages sum to 100
3. Allocates a `master_reply_id` from `REPLY_ID_COUNTER`
4. Creates initial `ExecutionState` with the offer asset in `accumulated_assets`
5. Calls `proceed_to_next_step` to begin stage execution

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
   - Allocates a unique `submsg_id`
   - Saves `SubmsgReplyState { master_reply_id, split_index, op_index }` so replies can be routed
   - Calls `create_swap_cosmos_msg` to build the correct `CosmosMsg`
   - Dispatches all as `SubMsg::reply_on_success`

### Swap Message Construction (execute.rs: `create_swap_cosmos_msg`)

**AMM swaps:**
- Native input → `WasmMsg::Execute` with `funds` containing the coin
- CW20 input (standard) → `Cw20ExecuteMsg::Send` to pool with inner `AmmPairExecuteMsg::Swap`
- CW20 input (tax token) → `reflection::ExecuteMsg::TaxExemptSend` to pool with inner swap msg

**Orderbook swaps:**
- Native input only (errors on CW20)
- Rounds amount down to nearest `min_quantity_tick_size`
- Queries `GetOutputQuantity` for expected output
- Applies 0.5% slippage buffer
- Sends `SwapMinOutput` with rounded funds

### Reply Handling (reply.rs: `handle_reply`)

Dispatch logic:
- If `SUBMSG_REPLY_STATES` contains `msg.id` → it's a swap reply → `handle_swap_reply`
- Otherwise, use `msg.id` as `master_reply_id`, dispatch based on `exec_state.awaiting`:
  - `Conversions` → `handle_conversion_reply`
  - `FinalConversions` → `handle_final_conversion_reply`
  - `PathConversion` → `handle_path_conversion_reply`

### Swap Reply Processing (reply.rs: `handle_swap_reply`)

1. Parses output amount from events via `parse_amount_from_swap_reply`:
   - First checks for `post_tax_amount` in wasm events (tax tokens, matched by `to == contract_address`)
   - Then falls back to `return_amount` (AMM) or `swap_final_amount` (orderbook from `wasm-atomic_swap_execution` event)
   - Truncates decimal portions to integer

2. **If more operations remain in this split's path** (multi-hop):
   - Checks if next operation's expected input type matches the received output type
   - If mismatch: sets `Awaiting::PathConversion`, stashes `PendingPathOp`, dispatches conversion
   - If match: creates next swap message, allocates new `submsg_id`, dispatches

3. **If path is complete** (last operation in split):
   - Applies fee via `apply_fee` (looks up `FEE_MAP` for the pool, deducts percentage)
   - Sends fee to `fee_collector` if non-zero
   - Adds output to `accumulated_assets`
   - Decrements `replies_expected`
   - If all splits done → increments `current_stage_index` → `proceed_to_next_step`

### Final Stage (reply.rs: `handle_final_stage`)

After all stages complete:

1. Checks if all accumulated outputs are the same asset type
2. **If uniform:** checks `minimum_receive`, sends total to `plan.sender`
3. **If mixed types:** dispatches conversion submessages for non-matching assets → `Awaiting::FinalConversions`
4. `handle_final_conversion_reply` accumulates converted amounts → checks `minimum_receive` → sends payout

### Amount Parsing from Replies

**Swap replies** (`parse_amount_from_swap_reply`):
- Priority 1: `post_tax_amount` from wasm event where `to == contract_address` (tax tokens)
- Priority 2: `return_amount` from wasm events (AMM)
- Priority 3: `swap_final_amount` from `wasm-atomic_swap_execution` events (orderbook)

**Conversion replies** (`parse_amount_from_conversion_reply`):
- Priority 1: `amount` from `transfer` event where `recipient == contract_address` (native→CW20)
- Priority 2: `amount` from wasm event where `action == "transfer"` (CW20→native)

---

## Fee System

- Fees are configured per pool address in `FEE_MAP` as `Decimal` percentages (must be < 1.0 / 100%)
- Deducted at **path completion** — when the last operation in a split's path returns a result
- Formula: `fee = amount * fee_fraction`, `amount_after_fee = amount - fee`
- Fee is sent to `config.fee_collector` as a separate message appended to the response
- Pools with no entry in `FEE_MAP` have zero fee

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

`simulate_route` walks through stages and splits exactly as execution would, but queries each pool's simulation endpoint instead of executing swaps:
- AMM: queries `Simulation { offer_asset }` → uses `return_amount`
- Orderbook: queries `GetOutputQuantity { from_quantity, source_denom, target_denom }` → uses `result_quantity`

Does **not** account for fees, conversions, or tax token deductions.

Split amount calculation: each split gets `total * percent / 100`, except the last split which gets the remainder to avoid rounding dust.

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
- `pending_path_op` holds the next operation in a multi-hop path while a mid-path conversion completes
- The last split in a stage receives the remainder amount (total - already allocated) to prevent rounding dust loss
