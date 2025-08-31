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

The `AggregateSwaps` message is the primary entry point for executing complex, multi-step trading routes. It is designed to be highly flexible, allowing users to split funds across different decentralized exchanges (DEXs), chain swaps together in sequence, and seamlessly handle conversions between native and CW20 token types.

### Execution Flow

When the contract receives an `AggregateSwaps` message, it performs the following steps:

1.  **Takes Custody:** The user sends their initial funds (either a native token or a CW20 token via a `Send` message) along with the `AggregateSwaps` instructions. The aggregator contract takes custody of these initial funds.

2.  **Processes Stage by Stage:** The contract processes the route one `Stage` at a time.
    *   For the current stage, it divides the input funds according to the `percent` specified in each `Split`.
    *   It then executes the `Operation` (e.g., an AMM swap) for each split in parallel as submessages.

3.  **Accumulates and Normalizes:** The contract's `reply` handler waits for all splits in a stage to complete.
    *   It accumulates the outputs from all the swaps.
    *   **Asset Normalization (Key Feature):** Before proceeding, it checks if the accumulated assets match the required input for the *next* stage. If there is a mismatch (e.g., it holds a mix of native and CW20 tokens, but the next stage requires only the CW20 version), it will automatically call the `cw20_adapter` contract to convert the assets into the required type.

4.  **Repeats or Completes:**
    *   If there is another stage, it uses the now-unified assets as input and repeats step 2.
    *   If it was the final stage, it proceeds to the final payout.

5.  **Final Payout and Safety Check:** After the final stage (and any final normalizations) are complete, the contract performs its most critical safety check.
    *   It verifies that the total amount of the final asset it holds in custody is greater than or equal to the user's specified `minimum_receive`.
    *   If the check passes, it sends the full balance of the final asset to the user.
    *   If the check fails, the entire transaction is reverted, and the user gets their initial funds back.

### Message Structure

The `AggregateSwaps` message is composed of several nested structs that define the route.

```rust
pub struct ExecuteMsg::AggregateSwaps {
    /// A vector of `Stage`s, executed sequentially.
    pub stages: Vec<Stage>,

    /// The minimum amount of the *final* output token the user is willing to receive.
    /// Acts as a final, global safety check for the entire route. If the final balance
    /// held by the contract is less than this, the transaction reverts.
    pub minimum_receive: Option<String>,
}

pub struct Stage {
    /// A vector of `Split`s, executed in parallel within the stage.
    pub splits: Vec<Split>,
}

pub struct Split {
    /// The percentage of the stage's input funds to allocate to this operation.
    /// All percentages in a stage must sum to 100.
    pub percent: u8,
    /// The specific swap operation to perform.
    pub operation: Operation,
}

pub enum Operation {
    /// A swap on a constant-product (AMM) DEX.
    AmmSwap(AmmSwapOp),
    /// A swap on an orderbook-style DEX.
    OrderbookSwap(OrderbookSwapOp),
}
```

### Example Usage

Here is an example of a complex, three-stage route that swaps USDT for a mix of native and CW20 SHROOM, which are then automatically normalized and sent to the user.

**Route:**
1.  **Stage 1:** Swap 1,000 USDT for INJ.
2.  **Stage 2:** Split the resulting INJ, sending 10% to an AMM to get CW20 SHROOM and 90% to an Orderbook to get Native SHROOM.
3.  **Final Payout:** The contract automatically converts the Native SHROOM to CW20 SHROOM and sends the total unified SHROOM balance to the user.

```rust
let msg = ExecuteMsg::AggregateSwaps {
    stages: vec![
        // Stage 1: 100% of USDT to the Orderbook to get INJ.
        Stage {
            splits: vec![Split {
                percent: 100,
                operation: Operation::OrderbookSwap(/* ... */),
            }],
        },
        // Stage 2: The resulting INJ is split 10/90 to get a mix of SHROOM types.
        Stage {
            splits: vec![
                Split {
                    percent: 10, // 10% to CW20 SHROOM
                    operation: Operation::AmmSwap(/* ... */),
                },
                Split {
                    percent: 90, // 90% to Native SHROOM
                    operation: Operation::OrderbookSwap(/* ... */),
                },
            ],
        },
    ],
    // The final expected output is unified CW20 SHROOM.
    minimum_receive: Some("9900000000".to_string()), // Min 9,900 CW20 SHROOM
};

// This message would be sent to the aggregator contract with 1,000 USDT.
wasm.execute(&aggregator_addr, &msg, &[Coin::new(1_000_000_000, "usdt")], &user);
```