# Choice Aggregation Contract

## Getting Started

### Prerequisites

-   [Rust](https://www.rust-lang.org/tools/install) (latest stable version recommended)
-   `rustup` target for Wasm: `rustup target add wasm32-unknown-unknown`
-   [Docker](https://www.docker.com/get-started/) for reproducible production builds.

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
# 1. Build the Wasm binary first
./build-release.sh

# 2. Run the tests
cargo test
```

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
│   │                       # both AMM and Orderbook behavior with predictable rates.
│   │
│   ├── cw20_adapter/       # A utility contract to handle conversions between native
│   │                       # Injective tokenfactory denoms and their CW20 equivalents.
│   │
│   └── cw20_base/          # The standard CW20 fungible token contract (e.g., for SHROOM, SAI).
│
├── tests/                  # Workspace-level integration tests. This is where all the
│   │                       # test files (like the ones we've been writing) reside. They use
│   │                       # `injective-test-tube` to spin up a local chain environment.
│
├── .gitignore              # Specifies intentionally untracked files to ignore.
├── build_release.sh        # A script to build optimized, production-ready .wasm files.
├── Cargo.lock              # Records the exact versions of all dependencies.
├── Cargo.toml              # The workspace's main manifest file, defining members and dependencies.
├── deploy_testnet.sh       # A utility script for deploying contracts to a testnet.
├── LICENSE                 # Project's software license.
├── readme.md               # This file.
└── test_routes.txt         # A utility file for defining or documenting test routes.
```

## Core Functionality: `AggregateSwaps`

The `AggregateSwaps` message is the primary entry point for executing complex trading routes. It is designed to be highly flexible, allowing users to define routes as a **Directed Acyclic Graph (DAG)** of swaps. This enables parallel, multi-hop paths that can utilize different intermediate assets, all within a single transaction.

### Execution Flow

When the contract receives an `AggregateSwaps` message, it performs the following steps:

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

The `AggregateSwaps` message is composed of several nested structs that define the route graph.

```rust
pub struct ExecuteMsg::AggregateSwaps {
    /// A vector of `Stage`s, executed sequentially. Each stage is a synchronization barrier.
    pub stages: Vec<Stage>,

    /// The minimum amount of the *final* output token the user is willing to receive.
    /// If the final balance held by the contract is less than this, the transaction reverts.
    pub minimum_receive: Option<String>,
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
    /// A swap on a constant-product (AMM) DEX.
    AmmSwap(AmmSwapOp),
    /// A swap on an orderbook-style DEX.
    OrderbookSwap(OrderbookSwapOp),
}

// These structs define the specific details for each operation type.
pub struct AmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: external::AssetInfo,
    pub ask_asset_info: external::AssetInfo,
}

pub struct OrderbookSwapOp {
    pub swap_contract: String,
    pub offer_asset_info: external::AssetInfo,
    pub ask_asset_info: external::AssetInfo,
}
```

### Example Usage

Here is an example of a complex route that showcases the multi-hop `Path` functionality.

**Route:** Start with `INJ`. Split the funds 50/50 into two parallel, multi-hop paths that use different intermediate assets (`USDT` and `AUSD`) but both end up with `SHROOM`.

```json
{
  "aggregate_swaps": {
    "stages": [
      {
        "splits": [
          {
            "percent": 50,
            "path": [
              {
                "amm_swap": {
                  "pool_address": "inj1...",
                  "offer_asset_info": { "native_token": { "denom": "inj" } },
                  "ask_asset_info": { "native_token": { "denom": "peggy0x...usdt" } }
                }
              },
              {
                "orderbook_swap": {
                  "swap_contract": "inj1...",
                  "offer_asset_info": { "native_token": { "denom": "peggy0x...usdt" } },
                  "ask_asset_info": { "token": { "contract_addr": "inj1...shroom" } }
                }
              }
            ]
          },
          {
            "percent": 50,
            "path": [
              {
                "amm_swap": {
                  "pool_address": "inj1...",
                  "offer_asset_info": { "native_token": { "denom": "inj" } },
                  "ask_asset_info": { "native_token": { "denom": "peggy0x...ausd" } }
                }
              },
              {
                "amm_swap": {
                  "pool_address": "inj1...",
                  "offer_asset_info": { "native_token": { "denom": "peggy0x...ausd" } },
                  "ask_asset_info": { "token": { "contract_addr": "inj1...shroom" } }
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