# Adding a New Pool Type — Information Checklist

This document explains exactly what information is needed before a new pool type can be
integrated into the `dex_aggregator` contract. If you are tasked with adding a new venue,
**gather all of the answers below first** before writing any code.

> Current as of **v2.2.0**. AMM, native orderbook and CLMM are all implemented; the reference
> tables show what each of them answered, so use them as worked examples.
>
> ⚠️ **Two answers are non-negotiable, and both are fund-safety requirements.** Read
> [v2_2_0_changes.md](v2_2_0_changes.md) before designing the op struct:
>
> 1. **§5 — the op must expose the asset it produces *before* it executes.** Either as an
>    explicit `ask_asset_info` field, or via a query the aggregator can make up-front. Deriving
>    it from the swap event afterwards is what let an arbitrary contract name any asset it liked.
> 2. **§9 — every amount that leaves the contract must go through `route_spendable`.** If your
>    integration adds a new disbursement path, it must be clamped like the existing ones.

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
| AMM | `AmmPairExecuteMsg::Swap` | `offer_asset`, `belief_price` (optional), `max_spread` (**never `None` on the wire** — see `AmmSwapOp`), `to` (optional) |
| Orderbook | *(none — not a contract)* | The aggregator builds `MsgCreateSpotMarketOrder` itself in `orderbook_exec.rs`, from its own default subaccount |
| CLMM | `ClmmPoolExecuteMsg::SwapExactInput` | `minimum_amount_out`, `recipient` (optional), `deadline` (optional) |

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
| Orderbook | n/a — the order debits the contract's own bank balance | No — errors on CW20 |
| CLMM | Yes — coin in `funds` | Yes — `Cw20ExecuteMsg::Send` with a `SwapExactInput` hook |

A registered tax token (`TAX_TOKEN_REGISTRY`) uses `reflection::ExecuteMsg::TaxExemptSend`
instead of `Cw20ExecuteMsg::Send`. Handle that in your new arm too.

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
| Orderbook | *(no query — the book is walked via `InjectiveQuerier`)* | `StepExecutionEstimate` | `result_quantity: FPDecimal` |
| CLMM | `Quote { token_in: AssetInfo, amount_in: Uint128 }` | `QuoteResponse` | `amount_out: Uint128` |

⚠️ **The estimator and the executor must share code.** `SimulateRoute` is the arb bot's
pre-broadcast gate; anything that changes what the executor submits and is not visible to the
query path makes the gate clear routes that are guaranteed to revert. The orderbook path shares
`estimate_single_swap_execution`, `direct_order_params`, `sell_base_quantity` and
`meets_min_notional` for exactly this reason. Do the same.

---

## 4. Reply Event Format

After a swap submessage succeeds, the aggregator parses the **output amount** from the reply's
events (`parse_amount_from_swap_reply`). It does **not** parse the output *asset* — see §5.

**Questions to answer:**

- What **event type** does the pool emit? (e.g. `"wasm"`, `"wasm-swap"`, a custom event name?)
- What **attribute key** contains the output amount? (e.g. `"return_amount"`, `"amount_out"`)
- Is the amount an integer string, or can it contain decimals? (Decimal amounts are truncated.)
- Are there any other attributes needed for disambiguation (e.g. `"action"`, `"sender"`)?

**For reference:**

| Pool type | Event type | Amount attribute | Format |
|-----------|-----------|-----------------|--------|
| AMM | `wasm` | `return_amount` | Integer string |
| CLMM | `wasm` | `amount_out` | Integer string |
| Orderbook | *(none)* | decoded from the typed `MsgCreateSpotMarketOrderResponse` | `FPDecimal`, descaled by 1e18 |

⚠️ **An amount read from a pool's event is untrusted input**, because `pool_address` is
caller-supplied and there is no allowlist. It is safe only because every amount the route
subsequently moves is clamped by `route_spendable` to what the route actually brought in. If your
integration can get a *chain-signed* figure instead (as the orderbook path does), prefer that.

---

## 5. Operation-Specific Parameters

Each pool type has its own struct carrying pool-specific config per operation.

**Questions to answer:**

- Besides `pool_address`/`contract_address` and `offer_asset_info` (which are standard), what **additional fields** does this pool type need per-operation?
- **How is the output (ask) asset determined before the hop runs?** This is mandatory, not a
  nicety: `snapshot_entry_balances` must record that denom's pre-route balance, because that
  baseline is what bounds everything the route later spends and pays out. Two acceptable
  answers: an explicit `ask_asset_info: Option<AssetInfo>` on the op (free), and/or a pool query
  the aggregator can make up-front (`Pair {}` for AMM, `GetConfig {}` for CLMM). "Read it back
  from the swap event" is **not** acceptable — that made the venue the authority on what it had
  produced, and a caller-authored contract could name any asset it liked.
- Resolution must **fail closed**: if the output can't be determined, the route errors
  (`UnresolvableAskAsset`). Never fall through to "unbounded".
- For the orderbook, the extra field set is `quantity`/`worst_price` (direct mode). For CLMM it
  is `minimum_amount_out`/`slippage_bps`.
- Which fields would be provided by the route planner (off-chain) vs. derived on-chain? Prefer
  off-chain where the planner already knows the value — every avoided query is gas the
  latency-sensitive arb instance keeps.

**Proposed struct template:**

```rust
pub struct MyPoolSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
    /// REQUIRED pattern. `None` => resolved from a pool query in
    /// `get_operation_output`; supplying it skips that query.
    #[serde(default)]
    pub ask_asset_info: Option<amm::AssetInfo>,
    // What else goes here? List every field with its type.
}
```

Keep every new field `Option<T>` with `#[serde(default)]` so existing route JSON keeps
deserializing.

---

## 6. Pre-Execution Queries or Rounding

The orderbook integration performs extra work before executing: it rounds the input amount to `min_quantity_tick_size` and queries the simulation to compute a minimum output with 0.5% slippage.

**Questions to answer:**

- Does the new pool type require **rounding** the input amount to any tick size or step? If so,
  the rounding must live in code **shared with the estimator**, or `SimulateRoute` will
  over-report by exactly the rounding loss.
- Does the new pool type require a **pre-execution simulation query** to compute a minimum
  output, price limit, or other parameter?
- If yes, what slippage tolerance should be applied? Make it a caller-settable
  `Option<u16>` bps field with a documented default, not a hardcoded constant.
- What should happen if the amount rounds to zero, or the venue can't fill at all?
  **Return `Ok(None)` from `create_swap_cosmos_msg`** — a graceful zero-value path. The split
  contributes nothing, its allocation is swept back, and the rest of the route proceeds. Do not
  return an error: that reverts the whole route over one dust split, and diverges from the
  no-fill the gate quoted.

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
| 5 | Op struct fields | All per-operation fields, **including how the output asset is resolved before execution** |
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
| `contracts/dex_aggregator/src/reply.rs` | Add arms in `get_operation_input`, **`get_operation_output`** (the up-front asset resolution — see §5), `operation_kind` and `get_operation_address`. Update `parse_amount_from_swap_reply` if the event format differs from the existing patterns. |
| `contracts/dex_aggregator/src/query.rs` | Add arms in `simulate_single_operation` and `get_path_start_info`, sharing the executor's sizing code |
| `contracts/dex_aggregator/src/execute.rs` | Add the venue's address to the `FlashRoute` reentrancy check if it carries one |
| `contracts/mock_swap/src/lib.rs` | Add `ProtocolType::Clmm` variant, add matching event emission in `execute` (~line 201), add simulation query handling in `query` if the query format differs |
| `tests/integration.rs` | Add integration tests deploying mock CLMM pools and routing through them |
| `contracts/dex_aggregator/schema/` | Regenerate schemas after msg.rs changes (`cd contracts/dex_aggregator && cargo run --example schema`) |
