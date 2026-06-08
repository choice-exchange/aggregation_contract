# Orderbook-Merge — Progress / Resume Notes

Living status doc for folding `inj-orderbook-swap-contract` into `dex_aggregator`.
Last updated **2026-06-08**. Design lives in
[orderbook_merge_plan.md](orderbook_merge_plan.md); optimization analysis in
[orderbook_optimization_review.md](orderbook_optimization_review.md); the test-tube
proof in [orderbook_proof_results.md](orderbook_proof_results.md).

## Where we are

Working on branch **`orderbook-merge`** (off `clmm`) in this repo. Steps 1–2 are
committed; **steps 3–6 are done in the working tree (UNCOMMITTED) as of 2026-06-08**.
Same-repo modification of `dex_aggregator` — **not** a new repo (it reuses the
Stage→Split→Operation route, reply state machine, cw20_adapter conversions). Deploy
is a fresh code ID. Treated as a **new contract version not yet in use**, so the ABI
took the full market-derived redesign (not the `offer_denom: String` fallback).

> Note: an unrelated working-tree of per-op slippage + CLMM exact-output + orderbook
> integration docs was split off to branch **`clmm-exact-output`** (commit `b2971c6`,
> pushed) before resuming the merge. That branch's `OrderbookSwapOp` kept the *old*
> `offer/ask/min_quantity_tick_size` shape — it is a different lineage and will need
> reconciling if/when both land.

| Step | State | Commit |
|---|---|---|
| 1. Bump/add deps + cosmwasm-std 2→3 migration; `build_release.sh` compiles MVP-clean | ✅ done | `51faaf6` |
| 2. Port `orderbook_exec.rs` (estimators/helpers/types), compile in isolation | ✅ done | `8191592` |
| 3. Change `OrderbookSwapOp`; thread querier into planner/input resolution | ✅ done | uncommitted |
| 4. Rewrite `create_swap_cosmos_msg` orderbook arm (atomic SpotOrder, self-relayer) | ✅ done | uncommitted |
| 5. Rewrite `handle_swap_reply` orderbook branch + typed decode; delete dead event parse | ✅ done | uncommitted |
| 6. Rewrite `simulate_single_operation` orderbook arm (shared estimator) | ✅ done | uncommitted |
| 7. Tests (reconcile integration dev-deps; port buy/sell + mixed-route + sub-tick); rebuild artifacts | ⬜ next | — |

`cargo build` (workspace), `cargo clippy --lib -p dex_aggregator`, and
`cargo test -p dex_aggregator --lib` (15 tests) are all green. Artifacts in
`artifacts/` are still the Step-1 build — **rebuild via `./build_release.sh` before
`cargo test`** (integration tests `include_bytes!` them) once Step 7 lands.

## What steps 3–6 actually did (2026-06-08)

- **ABI (msg.rs):** `OrderbookSwapOp { market_id: MarketId, target_denom: String,
  #[serde(default)] quantity: Option<FPDecimal>, #[serde(default)] worst_price:
  Option<FPDecimal> }`. Removed `swap_contract`/`offer_asset_info`/`ask_asset_info`/
  `min_quantity_tick_size`. Deleted the dead `msg::orderbook` module (external-contract
  `GetOutputQuantity`/`SwapMinOutput`/`SwapEstimationResult`).
- **orderbook_exec.rs:** added `load_market`, `offer_denom_for`, `is_buy_for_target`,
  `build_swap_order_msg` (direct vs estimation mode; returns `Ok(None)` ⇒ caller raises
  `AmountTooSmall`), `parse_order_output` (typed `MsgCreateSpotMarketOrderResponse`
  decode, descale 10^18, buy⇒quantity / sell⇒quantity·price−fee).
- **execute.rs:** orderbook arm places a native atomic `SpotOrder` via
  `create_spot_market_order_msg`, subaccount = contract's default, fee recipient = self
  (relayer discount). Validates the offer denom is the market side opposite
  `target_denom` (`InvalidOrderbookDenom`).
- **reply.rs:** `handle_swap_reply` decodes the typed order response for orderbook ops
  (event-attr parse only for AMM/CLMM now); zero fill ⇒ existing zero-value-path. Final
  op skips the `FEE_MAP`/`addr_validate` fee step for orderbook (no pool address; the
  exchange already charges its own fee). `get_operation_input`/`plan_next_stage` now take
  `Deps<InjectiveQueryWrapper>` to derive the offer denom from the market;
  `get_operation_output`/`get_operation_address` need no query.
- **query.rs + contract.rs:** query entry point is now `Deps<InjectiveQueryWrapper>`
  (`.into_empty()` for the non-simulate handlers); `simulate_single_operation` orderbook
  arm uses the **same** `estimate_single_swap_execution` as execution (sim/exec parity).

## How to resume

```bash
cd choice/aggregation_contract
git branch --show-current        # expect: orderbook-merge
git status -s                    # steps 3-6 are uncommitted working-tree changes
cargo build                      # fast local check
cargo test -p dex_aggregator --lib   # 15 passing
```

Then start **Step 7** (tests): the dev-dep blocker (`injective-test-tube` on cw-std 2 vs
our cw-std 3) must be reconciled before `tests/integration.rs` compiles. Orderbook can't
be mocked — needs a real local market (registered denom decimals + a min-notional via gov
`BatchExchangeModificationProposal`), per the proof.

## Step 1 outcome — dependency reality (plan table was wrong in places)

`injective-cosmwasm 0.3.6` **requires cosmwasm-std 3.0.5**, so the 2→3 migration is
forced. Resolved deviations from the plan's dep table (full writeup in the plan's
"Step 1 reconciliation notes"):

- **cosmwasm-schema stays 2.2.2, NOT 3.0.** 3.0's `#[cw_serde]` derives `cw-schema`
  `Schemaifier`; injective 0.3.6 types are `JsonSchema`-only and don't impl it. 2.2.2
  has no cosmwasm-std dep and keeps the JsonSchema-only derive. schemars is 0.8.x graph-wide.
- **No cw20 v3 exists** (2.0.0 → cw_utils → cosmwasm-std 2 conflict). Vendored the used
  surface into `contracts/{dex_aggregator,mock_swap}/src/cw20.rs` (byte-identical JSON).
- **cosmwasm-std 3.0 API breaks beyond the obvious:** `Coin.amount` is now `Uint256`
  (convert at every funds/Coin/balance boundary: `.into()` to build,
  `Uint128::try_from(c.amount).map_err(StdError::from)?` to read); `StdError` is opaque
  and not `PartialEq` → dropped `#[derive(PartialEq)]` on `ContractError` (tests use
  `matches!`). Plus `generic_err`→`msg`, drop the removed `abort` feature.
- **Optimizer toolchain wall.** `workspace-optimizer:0.17.0` is the latest image (Rust
  1.86) but the cw-std-3 graph (cw-schema → serde_with 3.20 → darling 0.23) needs rustc
  ≥1.88. Pinned `serde_with=3.12.0` / `darling=0.20.11` in the committed `Cargo.lock`
  (satisfy cw-schema's `>=3.9`). **Do NOT `cargo update` those upward** until a newer
  optimizer ships (see header comment in `build_release.sh`). The proof's `-Z build-std`
  MVP recipe was NOT needed — `wasm-opt -Os` strips post-MVP features once the graph builds.
- **Verified MVP-clean:** `wasm-tools validate --features=-reference-types` passes on both
  artifacts (same as the deployed baseline).

## Step 2 outcome — `orderbook_exec.rs`

Ported from the standalone contract's `queries.rs`/`helpers.rs`/`types.rs`. Kept the
single-market **from-source** path only:
`estimate_single_swap_execution`, `estimate_execution_{buy,sell}_from_source`,
`get_minimum_liquidity_levels`, `get_average/worst_price_from_orders`,
`get_effective_fee_discount_rate`, `round_up_to_min_tick`, `dec_scale_factor`, `Scaled`,
`FPCoin`, `StepExecutionEstimate`. Dropped routes/`steps_from`, `*_from_target`,
`SwapQuantityMode`/`SwapEstimationAmount`, `CurrentSwap*`, `SWAP_*` state, exact-output.

Key adaptations: aggregator **always self-relays** → fee discount always applied, no
`Config` lookup (`is_self_relayer = true`, contract address passed in); `FPCoin`/
`StepExecutionEstimate` are plain `derive`s (not `cw_serde` — internal, no schema);
`is_buy = input.denom != market.base_denom` (paying quote ⇒ buy base).

**Verified facts that the next steps rely on** (from the proof): atomic spot orders are
IOC/partial-fill (oversize doesn't revert); a loose `worst_price` is accepted (no band
reject on a plain market); no-reply chaining works (G6) — so direct-mode arb can fire all
orders in one execute and check `minimum_receive` once.

## Watch-outs for steps 3–7

- **Step 3 (ABI):** `OrderbookSwapOp` → `{ market_id, target_denom, quantity: Option<FPDecimal>,
  worst_price: Option<FPDecimal> }` with `#[serde(default)]` on the optionals (dApp omits
  them). Removes `swap_contract`, `offer_asset_info`, `ask_asset_info`,
  `min_quantity_tick_size`. Thread `&QuerierWrapper<InjectiveQueryWrapper>` into
  `plan_next_stage`, `get_operation_input`, and simulate's `get_path_start_info` so the
  offer denom is derived from the market (the *other* side vs `target_denom`). Fallback if
  too invasive: keep a plain `offer_denom: String` on the op.
- **Step 4:** round sell input DOWN to `min_quantity_tick_size`; zero ⇒ `AmountTooSmall`
  (replaces the dead self-call to `{}`). Subaccount =
  `get_default_subaccount_id_for_checked_address(contract)`; fee recipient = the contract
  (self-relayer). `OrderType::{Buy,Sell}Atomic`. Add error variants `AmountTooSmall`,
  `OrderResponseDecode`, `InvalidOrderbookDenom` to `error.rs`.
- **Step 5:** decode `v1beta1::MsgCreateSpotMarketOrderResponse` from
  `msg.result.into_result()?.msg_responses` (typed). Descale price/qty/fee by 10^18. Output:
  buy ⇒ `quantity`; sell ⇒ `quantity*price - fee`. Delete `swap_final_amount` /
  `wasm-atomic_swap_execution` parsing in `parse_amount_from_swap_reply` (now dead).
- **Step 7 dev-dep blocker:** `tests/integration.rs` mixes `injective-test-tube` (cosmwasm-std
  2) with our cosmwasm-std-3 types → won't compile until reconciled. Lib unit tests are
  unaffected (proven in Step 2). Orderbook can't be mocked — needs a real local market with
  registered denom decimals + min-notional (gov `BatchExchangeModificationProposal`), per the
  proof. Rebuild `./build_release.sh` before `cargo test` (integration `include_bytes!` the artifacts).
