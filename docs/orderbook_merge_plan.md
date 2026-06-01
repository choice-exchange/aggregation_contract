# Merge Plan: Native Orderbook Swaps in `dex_aggregator`

Goal: fold `inj-orderbook-swap-contract`'s execution into `dex_aggregator` so the
aggregator places Injective spot-market orders directly, instead of calling an
external swap contract. Drop the per-op `min_quantity_tick_size` and the explicit
`offer/ask` asset info for orderbook hops — derive them from the on-chain market.
Keep the CW20 ↔ native conversion machinery (`cw20_adapter`) intact.

## Design decisions (locked)

1. **One `MarketId` per `OrderbookSwap` op.** Multi-market orderbook routes become
   multiple ops in a path. This deletes the external contract's route registry,
   `steps_from`, multi-step loop, and exact-output/refund logic. Each op maps to
   exactly one atomic order = one reply, so the existing aggregator state machine
   needs no nesting.
2. **Slippage = route-level `minimum_receive` only.** Remove the hardcoded 0.5%
   per-hop haircut. Intermediate orderbook hops place orders at the orderbook's
   own worst-price (already conservative); the single end-of-route
   `minimum_receive` check is the user's protection.
3. **Derive tick / direction / denoms from the market.** The op carries only
   `market_id` + `target_denom`. The offer denom is the market's *other* denom;
   `is_buy = (target_denom == market.base_denom)`; ticks come from the market.

## Two consumers, two execution modes

This contract serves **two** callers with opposing priorities (see
`orderbook_optimization_review.md` for the full analysis):

- **Choice dApp swaps** — caller does not know the book; wants accurate fills,
  minimal dust, and an on-chain `SimulateRoute` quote. → **Estimation mode**.
- **Arb bot** — rebuilds the book off-chain every block; wants minimal latency/gas
  and supplies the exact order parameters. → **Direct mode**.

Both emit the same `Stage → Split → Operation` route and the same end-of-route
`minimum_receive`. The contract does not special-case round trips; arb (start
asset == end asset) and a plain dApp A→B swap use identical machinery.

## New `OrderbookSwapOp`

```rust
// msg.rs
#[cw_serde]
pub struct OrderbookSwapOp {
    pub market_id: MarketId,           // single spot market to execute against
    pub target_denom: String,          // native denom we want OUT of this hop
    #[serde(default)]
    pub quantity: Option<FPDecimal>,   // direct mode: base qty to trade; None => estimate
    #[serde(default)]
    pub worst_price: Option<FPDecimal>,// direct mode: price bound;       None => estimate
}
```

Removed vs today: `swap_contract`, `offer_asset_info`, `ask_asset_info`,
`min_quantity_tick_size`. `target_denom` stays because the caller must declare the
hop's output to chain it anyway, and it lets us resolve direction with a single
market query.

- **Direct mode** (both `quantity` and `worst_price` supplied): issue the atomic
  order with no estimation queries; `minimum_receive` is the only net.
- **Estimation mode** (fields absent): walk the book for accurate sizing (dApp).
- `#[serde(default)]` is required so the dApp router can omit the optional fields —
  without it the absent-field JSON fails to deserialize.

### Planner integration (`reply.rs`)

`plan_next_stage`, `get_operation_input`, and `simulate`'s `get_path_start_info`
currently return `amm::AssetInfo` with no querier. For orderbook the offer denom is
*derived*, so these need access to the querier:

- Thread `&QuerierWrapper<InjectiveQueryWrapper>` into `plan_next_stage` and
  `get_operation_input` (callers in `proceed_to_next_step`, `execute_planned_swaps`,
  `handle_swap_reply` all have `deps`).
- `get_operation_input(OrderbookSwap)` → query the spot market, return
  `NativeToken { denom: <the side that isn't target_denom> }`.
- `get_operation_output(OrderbookSwap)` → `NativeToken { denom: target_denom }`
  (no query needed).

Orderbook input is always native, so conversion classification (native vs CW20) is
unchanged — only the concrete offer denom is now resolved by query rather than
passed in. CW20→native conversion *before* an orderbook hop still flows through the
existing `Awaiting::Conversions` / `Awaiting::PathConversion` paths.

> Lighter-weight alternative if querier-threading proves too invasive: keep an
> `offer_denom: String` on the op (plain native string, still no tick / no
> `AssetInfo` / no contract addr). Recommended path is full derivation from
> `market_id`; this is the fallback.

## Ported orderbook execution module (`orderbook_exec.rs`)

Port the *source/input* estimation path from the swap contract; drop the
target/exact-output variants.

Keep:
- `estimate_single_swap_execution` (single market only).
- `estimate_execution_buy_from_source`, `estimate_execution_sell_from_source`.
- `get_minimum_liquidity_levels`, `get_average_price_from_orders`,
  `get_worst_price_from_orders`, `get_effective_fee_discount_rate`.
- `round_to_min_tick` / `round_up_to_min_tick`, `dec_scale_factor`, `Scaled`.
- `FPCoin`, `StepExecutionEstimate`.

Drop: `SwapRoute`/routes, `steps_from`, `SwapQuantityMode`, `CurrentSwapOperation`,
`CurrentSwapStep`, `SWAP_*` cross-step state, `SwapExactOutput`, `GetInputQuantity`,
`*_from_target` estimators, `WithdrawSupportFunds` (covered by
`emergency_withdraw`).

### Execution (`create_swap_cosmos_msg`, orderbook arm)

Replace the current external-call arm with:

1. Query the spot market for `market_id` (base/quote/ticks).
2. `is_buy = target_denom == base_denom`. Validate the incoming `offer_asset_info`
   denom is the market's other side.
3. For a **sell** (input = base): round the input quantity *down* to
   `min_quantity_tick_size`. If the rounded quantity is zero → return
   `ContractError::AmountTooSmall` (replaces today's dead self-call to `{}`).
   Sub-tick dust remains in the contract balance (recoverable via
   `emergency_withdraw`; document this).
4. Build the order via `estimate_single_swap_execution(is_simulation=false)` →
   `worst_price`, quantity, `is_buy`; construct `SpotOrder` with
   `OrderType::{Buy,Sell}Atomic`, subaccount =
   `get_default_subaccount_id_for_checked_address(contract)`, fee recipient =
   the contract itself (self-relayer, preserves the fee-share discount).
5. `create_spot_market_order_msg(...)` as the submessage. It gets a normal
   aggregator `submsg_id` from `REPLY_ID_COUNTER` and is registered in
   `SUBMSG_REPLY_STATES` like any other op — `reply_on_success`.

The contract already holds the input funds during a route, so the in-execution
margin check (`funds_in_contract`, `is_simulation=false`) is satisfied exactly as
in the standalone contract.

### Reply handling (`handle_swap_reply`, orderbook branch)

Branch at the top of `handle_swap_reply` on
`matches!(replied_op, Operation::OrderbookSwap(_))`:

- Decode `MsgCreateSpotMarketOrderResponse` from
  `msg.result.into_result()?.msg_responses` (typed — no event-string parsing).
- Descale `price`, `quantity`, `fee` by `10^18`.
- `is_buy = target_denom == market.base_denom` (one market query, or cache base
  denom from the op's resolved input).
- Output amount: buy → `quantity`; sell → `quantity * price - fee`.
- Output asset = `NativeToken { denom: target_denom }`. Feed into the existing
  multi-hop / accumulation logic unchanged.

Delete the `swap_final_amount` / `wasm-atomic_swap_execution` parsing from
`parse_amount_from_swap_reply` (now dead).

### Simulation parity (`query.rs`)

Replace the orderbook arm of `simulate_single_operation`: instead of querying the
external `GetOutputQuantity`, call the ported
`estimate_single_swap_execution(is_simulation=true)` for the single market and
return `NativeToken { target_denom }` with `result_quantity`. Sim and execution now
share identical code → no divergence. Drop tick pre-rounding (the estimator rounds
internally).

## Config / state changes

- `Config`: add nothing required for orderbook (fee recipient = self). Keep
  existing `admin`, `cw20_adapter_address`, `fee_collector` (aggregator's own fee,
  distinct from orderbook trading fees).
- Remove from the ported code: `SWAP_ROUTES`, `SWAP_OPERATION_STATE`, `STEP_STATE`,
  `SWAP_RESULTS`, and the orderbook contract's own `CONFIG`.
- `error.rs`: add `AmountTooSmall {}`, `OrderResponseDecode { err }`,
  `InvalidOrderbookDenom { denom, market_id }`.

## Dependency reconciliation (workspace + `dex_aggregator/Cargo.toml`)

Target the **latest** injective libs (verified available + used by the proof).
This is not a small bump: the latest injective stack pulls **cosmwasm-std 3.0**,
a major version jump from the aggregator's current 2.2.

| crate | aggregator (now) | swap (now) | merged target (latest) | **ACTUAL (Step 1, built)** |
|---|---|---|---|---|
| cosmwasm-std | 2.2.2 | 2.1.0 | **3.0** | 3.0.7 ✓ |
| cosmwasm-schema | 2.2.2 | 2.1.1 | **3.0** | **stays 2.2.2** (see note 1) |
| cw-storage-plus | 2.0 | 2.0 | **3.0** (cw-std 3 compatible) | 3.0.1 ✓ |
| cw2 | 2.0 | 2.0 | **bump** | 3.0.0 ✓ |
| cw20 | 2.0 | 2.0 | **bump** | **vendored** (see note 2) |
| injective-cosmwasm | 0.3.4-1 | =0.3.0 | **0.3.6** | 0.3.6 ✓ |
| injective-math | 0.3.4-1 | 0.3.0 | **0.3.6** | 0.3.6 ✓ |
| injective-std | — | 1.13.0 | **add 1.19.0** | 1.19.0 ✓ |
| prost | — | 0.12.6 | **add 0.13** | 0.13 ✓ |
| thiserror | 2.0 | 1.0 | 2.0 | 2.0 ✓ |
| serde_with (transitive) | — | — | — | **pin =3.12.0** (see note 4) |
| darling (transitive) | — | — | — | **pin =0.20.11** (see note 4) |
| injective-test-tube (dev) | 1.16.3-1 | 1.13.2 | **1.19.0** | 1.19.0 (dev conflict, note 5) |

### Step 1 reconciliation notes (discovered while building, 2026-06-01)

The plan's "latest" column was right about cosmwasm-std/injective/cw-storage-plus/cw2
but wrong about cosmwasm-schema and cw20, and missed two API breaks + a toolchain wall.
All resolved; `./build_release.sh` produces MVP-clean wasm for both contracts.

1. **cosmwasm-schema stays on 2.2.2, NOT 3.0.** cosmwasm-schema 3.0's `#[cw_serde]`
   derives the new `cw-schema::Schemaifier`. injective-cosmwasm/injective-math 0.3.6
   types derive only `schemars::JsonSchema` (plain `#[derive(...)]`, not `cw_serde`),
   so nesting e.g. `FPDecimal` in a `cw_serde` type fails the `Schemaifier` bound.
   cosmwasm-schema 2.2.2 has **no cosmwasm-std dep** (pure schemars-0.8 macro crate),
   coexists with cosmwasm-std 3.0, and keeps the JsonSchema-only derive. schemars is
   0.8.x across the whole graph, so this is safe.
2. **No cw20 v3 exists.** Latest published cw20 is 2.0.0, locked to cosmwasm-std 2 via
   `cw_utils`. Vendored the used surface (`Cw20ExecuteMsg::{Transfer,Send}`,
   `Cw20QueryMsg::Balance`, `BalanceResponse`, `Cw20ReceiveMsg`) into
   `contracts/*/src/cw20.rs`; JSON wire format is byte-identical to upstream v2.
3. **Two cosmwasm-std 2→3 API breaks beyond the ones listed above:**
   - `Coin.amount` is now `Uint256` (was `Uint128`). Every funds/Coin/balance
     boundary needs a conversion: `x.into()` when building a `Coin`,
     `Uint128::try_from(coin.amount).map_err(StdError::from)?` when reading one.
   - `StdError` is opaque and no longer `PartialEq`, so `ContractError` can't
     `#[derive(PartialEq)]`. Tests that compared errors by value must use `matches!`.
   (Plus the already-listed `generic_err`→`msg` and dropping the `abort` feature.)
4. **Optimizer toolchain wall.** The latest cosmwasm optimizer image
   (`workspace-optimizer:0.17.0`) pins Rust 1.86, but cosmwasm-std 3.0.7 →
   cw-schema → serde_with 3.20 → darling 0.23 require rustc ≥1.88. Pinned
   serde_with=3.12.0 / darling=0.20.11 in Cargo.lock (still satisfy cw-schema's
   `>=3.9`). The build-std recipe from the proof was NOT needed — the optimizer's
   `wasm-opt -Os` yields reference-types-clean wasm once the graph builds. Keep
   Cargo.lock committed; do not `cargo update` those two upward (see build_release.sh).
5. **Dev-dep conflict deferred to Step 7.** `cw20-base` (dev) and the test harness
   still pull cosmwasm-std 2; `build_release.sh` only compiles lib targets so Step 1
   is unaffected, but `cargo test` will need the dev-deps reconciled (vendor/replace
   cw20-base usage or drop it) before the integration tests compile.

- **cosmwasm-std 2 → 3 migration is real work**, not a version string. Confirmed
  API breaks (from building the proof on 3.0):
  - `StdError::generic_err(..)` is **gone** → use `StdError::msg(..)`. The contract
    constructs these in many places (`execute.rs`, `reply.rs`).
  - The `abort` cosmwasm-std feature **no longer exists** (drop it from features).
  - `StdError` is now opaque (no variant enum); error matching/`thiserror`
    `#[from] StdError` still works but custom destructuring does not.
  - Re-verify cw-storage-plus / cw2 / cw20 each have a cw-std-3-compatible release
    and bump together (they are tightly version-locked to cosmwasm-std).
- **injective-std 1.19 chain change:** markets now carry `base_decimals` /
  `quote_decimals` natively (`MsgInstantSpotMarketLaunch` gained these fields, and
  `SpotMarket` exposes them). The merged contract can read decimals from the market
  instead of inferring them — relevant to estimation-mode sizing.
- v1.19 exposes both `v1beta1` and `v2` exchange message/query routes. The proof
  uses `v1beta1` end-to-end (matching `injective-cosmwasm 0.3.6` bindings); confirm
  the order-response `type_url` the bindings produce decodes as
  `v1beta1::MsgCreateSpotMarketOrderResponse` (the proof's probe asserts this).
- **Risk:** `injective-std` as a prod dep previously broke `ed25519-zebra` in the
  `choice_exchange` workspace. The proof workspace builds the probe to wasm cleanly
  with injective-std 1.19, so it is likely OK — still **verify `./build_release.sh`
  early**, not at the end.

## Testing

- Keep `mock_swap` + existing AMM/CLMM unit and integration tests.
- Port the swap contract's `injective-test-tube` single-market buy & sell tests
  into `tests/integration.rs` (orderbook can't be mocked — needs the exchange
  module; use a real local market like the standalone contract does).
- Add a route test mixing an AMM hop and an orderbook hop with a CW20→native
  conversion between them, asserting `minimum_receive` enforcement.
- Add a sub-tick sell test asserting `AmountTooSmall` rather than a silent failure.
- **Rebuild `./build_release.sh` before `cargo test`** (integration tests
  `include_bytes!` the artifacts).

## Migration / deploy

- New code ID; instantiate fresh (no state migration from the old aggregator —
  `ACTIVE_ROUTES` is transient). The standalone orderbook contract can be
  deprecated once the off-chain router emits single-`MarketId` orderbook ops.
- Off-chain router change: emit `OrderbookSwapOp { market_id, target_denom }`,
  splitting any multi-market orderbook route into one op per market. No more tick
  lookups.

## Step order for implementation

1. Bump/add deps; confirm `build_release.sh` compiles the unchanged aggregator
   with `injective-std` + `prost` added (dependency smoke test).
2. Add `orderbook_exec.rs` (ported estimators + helpers + types), compile in
   isolation.
3. Change `OrderbookSwapOp`; thread querier into planner/input resolution.
4. Rewrite `create_swap_cosmos_msg` orderbook arm.
5. Rewrite `handle_swap_reply` orderbook branch + decode; delete dead event parse.
6. Rewrite `simulate_single_operation` orderbook arm.
7. Tests; then deprecate the standalone contract.
