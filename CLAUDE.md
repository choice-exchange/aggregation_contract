# CLAUDE.md

## Project Overview

CosmWasm DEX aggregator smart contract for the Injective blockchain. Routes swaps through multiple AMM pools, orderbook contracts, and CLMM (Concentrated Liquidity) pools in parallel, multi-hop paths with automatic CW20/native token conversion. Also supports **FlashRoute** — capital-free CLMM flash-arb (borrow from a CLMM pool's `Flash {}`, run a cycle through other venues, repay principal+fee, keep the surplus; see `docs/flash_route_plan.md`). Cargo workspace with three members: `dex_aggregator` (main contract), `mock_swap` (test helper), and `mock_clmm_flash` (test flash-pool helper).

Mainnet deployments:

The same bytecode runs under **two instantiations with different threat models** — audit and
test every change against both:

- **Choice frontend — `inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0`.** Permissionless; many
  unrelated users. Routes are suggested by the Choice router but *submitted by the user*, so
  every field of `stages` is attacker-controlled (and `pool_address` has **no allowlist**).
- **Arb bot — `inj1vhu5z87dcuyyuz9e725kasecqygprl6jpkj7hx`.** Effectively single-caller;
  `FlashRoute` additionally gated by `FLASH_SIGNERS`. Latency- and gas-sensitive; the concern
  is economic correctness — does `SimulateRoute` agree with what execution does.

Both currently run **Code ID 2060 (v2.0.1)**; the v2.0.1 migration is DONE. wasm-admin on both
is the Choice Admin Timelock `inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9` (48h delay).

- **v2.2.0 — built, NOT uploaded, NOT migrated.** Audit fixes: routes bounded to their own
  funds, fee ordering, direct-mode tick snapping, gate parity. See
  [docs/v2_2_0_changes.md](docs/v2_2_0_changes.md) — read it before touching
  `snapshot_entry_balances`, `finalize_route`, or `build_swap_order_msg`. Supersedes the
  unshipped v2.1.0 ([docs/v2_1_0_changes.md](docs/v2_1_0_changes.md), still accurate as
  background).
- **v1 (pre-merge) — legacy:** `inj1a4qvqym6ajewepa7v8y2rtxuz9f92kyq2zsg26` (Code ID 1892,
  AMM/orderbook only, no CLMM). wasm-admin = `inj1yrg4pg8…`.

Live state worth knowing (verified on chain 2026-07-30): `FEE_MAP` is **empty** on both
instances (`all_fees` → `[]`), and flash is **switched off** — `flash_signers` is empty and
`unrestricted` is false on both, so `FlashRoute` is deny-all for everyone including the admin.

⚠️ **The timelock is only the *wasm* admin.** `config.admin` on the Choice instance is the hot
EOA `inj1yrg4pg8hcu0sw5rjlrcqfmw2ewf2uztlmdysak` (key `choicedev`), so `EmergencyWithdraw`,
`UpdateCw20Adapter`, `UpdateFeeCollector`, `UpdateAdmin` and the flash-signer setters are one
key away from a 48h-delay-free change — and `UpdateCw20Adapter` repoints every conversion, so
it reaches users' in-flight funds. Migration is the only action the timelock actually gates.

## Build, Test, and Deploy Commands

```bash
# Development build
cargo build

# Production WASM build (uses cosmwasm/workspace-optimizer:0.17.0 Docker image)
# Outputs dex_aggregator.wasm, mock_swap.wasm, mock_clmm_flash.wasm to ./artifacts/
# (runs --locked; refresh Cargo.lock with a local `cargo build` FIRST or the
#  optimizer aborts on a stale lock. Triggered by adding a member/dep AND by
#  bumping a crate version — the lock records the version too. Failure looks like
#  "the lock file needs to be updated but --locked was passed" + a panic at
#  pkg_build.rs, exit 101. ⚠️ It leaves the PREVIOUS artifacts in place, so a
#  following `cargo test` passes against stale wasm — always check the exit code.)
./build_release.sh

# Run tests (MUST run ./build_release.sh first — see note below)
cargo test

# Run a single test
cargo test <test_name> -- --nocapture

# Lint
cargo clippy --all-targets

# Generate JSON schemas to contracts/dex_aggregator/schema/
cd contracts/dex_aggregator && cargo run --example schema
```

**CRITICAL: Run `./build_release.sh` before `cargo test`.** Integration tests use `include_bytes!` to embed WASM artifacts at compile time. Tests will fail to compile or test stale code if artifacts aren't rebuilt after source changes.

### Deployment (uses `injectived` CLI)

```bash
./scripts/upload_code_mainnet.sh    # Upload new code to mainnet
./scripts/deploy_mainnet.sh         # Instantiate on mainnet (edit CODE_ID first)
./scripts/deploy_testnet.sh         # Deploy on testnet (chain ID: injective-888)
```

## Architecture

### Key Files (contracts/dex_aggregator/src/)

| File | Purpose |
|------|---------|
| `contract.rs` | Entry points: `instantiate`, `execute`, `query`, `reply`. Routes `ExecuteMsg` variants to handlers (incl. `FlashRoute` / `FlashCallback`). |
| `msg.rs` | All message types. Submodules: `amm`, `orderbook`, `clmm`, `cw20_adapter`, `reflection`. Defines `Stage > Split > Operation` route structure. `clmm::ClmmPoolFlashMsg` + `ExecuteMsg::FlashRoute`/`FlashCallback` for flash-arb. |
| `execute.rs` | Core swap logic (`execute_aggregate_swaps_internal`, `create_swap_cosmos_msg`). Flash entry points: `execute_flash_route` (fires the pool's `Flash`), `execute_flash_callback` (borrower callback → runs the cycle via `proceed_to_next_step`). Admin functions: `set_fee`, `remove_fee`, `update_fee_collector`, `update_admin`, `emergency_withdraw`, `register_tax_token`, `deregister_tax_token`. |
| `reply.rs` | Submessage reply state machine. Manages `Awaiting` states. Core function `proceed_to_next_step` drives stage-by-stage execution. Fee deduction via `apply_fee` at path completion. `finalize_route` disposes the final output: pay the user, or (flash) repay `principal+fee` to the pool and forward the surplus. |
| `state.rs` | Storage: `CONFIG`, `FEE_MAP`, `ACTIVE_ROUTES`, `SUBMSG_REPLY_STATES`, `REPLY_ID_COUNTER`, `TAX_TOKEN_REGISTRY`, `PENDING_FLASH`. Defines `ExecutionState`, `SubmsgReplyState`, `Awaiting`, `RoutePlan` (with `flash_repayment`), `FlashRepayment`, `PendingFlashCtx`. |
| `query.rs` | `simulate_route`, `query_config`, `query_fee_for_pool`, `query_all_fees`. Contains unit tests. |
| `error.rs` | `ContractError` enum with `thiserror`. |

### Execution Flow

1. User calls `ExecuteRoute` (native funds) or sends CW20 via `Receive` hook. `minimum_receive` is **mandatory and must be > 0** (`ZeroMinimumReceive`); a CW20 `Receive` whose hook fails to deserialize **reverts** (`InvalidCw20Hook`) — it never keeps the tokens.
2. `execute_aggregate_swaps_internal` validates input, **snapshots the contract's pre-route balance of every touched denom** (offer + every op input) into `ExecutionState.entry_balances`, creates `ExecutionState`, calls `proceed_to_next_step`
3. Each stage: calculates per-split amounts, dispatches CW20/native conversions if needed (`Awaiting::Conversions`)
4. Executes parallel swap submessages, each tracked by unique reply IDs in `SUBMSG_REPLY_STATES`
5. `handle_swap_reply` processes each reply; for multi-hop paths, chains to next operation. Orderbook BUY hops capture their price-improvement surplus (sized at `worst_price`, filled cheaper) into `ExecutionState.pending_fees`
6. Mid-path conversions handled via `Awaiting::PathConversion`
7. After final stage: normalizes output assets (`Awaiting::FinalConversions`), checks `minimum_receive`, sends the tracked output to the user, then **`build_residue_sweep` drains every touched denom** (current − entry baseline): the `pending_fees` portion → fee collector, the remainder (unfilled orderbook remainders, dropped intermediates, un-spent split inputs, no-fills) → the user. **Invariant: a successful route leaves nothing in the contract.**

### Supporting Contracts

- `mock_swap` (`contracts/mock_swap/src/lib.rs`) — Mock DEX with configurable rates, supports AMM/Orderbook/CLMM protocol types, used in integration tests
- `mock_clmm_flash` (`contracts/mock_clmm_flash/src/lib.rs`) — Mock CLMM flash-loan pool: faithfully mirrors `choice_clmm_pool`'s flash interface (lend → `FlashCallback` → balance-delta repayment check + reentrancy lock + `GetConfig`) at the JSON wire level. The flash source in the `FlashRoute` integration tests; the aggregator is the borrower, so no separate borrower mock is needed. (The real pool can't be embedded — `choice_exchange` is cosmwasm-std 2.x vs this workspace's 3.x.)
- `cw20_adapter` and `cw20_base` — Pre-compiled WASMs in project root, not built from this workspace

## Code Conventions

### Naming
- `snake_case` for functions, variables, module names
- `PascalCase` for types, enums, structs, enum variants
- `UPPER_SNAKE_CASE` for constants

### Patterns
- Entry points use Injective custom types: `DepsMut<InjectiveQueryWrapper>`, `Response<InjectiveMsgWrapper>`
- Messages use `#[cw_serde]` macro; query enum uses `#[derive(QueryResponses)]` with `#[returns(...)]`
- Error handling: `ContractError` enum via `thiserror`, propagated with `?`
- State: `cw-storage-plus` types — `Item<T>` for singletons, `Map<K, V>` for key-value stores
- Execute handlers return `Result<Response<InjectiveMsgWrapper>, ContractError>`
- Query handlers return `StdResult<Binary>`
- Admin checks: `info.sender != config.admin` → `ContractError::Unauthorized {}`
- Response attributes for tracking: `.add_attribute("action", "...")`

### Asset Handling
- `amm::AssetInfo` enum: `Token { contract_addr }` (CW20) or `NativeToken { denom }` (bank)
- Tax tokens in `TAX_TOKEN_REGISTRY` use `reflection::ExecuteMsg::TaxExemptTransfer` / `TaxExemptSend`
- CW20 tokens sent to pools via `Cw20ExecuteMsg::Send`; native tokens as `funds` in `WasmMsg::Execute`

### Submessage Reply Pattern
- Each swap gets a unique `submsg_id` from `REPLY_ID_COUNTER` (monotonically incrementing)
- `SubmsgReplyState` maps `submsg_id` → `master_reply_id`, `split_index`, `op_index`
- `ExecutionState` stored in `ACTIVE_ROUTES` keyed by `master_reply_id`
- All submessages use `SubMsg::reply_on_success`
- Reply *amounts* come from wasm event attributes: `return_amount` (AMM), `amount_out` (CLMM), `post_tax_amount` (tax tokens); orderbook hops decode the typed spot-order response instead
- Reply *assets* are **not** read from the reply. A hop's output `AssetInfo` is resolved before it runs — from the op's `ask_asset_info`, else the pool's `Pair {}`/`GetConfig {}`. Trusting the venue's `ask_asset` let an arbitrary pool name any asset it liked (v2.2.0 §1)

## Testing

- **Integration tests** (`tests/integration.rs`): Uses `injective-test-tube` for local chain simulation. `setup()` deploys all contracts, returns `TestEnv` with admin/user accounts and contract addresses.
- **Unit tests** (`contracts/dex_aggregator/src/query.rs`): Simulation and fee query tests using `mock_dependencies()`.
- Mock swap contracts configured with `SwapConfig { rate, protocol_type, input_decimals, output_decimals, ... }`.
- WASM artifacts loaded via `include_bytes!` — stale artifacts mean stale tests.

## Important Notes

- `reply.rs` is the most complex module — state machine changes require careful review of all `Awaiting` state transitions
- **The gate must see what reverts (v2.1.0).** `SimulateRoute` is the arb bot's pre-broadcast gate. Anything that changes what the executor actually submits MUST be visible to the query path, or the gate clears routes that are guaranteed to fail. The two paths deliberately share `estimate_single_swap_execution`, `sell_base_quantity`, `meets_min_notional` and `apply_fee` — adding an execution-side adjustment outside that shared code re-opens the hole. Four separate bugs were this one mistake.
- Orderbook swaps only support native token inputs/outputs. **Both** the estimator and the order builder floor quantity via `sell_base_quantity`, and both check `meets_min_notional`. **Direct mode skips the estimators entirely**, so `direct_order_params` is the only place that snaps its quantity/price to the market ticks and bounds it by what the hop holds — and `SimulateRoute` sizes through the same helper. A hop that cannot place an order is a graceful zero-value path, never a route-wide revert.
- **Never read a hop's output asset from its reply.** It is resolved before the hop runs — `ask_asset_info` if supplied, else the pool's `Pair {}`/`GetConfig {}`. The venue is not an authority on what it produced; `pool_address` has no allowlist.
- CLMM swaps support both native and CW20 tokens; no rounding needed. In estimation mode a pre-execution `Quote` computes `minimum_amount_out` less `ClmmSwapOp::slippage_bps` (default 50 = 0.5%).
- **Never send `max_spread: None` to an AMM pair.** Choice's `assert_max_spread` no-ops on `None`, but Astroport substitutes its own 0.5% and asserts — invisibly to `SimulateRoute`. `AmmSwapOp::max_spread` defaults to and is clamped at 49%.
- **Splits within a stage are quoted against the same pre-stage state** — every message is built before any executes, and `simulate_route` is a single snapshot. Two splits on one venue over-quote. Supported (and tested), so not rejected; merge them or use separate stages.
- ⚠️ **`ob_buy_surplus` OVERESTIMATES** the orderbook buy refund by a few atomic units. Every consumer must clamp against real balance, the way `build_residue_sweep` does with `.min(residue)`. Crediting it raw makes `finalize_route` promise more than it holds and reverts the whole route.
- **The protocol's orderbook carve is price improvement ONLY** — `(order_price − fill.price) × filled`. Everything else a buy leaves over is the user's: the quantity-tick flooring loss, the `(1 + fee)` sizing headroom, the relayer rebate, the margin against unfilled base. Measuring the user's share as "the unfilled fraction" instead made it exactly zero on every complete fill (v2.2.0 §4).
- `FPDecimal` (from `injective-math`) for orderbook quantities; `Uint128`/`Decimal` (from `cosmwasm-std`) for everything else (including CLMM)
- Fees are *charged* at path completion (end of a split's operation chain) but *paid* at finalize, via `pending_pool_fees`. Sending them from the reply that charged them raced the residue sweep — see v2.2.0 §3
- **Fund-safety invariants.** The engine tracks amounts *virtually*: a hop's output is a number the venue reported, and `pool_address` is caller-supplied with no allowlist. Two invariants hold it together, and BOTH are load-bearing.
  1. **A route may only move what it brought in (v2.2.0).** `snapshot_entry_balances` baselines the offer denom plus every op's input *and output*; `route_spendable = current − baseline` bounds every swap input, mid-path carry, adapter conversion, and the final payout (`OutputNotBacked`). A no-op on honest routes — the contract necessarily holds at least the tracked output — but on a hop that reported an output it never delivered the credit collapses to zero and the route dies at its own `minimum_receive`. Fails closed (`UnsnapshottedAsset`). Both the dispatch clamp and the payout clamp are needed: clamping only the payout still lets a fabricated intermediate be *spent* through a real pool.
  2. **A successful route leaves nothing behind (v2.0.1).** `build_residue_sweep` drains every touched denom: accrued fees → fee collector (`pending_pool_fees` = the per-pool `FEE_MAP` carve, always owed; `pending_fees` = orderbook buy price improvement, suppressed on flash cycles where it is the caller's own arb profit and credited into what `min_profit` measures), everything else → the user. The final-output denom is netted against the tracked payout, not skipped — skipping it stranded the buy surplus on every CLOSED cycle.
  `minimum_receive == 0` is rejected so a route that produces nothing can never "succeed" returning nothing. Regression tests: `test_hostile_pool_cannot_drain_contract_balance`, `test_ob_buy_unspent_input_returns_to_user`, `test_fee_on_non_final_stage_is_paid_once_to_collector`, `test_orderbook_surplus_to_fee_collector_and_contract_drains`, `test_amm_route_leaves_no_residue`, `test_zero_minimum_receive_is_rejected`.
- **FlashRoute** (`docs/flash_route_plan.md`): a flash-arb cycle must repay in the *borrowed* asset, so it ends in `flash_asset` (gated by `min_profit`), not an A→B user swap. The whole cycle runs depth-first inside the pool's `FlashCallback`, so repayment settles before the pool's repay check — the pool reverts the tx if unrepaid. Repay uses the same Bank `Send` / CW20 `Transfer` (never CW20 `Send`) the pool requires. `FlashCallback` is gated on `PENDING_FLASH` + `info.sender == flash_pool`; the cycle may not route through `flash_pool` (reentrancy lock).
- CI (`.github/workflows/test.yml`) runs `cargo build --verbose && cargo test --verbose` on push/PR to main
