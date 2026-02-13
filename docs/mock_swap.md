# mock_swap Contract — Deep Reference

## Purpose

The `mock_swap` contract is a test-only helper that simulates both AMM and orderbook DEX pools with a configurable exchange rate. It implements the same execute and query interfaces that the `dex_aggregator` contract calls, so integration tests can deploy mock pools with known rates and verify the aggregator's routing logic end-to-end.

**This contract is never deployed to mainnet/testnet.** It exists solely for `injective-test-tube` integration tests.

## Source

Single file: `contracts/mock_swap/src/lib.rs` (~273 lines)

Compiled artifact: `artifacts/mock_swap.wasm` (produced by `./build_release.sh`)

---

## Configuration

Each mock_swap instance is configured at instantiation with a single trading pair and rate:

```rust
pub struct SwapConfig {
    pub input_asset_info: AssetInfo,   // What this pool accepts
    pub output_asset_info: AssetInfo,  // What this pool returns
    pub rate: String,                  // Exchange rate as decimal string (e.g. "2.0")
    pub protocol_type: ProtocolType,   // Amm or Orderbook
    pub input_decimals: u8,            // Decimal places of input token
    pub output_decimals: u8,           // Decimal places of output token
}

pub enum ProtocolType {
    Amm,
    Orderbook,
}

pub struct InstantiateMsg {
    pub config: SwapConfig,
}
```

The config is stored in a single `Item<SwapConfig>` keyed as `"config"`.

### Rate Calculation

The output amount is calculated as:

```
offer_decimal = offer_amount / 10^input_decimals
return_decimal = offer_decimal * rate
final_return_amount = return_decimal.atomics() / 10^(18 - output_decimals)
```

Where 18 is the internal `DECIMAL_PRECISION` used by cosmwasm `Decimal`. If the input asset doesn't match `config.input_asset_info`, the output is `Uint128::zero()` and the swap is silently skipped.

---

## Execute Interface

The contract accepts three execute message variants:

### `Swap` (AMM protocol)

```rust
Swap {
    offer_asset: Asset,
    belief_price: Option<Decimal>,   // ignored
    max_spread: Option<Decimal>,     // ignored
    to: Option<String>,              // recipient override
    deadline: Option<u64>,           // ignored
}
```

This matches the interface the aggregator uses for `amm::AmmPairExecuteMsg::Swap`. The mock:
1. Extracts `offer_asset.amount` and `offer_asset.info`
2. If `to` is provided, sends output there; otherwise sends to `info.sender`
3. Calculates output using the configured rate
4. Emits a `wasm` event with `return_amount` attribute (parsed by the aggregator's reply handler)

### `SwapMinOutput` (Orderbook protocol)

```rust
SwapMinOutput {
    target_denom: String,
    min_output_quantity: String,   // ignored by mock
}
```

This matches `orderbook::OrderbookExecuteMsg::SwapMinOutput`. The mock:
1. Takes the input from `info.funds[0]`
2. Calculates output using the configured rate
3. Emits an `atomic_swap_execution` event with `swap_final_amount` attribute (parsed by the aggregator's reply handler)

### `Receive` (CW20 hook)

```rust
Receive(Cw20ReceiveMsg { sender, amount, msg })
```

Handles CW20 token inputs. The inner `msg` is decoded as `MockSwapHookMsg`:

```rust
pub struct MockSwapHookMsg {
    pub swap: MockSwapHookSwapField,
}

pub struct MockSwapHookSwapField {
    pub offer_asset: Option<Asset>,
    pub belief_price: Option<Decimal>,
    pub max_spread: Option<Decimal>,
    pub to: Option<String>,
    pub deadline: Option<u64>,
}
```

If decoding succeeds and `to` is set, output goes to that address. Otherwise output goes to `sender`. The input asset is identified as `AssetInfo::Token { contract_addr: info.sender }` (the CW20 contract that called Receive).

---

## Output Messages

After calculating the return amount, the mock sends the output via:

- **CW20 output:** `WasmMsg::Execute` → `Cw20ExecuteMsg::Transfer { recipient, amount }`
- **Native output:** `BankMsg::Send { to_address, amount: [Coin] }`

If `final_return_amount` is zero, no message is sent and the response contains only an attribute `action: swap_skipped_or_zero_amount`.

---

## Events Emitted

The event type depends on `config.protocol_type`:

### AMM (`ProtocolType::Amm`)

```
Event("wasm")
  action = "swap"
  return_amount = "<output_amount>"
```

The aggregator's `parse_amount_from_swap_reply` looks for `return_amount` in wasm events.

### Orderbook (`ProtocolType::Orderbook`)

```
Event("atomic_swap_execution")
  sender = "<info.sender>"
  swap_input_amount = "<offer_amount>"
  swap_input_denom = "<input_denom>"
  refund_amount = "0"
  swap_final_amount = "<output_amount>"
  swap_final_denom = "<output_denom>"
```

The aggregator looks for `swap_final_amount` in `wasm-atomic_swap_execution` events.

---

## Query Interface

### `GetOutputQuantity` (Orderbook simulation)

```rust
GetOutputQuantity {
    from_quantity: FPDecimal,
    source_denom: String,
    target_denom: String,
}
```

Returns `SwapEstimationResult`:

```rust
pub struct SwapEstimationResult {
    pub result_quantity: FPDecimal,
    pub expected_fees: Vec<FPCoin>,
}
```

**Validation:** The `source_denom` and `target_denom` must match the configured input/output asset denoms. If not, returns a `StdError::generic_err` explaining the mismatch.

**Calculation:** `result_quantity = from_quantity * rate` using `FPDecimal` arithmetic.

**Fees:** Always returns a single `FPCoin` with `amount: FPDecimal::ZERO` and `denom` set to the target denom.

The aggregator calls this query in `create_swap_cosmos_msg` (for orderbook ops) to determine `min_output_quantity` with slippage.

Note: The mock does **not** implement `amm::QueryMsg::Simulation`. The aggregator's simulation queries for AMM pools go to the real pool contracts in production; in integration tests, AMM simulation queries are not used since `SimulateRoute` is tested via unit tests with `MockQuerier` in `query.rs`.

---

## Asset Types

The mock defines its own copies of the asset types (not imported from `dex_aggregator`):

```rust
pub enum AssetInfo {
    Token { contract_addr: String },
    NativeToken { denom: String },
}

pub struct Asset {
    pub info: AssetInfo,
    pub amount: Uint128,
}
```

These serialize identically to `dex_aggregator::msg::amm::AssetInfo` and `amm::Asset`.

---

## Usage in Integration Tests

In `tests/integration.rs`, mock_swap contracts are deployed with specific configurations to create test pools:

```rust
// Example: deploy a mock AMM pool that converts INJ → USDT at rate 20.0
let config = SwapConfig {
    input_asset_info: AssetInfo::NativeToken { denom: "inj".into() },
    output_asset_info: AssetInfo::NativeToken { denom: "usdt".into() },
    rate: "20.0".into(),
    protocol_type: ProtocolType::Amm,
    input_decimals: 18,
    output_decimals: 6,
};
```

The test `setup()` function typically:
1. Stores the `mock_swap.wasm` code
2. Instantiates multiple instances with different configs (different pairs, rates, protocol types)
3. Funds each mock contract with output tokens so it can fulfill swaps
4. Returns pool addresses in `TestEnv` for use in route construction

### Important: Funding Mock Pools

Mock contracts must be pre-funded with their output asset. For native outputs, tests use `bank_send` to fund the contract. For CW20 outputs, tests mint/transfer tokens to the mock contract address. Without funding, swap messages will fail with insufficient balance errors.

### Key Testing Patterns

- **Rate verification:** Deploy pool with rate X, swap amount Y, verify output is Y * X (adjusted for decimals)
- **Multi-hop chains:** Deploy pool A→B and pool B→C, build a 2-operation path, verify end-to-end output
- **Protocol mixing:** Deploy AMM and orderbook mocks in the same route to test mixed protocol handling
- **CW20 swaps:** Deploy a mock that accepts CW20 input and outputs native (or vice versa), test with `Cw20ExecuteMsg::Send`

---

## Dependencies

```toml
[dependencies]
cosmwasm-std     = "2.2.2"
cosmwasm-schema  = "2.2.2"
cw-storage-plus  = "2.0.0"
cw20             = "2.0.0"
injective-cosmwasm = "0.3.4-1"
injective-math     = "0.3.4-1"
schemars         = "0.8.22"
serde            = "1.0.219"
```

Note: `injective-cosmwasm` is needed because the query entry point uses `Deps<InjectiveQueryWrapper>` for compatibility with the Injective test-tube environment.
