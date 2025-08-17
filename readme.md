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

```
src/
├── contract.rs     # Core entry points (instantiate, execute, query, reply)
├── error.rs        # Custom contract error types
├── execute.rs      # Handlers for execute messages
├── lib.rs          # Crate root module declarations
├── msg.rs          # Message definitions (Instantiate, Execute, Query) and Route structures
├── query.rs        # Handlers for query messages
├── reply.rs        # Logic for handling submessage replies
├── state.rs        # State definitions and storage management
└── tests.rs        # Integration tests using injective-test-tube
```