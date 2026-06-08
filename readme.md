# Choice Aggregation Contract

## Mainnet Deployment

Code Id: 1892

Address: `inj1a4qvqym6ajewepa7v8y2rtxuz9f92kyq2zsg26`

## Getting Started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (latest stable version recommended)
- `rustup` target for Wasm: `rustup target add wasm32-unknown-unknown`
- [Docker](https://www.docker.com/get-started/) for reproducible production builds.

---

## Build

### Development Build

This command compiles the contract to Wasm for testing or local development.

```bash
cargo build --release --target wasm32-unknown-unknown
```

The output will be an unoptimized Wasm file located at `./target/wasm32-unknown-unknown/release/dex_aggregator.wasm`.

### Production Build (Recommended)

For deploying the contract to a live network, it is crucial to use the CosmWasm optimizer. This produces a much smaller and more efficient binary, which saves significant gas fees.

This command uses a Docker container to create a reproducible, optimized build in the `./artifacts/` directory.

```bash
# First, create the output directory if it doesn't exist
mkdir -p artifacts

# Run the optimizer
docker run --rm -v "$(pwd)":/code \
  --mount type=volume,source="$(basename "$(pwd)")_cache",target=/code/target \
  --mount type=volume,source=registry_cache,target=/usr/local/cargo/registry \
  cosmwasm/workspace-optimizer:0.17.0
```

The final, deployable contract will be located at `./artifacts/dex_aggregator.wasm`.

---

## Testing

The test suite uses `injective-test-tube`, which provides a high-fidelity testing environment by running a local Injective chain instance.

**Important:** The test framework requires the Wasm binary to be compiled *before* the tests are run.

```bash
# 1. Build the Wasm artifacts first (rebuilds every workspace member, including a
#    fresh dex_aggregator.wasm — a stale artifact tests old code).
./build_release.sh

# 2. Run the tests
cargo test
```

> If you add a workspace member or dependency, refresh `Cargo.lock` with a local
> `cargo build` before `./build_release.sh` — the optimizer runs `--locked` and
> aborts on a stale lock.

## Project Structure

This project is organized as a CosmWasm workspace, containing multiple interdependent smart contracts. The integration tests orchestrate interactions between all of these contracts to simulate real-world scenarios.

```
AGGREGATION_CONTRACT/
│
├── .github/workflows/      # Continuous integration workflows (e.g., running tests).
│   └── test.yml
│
├── artifacts/              # Stores the compiled and optimized .wasm files for each contract.
│
├── contracts/              # Source code for all smart contracts in the workspace.
│   │
│   ├── dex_aggregator/     # The core aggregation contract.
│   │   └── src/
│   │       ├── contract.rs     # Core entry points (instantiate, execute, query, reply).
│   │       ├── error.rs        # Custom contract error types.
│   │       ├── execute.rs      # Handlers for execute messages.
│   │       ├── lib.rs          # Crate root module declarations.
│   │       ├── msg.rs          # Message definitions and Route structures.
│   │       ├── query.rs        # Handlers for query messages.
│   │       ├── reply.rs        # Logic for handling submessage replies.
│   │       └── state.rs        # State definitions and storage management.
│   │
│   ├── mock_swap/          # A mock DEX contract used for integration testing. It simulates
│   │                       # AMM, Orderbook, and CLMM behavior with predictable rates.
│   │
│   └── mock_clmm_flash/    # A mock CLMM flash-loan pool for the FlashRoute integration
│                           # tests. Faithfully mirrors choice_clmm_pool's flash interface
│                           # (lend → FlashCallback → balance-delta repayment check +
│                           # reentrancy lock + GetConfig) at the JSON wire level.
│
├── cw20_adapter/           # Pre-compiled WASM (not a workspace member): converts between
│                           # native tokenfactory denoms and their CW20 equivalents.
│
├── cw20_base/              # Pre-compiled WASM (not a workspace member): the standard CW20
│                           # token (e.g. for SHROOM, SAI), used by the integration tests.
│
├── docs/                   # Design docs (e.g. flash_route_plan.md).
│
├── scripts/                # Deploy/upload helpers: deploy_mainnet.sh, deploy_testnet.sh,
│                           # upload_code_mainnet.sh (use `injectived`).
│
├── tests/                  # Workspace-level integration tests, driven by
│                           # `injective-test-tube` (spins up a local chain environment).
│
├── artifacts/              # Optimized .wasm output from build_release.sh.
├── build_release.sh        # Builds optimized, production-ready .wasm for every member.
├── Cargo.lock              # Records the exact versions of all dependencies.
├── Cargo.toml              # The workspace manifest (members + shared dependencies).
├── CLAUDE.md               # Contributor/agent guide for this contract.
├── LICENSE                 # Project's software license.
└── readme.md               # This file.
```

## Core Functionality: `ExecuteRoute`

The `ExecuteRoute` message is the primary entry point for executing complex trading routes (CW20-initiated routes use the `Receive` hook's `Cw20HookMsg::ExecuteRoute`). It is designed to be highly flexible, allowing users to define routes as a **Directed Acyclic Graph (DAG)** of swaps. This enables parallel, multi-hop paths that can utilize different intermediate assets, all within a single transaction.

### Execution Flow

When the contract receives an `ExecuteRoute` message, it performs the following steps:

1.  **Takes Custody:** The user sends their initial funds (either a native token or a CW20 token via a `Receive` message) along with the `AggregateSwaps` instructions. The aggregator contract takes custody of these initial funds.

2.  **Processes Stage by Stage:** The contract processes the route one `Stage` at a time. A stage represents a synchronization point where all parallel paths must complete before the next stage begins.

3.  **Executes Parallel Paths:** Within each stage, the contract divides the input funds according to the `percent` specified in each `Split`. Each `Split` defines a `Path` of one or more sequential swap `Operation`s.
    *   The contract begins executing the first operation of each `Path` in parallel.
    *   The `reply` handler receives the output of an operation and seamlessly dispatches the *next* operation in that specific `Path`, using the output of the previous step as the new input.

4.  **Accumulates and Normalizes:**
    *   **Mid-Path Conversion (Key Feature):** If the output of one hop in a path (e.g., `CW20 SHROOM`) does not match the required input for the next hop (e.g., `Native SHROOM`), the contract will automatically pause that path, perform the necessary conversion via the `cw20_adapter`, and then resume the path with the correctly-formed asset.
    *   **End-of-Stage Normalization:** When all parallel paths in a stage are complete, the contract accumulates all the final outputs. Before proceeding to the next stage, it plans and executes the minimum set of conversions required to satisfy the input requirements of all splits in the upcoming stage.

5.  **Repeats or Completes:**
    *   If there is another stage, it uses the now-normalized assets as input and repeats step 3.
    *   If it was the final stage, it proceeds to the final payout.

6.  **Final Payout and Safety Check:** After the final stage (and any final normalizations) are complete, the contract performs its most critical safety check.
    *   It verifies that the total amount of the final asset it holds is greater than or equal to the user's specified `minimum_receive`.
    *   If the check passes, it sends the full balance of the final asset to the user.
    *   If the check fails, the entire transaction is reverted, and the user gets their initial funds back.

### Message Structure

The `ExecuteRoute` message is composed of several nested structs that define the route graph.

```rust
// ExecuteMsg::ExecuteRoute { ... }
pub struct ExecuteRoute {
    /// A vector of `Stage`s, executed sequentially. Each stage is a synchronization barrier.
    pub stages: Vec<Stage>,

    /// The minimum amount of the *final* output token the user is willing to receive.
    /// If the final balance held by the contract is less than this, the transaction reverts.
    pub minimum_receive: Option<Uint128>,
}

pub struct Stage {
    /// A vector of `Split`s, whose paths are executed in parallel.
    pub splits: Vec<Split>,
}

pub struct Split {
    /// The percentage of the stage's input funds to allocate to this path.
    /// All percentages in a stage must sum to 100.
    pub percent: u8,
    
    /// A `Path` is a vector of `Operation`s, representing a sequence of multi-hop swaps.
    pub path: Vec<Operation>,
}

pub enum Operation {
    /// A swap on a constant-product (legacy XYK / AMM) pair.
    AmmSwap(AmmSwapOp),
    /// A native atomic spot-market order on Injective's orderbook (placed by the
    /// aggregator itself — no external swap contract).
    OrderbookSwap(OrderbookSwapOp),
    /// A swap on a concentrated-liquidity (CLMM) pool.
    ClmmSwap(ClmmSwapOp),
}

// These structs define the specific details for each operation type.
// AMM/CLMM ops carry only the `offer` side: the output (ask) asset is derived
// from the pool's swap event (`ask_asset` attribute) during execution and from
// the pool's `Pair {}` / `GetConfig {}` query during `SimulateRoute`.
pub struct AmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
}

// Orderbook hops are placed natively by the aggregator itself (it submits an
// atomic spot-market order from its own subaccount — there is no external swap
// contract). Native denoms only. `market_id` + `target_denom` are sufficient:
// the offer denom is the market's other side, the direction is
// `is_buy = (target_denom == market.base_denom)`, and the ticks come from the
// market — all derived on-chain.
pub struct OrderbookSwapOp {
    pub market_id: MarketId,
    /// The native denom this hop must produce (market base for a buy, quote for a sell).
    pub target_denom: String,
    /// Direct mode (arb bot): fixed base quantity. `None` => estimate from the book.
    pub quantity: Option<FPDecimal>,
    /// Direct mode (arb bot): worst acceptable price bound. `None` => estimate from the book.
    pub worst_price: Option<FPDecimal>,
}

pub struct ClmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
    /// Direct mode (arb bot): the `SwapExactInput` floor, passed straight through.
    /// `None` => estimate it from the pool (`Quote` + 0.5% slippage).
    pub minimum_amount_out: Option<Uint128>,
}
```

### Example Usage

Here is an example of a complex route that showcases the multi-hop `Path` functionality.

**Route:** Start with `INJ`. Split the funds into three parallel paths using AMM, Orderbook, and CLMM pools, all ending up with `USDT`.

```json
{
  "execute_route": {
    "stages": [
      {
        "splits": [
          {
            "percent": 33,
            "path": [
              {
                "amm_swap": {
                  "pool_address": "inj1...",
                  "offer_asset_info": { "native_token": { "denom": "inj" } }
                }
              }
            ]
          },
          {
            "percent": 34,
            "path": [
              {
                "orderbook_swap": {
                  "market_id": "0x...",
                  "target_denom": "peggy0x...usdt"
                }
              }
            ]
          },
          {
            "percent": 33,
            "path": [
              {
                "clmm_swap": {
                  "pool_address": "inj1...",
                  "offer_asset_info": { "native_token": { "denom": "inj" } }
                }
              }
            ]
          }
        ]
      }
    ],
    "minimum_receive": "123000000"
  }
}
```

## FlashRoute: Capital-Free CLMM Flash-Arb

`FlashRoute` borrows a token from a Choice CLMM pool's `Flash {}`, runs a **cycle**
(`X → … → X`) through *other* venues using the same routing engine as `ExecuteRoute`,
repays `principal + flash_fee`, and forwards the surplus to the caller — all atomic,
with no upfront capital. Because a flash loan must be repaid in the borrowed asset,
the cycle ends in `flash_asset` (it is not an A→B user swap). Design details and the
test plan live in [`docs/flash_route_plan.md`](docs/flash_route_plan.md).

### How it works

1. The caller sends `FlashRoute`. The aggregator validates the cycle (rejecting any
   hop that routes through `flash_pool` — the pool's reentrancy lock would revert the
   tx), maps `flash_asset` onto the pool's `token0`/`token1`, and fires the pool's
   `Flash {}`.
2. The pool lends the tokens to the aggregator and calls it back with `FlashCallback`.
   The whole cycle then runs **depth-first inside that callback**.
3. At the end of the cycle the aggregator repays `principal + fee` to the pool by
   direct transfer (Bank `Send` / CW20 `Transfer` — never CW20 `Send`) and sends the
   surplus to the caller. If the surplus can't cover `min_profit`, the whole
   transaction reverts and the loan is unwound.

Safety: `FlashCallback` is gated on an in-flight `FlashRoute` (`PENDING_FLASH`) and on
`info.sender == flash_pool`, so a forged callback can't spend idle contract balances;
the pool's own repayment check is a final backstop (it reverts the tx if unrepaid).

### Message structure

```rust
// ExecuteMsg::FlashRoute { ... }
pub struct FlashRoute {
    /// CLMM pool to borrow from (its Flash {} is the loan source).
    pub flash_pool: String,
    /// Asset to borrow; must be the pool's token0 or token1.
    pub flash_asset: amm::AssetInfo,
    /// Amount to borrow (the cycle's working capital).
    pub flash_amount: Uint128,
    /// The X → … → X cycle. Must end in `flash_asset` and must NOT touch `flash_pool`.
    pub stages: Vec<Stage>,
    /// Surplus floor (in `flash_asset`); the tx reverts unless surplus ≥ this.
    pub min_profit: Uint128,
}

// ExecuteMsg::FlashCallback { fee0, fee1, data } — the borrower callback. Invoked by
// the pool mid-flash; wire-matches choice_clmm_common::pool::FlashCallbackMsg. Not
// called directly by users.
```

### Example: borrow USDT, arb across two pools, keep the spread

```json
{
  "flash_route": {
    "flash_pool": "inj1...clmmpool",
    "flash_asset": { "native_token": { "denom": "peggy0x...usdt" } },
    "flash_amount": "1000000000",
    "stages": [
      { "splits": [ { "percent": 100, "path": [
        { "amm_swap": { "pool_address": "inj1...poolA",
                        "offer_asset_info": { "native_token": { "denom": "peggy0x...usdt" } } } }
      ] } ] },
      { "splits": [ { "percent": 100, "path": [
        { "amm_swap": { "pool_address": "inj1...poolB",
                        "offer_asset_info": { "native_token": { "denom": "inj" } } } }
      ] } ] }
    ],
    "min_profit": "50000000"
  }
}
```
