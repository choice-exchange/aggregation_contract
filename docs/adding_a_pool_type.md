# Adding a New Pool Type (e.g. CLMM) — Information Checklist

This document explains exactly what information is needed before a new pool type can be integrated into the `dex_aggregator` contract. If you are an agent tasked with adding CLMM (or any new pool type) support, **gather all of the answers below first** before writing any code.

---

## 1. Swap Execute Message

The aggregator calls each pool via `WasmMsg::Execute`. You must provide the **exact Rust struct/enum** that the target pool contract expects.

**Questions to answer:**

- What is the full execute message type for performing a swap? (Provide the Rust enum variant with all fields.)
- Are there required fields beyond the basics (e.g. `sqrt_price_limit`, `tick_range`, `deadline`)?
- Are any fields optional? What are sensible defaults?

**For reference, here's what we have today:**

| Pool type | Execute message | Key fields |
|-----------|----------------|------------|
| AMM | `AmmPairExecuteMsg::Swap` | `offer_asset`, `belief_price` (optional), `max_spread` (optional), `to` (optional) |
| Orderbook | `OrderbookExecuteMsg::SwapMinOutput` | `target_denom`, `min_output_quantity` |

---

## 2. Token Input Handling

The aggregator must know how to attach tokens to the swap message.

**Questions to answer:**

- Does the pool accept **native tokens** (sent as `funds` on `WasmMsg::Execute`)?
- Does the pool accept **CW20 tokens** (sent via `Cw20ExecuteMsg::Send` with the swap msg as inner payload)?
- Does it accept **both**? Or only one?
- If CW20: what is the expected inner hook message format when the pool receives tokens via `Cw20::Send`?

**For reference:**

| Pool type | Native input | CW20 input |
|-----------|-------------|------------|
| AMM | Yes — coin in `funds` | Yes — `Cw20ExecuteMsg::Send { contract: pool, amount, msg: <swap_msg> }` |
| Orderbook | Yes — coin in `funds` | No — errors on CW20 |

---

## 3. Simulation / Quote Query

The aggregator queries each pool to estimate output (used for both `SimulateRoute` queries and, in the orderbook case, to compute `min_output_quantity` before execution).

**Questions to answer:**

- What is the **query message** to get an output estimate for a given input amount?
- What is the **response type** (exact struct with field names and types)?
- Which field in the response contains the expected output amount?
- Does the query use `Uint128`, `FPDecimal`, or another numeric type for amounts?

**For reference:**

| Pool type | Query message | Response | Output field |
|-----------|--------------|----------|-------------|
| AMM | `Simulation { offer_asset: Asset }` | `SimulationResponse` | `return_amount: Uint128` |
| Orderbook | `GetOutputQuantity { from_quantity: FPDecimal, source_denom, target_denom }` | `SwapEstimationResult` | `result_quantity: FPDecimal` |

---

## 4. Reply Event Format

After a swap submessage succeeds, the aggregator parses the **output amount** from the reply's events. This is the most critical piece — if the event format is wrong, the aggregator won't know how much it received.

**Questions to answer:**

- What **event type** does the pool emit? (e.g. `"wasm"`, `"wasm-swap"`, a custom event name?)
- What **attribute key** contains the output amount? (e.g. `"return_amount"`, `"swap_final_amount"`)
- Is the amount an integer string, or can it contain decimals? (The aggregator currently truncates decimal amounts.)
- Are there any other attributes needed for disambiguation (e.g. `"action"`, `"sender"`)?

**For reference:**

| Pool type | Event type | Amount attribute | Format |
|-----------|-----------|-----------------|--------|
| AMM | `wasm` | `return_amount` | Integer string |
| Orderbook | `wasm-atomic_swap_execution` | `swap_final_amount` | May contain decimals (truncated) |

---

## 5. Operation-Specific Parameters

Each pool type has its own struct carrying pool-specific config per operation.

**Questions to answer:**

- Besides `pool_address`/`contract_address`, `offer_asset_info`, and `ask_asset_info` (which are standard), what **additional fields** does this pool type need per-operation?
- For the orderbook, this is `min_quantity_tick_size`. For CLMM, it might be `sqrt_price_limit`, `tick_spacing`, `fee_tier`, etc.
- Which fields would be provided by the route planner (off-chain) vs. derived on-chain?

**Proposed struct template:**

```rust
pub struct ClmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
    pub ask_asset_info: amm::AssetInfo,
    // What else goes here? List every field with its type.
}
```

---

## 6. Pre-Execution Queries or Rounding

The orderbook integration performs extra work before executing: it rounds the input amount to `min_quantity_tick_size` and queries the simulation to compute a minimum output with 0.5% slippage.

**Questions to answer:**

- Does the new pool type require **rounding** the input amount to any tick size or step?
- Does the new pool type require a **pre-execution simulation query** to compute a minimum output, price limit, or other parameter?
- If yes, what slippage tolerance should be applied?
- What should happen if the rounded amount is zero? (Orderbook currently emits a no-op message.)

---

## 7. Output Delivery

**Questions to answer:**

- After a swap, does the pool **automatically send** the output tokens to the `to`/recipient address?
- Or does the caller need to **claim/withdraw** the output in a separate step?
- If the pool sends output automatically, does it use `BankMsg::Send` (native) or `Cw20ExecuteMsg::Transfer` (CW20)?

The aggregator assumes output is automatically delivered to `env.contract.address` (itself) after each swap. If the new pool type requires a separate claim step, that would need additional handling.

---

## 8. Contract Address / Pool Identifier

**Questions to answer:**

- Is the pool identified by a single **contract address** (like AMM and orderbook)?
- Or does it use a different identifier (pool ID, factory + pair key, etc.)?
- If a contract address, is it the same address for both execution and simulation queries?

---

## Summary: What to Provide

Before any code changes, provide a document or message containing:

| # | Item | What to provide |
|---|------|----------------|
| 1 | Execute message | Full Rust enum/struct definition |
| 2 | Token input | Native, CW20, or both — plus CW20 hook msg format if applicable |
| 3 | Simulation query | Query msg struct, response struct, which field = output amount |
| 4 | Reply events | Event type name, attribute key for output amount, attribute value format |
| 5 | Op struct fields | All per-operation fields beyond the standard three |
| 6 | Pre-execution logic | Any rounding, simulation queries, or slippage computation needed |
| 7 | Output delivery | Auto-sent to recipient, or requires claim? |
| 8 | Pool identifier | Contract address or other identifier |

---

## Files That Will Be Modified

Once the above information is gathered, here are the exact files and locations that need changes:

| File | What changes |
|------|-------------|
| `contracts/dex_aggregator/src/msg.rs` | Add `ClmmSwapOp` struct, add `Clmm(ClmmSwapOp)` variant to `Operation` enum, add `clmm` submodule with external message types |
| `contracts/dex_aggregator/src/execute.rs` | Add `Operation::Clmm` arm in `create_swap_cosmos_msg` (~line 93) |
| `contracts/dex_aggregator/src/reply.rs` | Add arm in `get_operation_output` (~line 508), `get_operation_input` (~line 754), `get_operation_address` (~line 810). Update `parse_amount_from_swap_reply` (~line 513) if the event format differs from existing patterns. |
| `contracts/dex_aggregator/src/query.rs` | Add `Operation::Clmm` arm in `simulate_single_operation` (~line 116) and `get_path_start_info` (~line 181) |
| `contracts/mock_swap/src/lib.rs` | Add `ProtocolType::Clmm` variant, add matching event emission in `execute` (~line 201), add simulation query handling in `query` if the query format differs |
| `tests/integration.rs` | Add integration tests deploying mock CLMM pools and routing through them |
| `contracts/dex_aggregator/schema/` | Regenerate schemas after msg.rs changes (`cd contracts/dex_aggregator && cargo run --example schema`) |
