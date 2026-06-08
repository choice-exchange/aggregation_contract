# dex_aggregator — orderbook-merge review findings

Review of the orderbook-merge revision (correctness + gas/interface). Scope: the
full `dex_aggregator` contract, cross-checked against the original
`inj-orderbook-swap-contract` estimators and the integration suite.

Status legend: ❌ open · 🔧 in progress · ✅ fixed

---

## Correctness

### C1 — HIGH ✅ zero-quote CLMM hop reverts the whole route
`execute.rs` (`create_swap_cosmos_msg`, CLMM estimation branch).

`to_json_binary(&{})` serializes `()` to JSON `null`, not `{}`. The resulting
self-`Execute` submessage fails `from_json::<ExecuteMsg>` at the entry point, and
because every swap submessage is `reply_on_success`, the failure propagates and
reverts the whole transaction — contradicting the "completes gracefully as a
zero-value path" comment. Only reachable in CLMM estimation mode (dApp path) on an
illiquid hop; the bot uses direct mode.

**Fix:** don't emit a submessage for a zero-quote hop. Treat it as a zero-fill the
same way zero-amount splits are already filtered out, so the split ends gracefully.

### C2 — HIGH ✅ multi-level orderbook BUYS revert ("Swap amount too high")
`orderbook_exec.rs` (`estimate_execution_buy_from_source`).

Order quantity is sized from the **average** fill price, but the funds check and
the chain's atomic-order margin reservation use the **worst** price:

```
available      = input / (1+fee)
expected_base  = available / average_price
required_funds = worst_price * expected_base * (1+fee) = (worst/avg) * input
```

`required_funds = (worst/avg)·input` exceeds the held `input` for any fill that
crosses more than one price level (worst > avg) → the execution-mode check errors,
and even relaxed, the placed `BuyAtomic` reserves margin at `worst*qty*(1+fee) >
input` and the chain rejects it. Inherited from the original standalone contract,
but the aggregator drives larger multi-hop routes where multi-level buys are
routine. Tests only fill buys at a single level so the gap is invisible.

**Fix:** size the order quantity from `worst_price` so `margin ≤ input` is
guaranteed (`expected_base = available / worst_price`); the small under-buy
leftover stays as recoverable dust. Add a multi-level-buy integration test
(seed a fill that crosses 10→11) to lock it in.

### C3 — LOW ✅ panics on degenerate books
`orderbook_exec.rs`: `assert_ne!` on zero-value level, `total_quantity == 0`
assert, `levels.last().unwrap()`. Prefer returning `StdError` over panicking in a
user-facing query/execute. Shouldn't happen with a well-formed book.

---

## Gas / interface

### G1 — HIGH ✅ same spot market queried ~6× per orderbook hop
`reply.rs` (`plan_next_stage` calls `get_operation_input` → `load_market` 3×,
`execute_planned_swaps` 1×), `execute.rs` (`create_swap_cosmos_msg` 1×),
`orderbook_exec.rs` (`estimate_single_swap_execution` re-queries by id 1× despite
`build_swap_order_msg` already holding `&SpotMarket`).

**Fix:**
1. `plan_next_stage` — compute each split's `offer_info` once and reuse across the
   three loops (single pass).
2. Thread the already-loaded `&SpotMarket` into `estimate_single_swap_execution`
   instead of re-querying.

### G2 — LOW ❌ `SimulateRoute` does a second query per AMM/CLMM hop
Deriving the ask asset via `Pair {}` / `GetConfig {}` (read-only query path).
Reasonable trade for a slimmer on-chain message; documented here only. An optional
`ask_asset_info` hint on the op would let callers skip it if sim latency matters.

### G3 — NIT ✅ `format!("{:?}", asset_info)` in an event attribute
`emergency_withdraw` emits Rust `Debug` into an on-chain attribute. Prefer the
denom/contract string.

### G4 — interface notes (no change)
- Direct-mode `OrderbookSwapOp` with only one of `quantity`/`worst_price` set
  silently falls back to estimation; direct mode does no funds pre-check.
- `FlashCallback.data` intentionally unused (both ends read `PENDING_FLASH`).

---

## Fix plan (this pass: critical findings)

- [x] C1 — zero-quote CLMM hop → graceful zero-fill
      (`create_swap_cosmos_msg` returns `Option`; all call sites route `None`
      through the shared `complete_zero_value_path` helper)
- [x] C2 — buy sizing from `worst_price` + multi-level-buy integration test
      (`test_orderbook_buy_crosses_multiple_price_levels`; reverted pre-fix)
- [x] G1 — dedupe market queries on the orderbook hot path
      (`plan_next_stage` resolves offer info once; `estimate_single_swap_execution`
      takes the already-loaded `&SpotMarket`)

### Validation
`./build_release.sh` (Docker optimizer) + `cargo test -p dex_aggregator`:
15/15 lib tests, **37/37 integration tests** pass, clippy clean. Single-level
orderbook outputs unchanged (no regression); the new multi-level buy test confirms
the C2 revert is gone.

## Follow-ups (second pass)

- [x] C3 — `orderbook_exec` book-walk panics replaced with `StdError`
      (`get_minimum_liquidity_levels`, `get_average_price_from_orders`,
      `get_worst_price_from_orders` now return `Result`).
- [x] G3 — `emergency_withdraw` emits the denom/contract string, not `Debug`.
- [ ] G2 — DEFERRED. Re-adding an optional `ask_asset_info` hint would partly undo
      the deliberate message-slimming, and the extra query is read-only
      (`SimulateRoute`, off-chain, no user gas). Not worth the interface surface.
