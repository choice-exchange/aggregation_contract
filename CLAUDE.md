# CLAUDE.md

## Project Overview

CosmWasm DEX aggregator smart contract for the Injective blockchain. Routes swaps through multiple AMM pools, orderbook contracts, and CLMM (Concentrated Liquidity) pools in parallel, multi-hop paths with automatic CW20/native token conversion. Cargo workspace with two members: `dex_aggregator` (main contract) and `mock_swap` (test helper). Deployed on Injective mainnet (Code ID 1892, address `inj1a4qvqym6ajewepa7v8y2rtxuz9f92kyq2zsg26`).

## Build, Test, and Deploy Commands

```bash
# Development build
cargo build

# Production WASM build (uses cosmwasm/workspace-optimizer:0.17.0 Docker image)
# Outputs to ./artifacts/dex_aggregator.wasm and ./artifacts/mock_swap.wasm
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
| `contract.rs` | Entry points: `instantiate`, `execute`, `query`, `reply`. Routes `ExecuteMsg` variants to handlers. |
| `msg.rs` | All message types. Submodules: `amm`, `orderbook`, `clmm`, `cw20_adapter`, `reflection`. Defines `Stage > Split > Operation` route structure. |
| `execute.rs` | Core swap logic (`execute_aggregate_swaps_internal`, `create_swap_cosmos_msg`). Admin functions: `set_fee`, `remove_fee`, `update_fee_collector`, `update_admin`, `emergency_withdraw`, `register_tax_token`, `deregister_tax_token`. |
| `reply.rs` | Submessage reply state machine. Manages `Awaiting` states. Core function `proceed_to_next_step` drives stage-by-stage execution. Fee deduction via `apply_fee` at path completion. |
| `state.rs` | Storage: `CONFIG`, `FEE_MAP`, `ACTIVE_ROUTES`, `SUBMSG_REPLY_STATES`, `REPLY_ID_COUNTER`, `TAX_TOKEN_REGISTRY`. Defines `ExecutionState`, `SubmsgReplyState`, `Awaiting` enum. |
| `query.rs` | `simulate_route`, `query_config`, `query_fee_for_pool`, `query_all_fees`. Contains unit tests. |
| `error.rs` | `ContractError` enum with `thiserror`. |

### Execution Flow

1. User calls `ExecuteRoute` (native funds) or sends CW20 via `Receive` hook
2. `execute_aggregate_swaps_internal` validates input, creates `ExecutionState`, calls `proceed_to_next_step`
3. Each stage: calculates per-split amounts, dispatches CW20/native conversions if needed (`Awaiting::Conversions`)
4. Executes parallel swap submessages, each tracked by unique reply IDs in `SUBMSG_REPLY_STATES`
5. `handle_swap_reply` processes each reply; for multi-hop paths, chains to next operation
6. Mid-path conversions handled via `Awaiting::PathConversion`
7. After final stage: normalizes output assets (`Awaiting::FinalConversions`), checks `minimum_receive`, sends to user

### Supporting Contracts

- `mock_swap` (`contracts/mock_swap/src/lib.rs`) — Mock DEX with configurable rates, supports AMM/Orderbook/CLMM protocol types, used in integration tests
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
- Reply amounts parsed from wasm event attributes: `return_amount` (AMM), `swap_final_amount` (orderbook), `amount_out` (CLMM), `post_tax_amount` (tax tokens)

## Testing

- **Integration tests** (`tests/integration.rs`): Uses `injective-test-tube` for local chain simulation. `setup()` deploys all contracts, returns `TestEnv` with admin/user accounts and contract addresses.
- **Unit tests** (`contracts/dex_aggregator/src/query.rs`): Simulation and fee query tests using `mock_dependencies()`.
- Mock swap contracts configured with `SwapConfig { rate, protocol_type, input_decimals, output_decimals, ... }`.
- WASM artifacts loaded via `include_bytes!` — stale artifacts mean stale tests.

## Important Notes

- `reply.rs` is the most complex module — state machine changes require careful review of all `Awaiting` state transitions
- Orderbook swaps only support native token inputs/outputs; amounts rounded to `min_quantity_tick_size`
- CLMM swaps support both native and CW20 tokens; no rounding needed. Pre-execution `Quote` query computes `minimum_amount_out` with 0.5% slippage
- `FPDecimal` (from `injective-math`) for orderbook quantities; `Uint128`/`Decimal` (from `cosmwasm-std`) for everything else (including CLMM)
- Fees deducted at path completion (end of a split's operation chain), not per-operation
- CI (`.github/workflows/test.yml`) runs `cargo build --verbose && cargo test --verbose` on push/PR to main
