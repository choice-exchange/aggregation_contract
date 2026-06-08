# FlashRoute — capital-free CLMM flash-arb for the dex_aggregator

## Goal

Let the aggregator borrow a token from a Choice CLMM pool via the pool's `Flash {}`
entry point, run a **cycle** (`X → … → X`) across *other* venues using the existing
multi-stage router, repay `principal + flash_fee`, and forward the surplus to the
initiator. This is an atomic, capital-free arbitrage primitive — *not* a user A→B
swap (a flash loan must be repaid in the same asset it was borrowed in).

## How it fits the CosmWasm execution model

The CLMM pool (`choice_clmm_pool/src/actions/flash.rs`):

1. `Flash { recipient, amount0, amount1, data }` lends the tokens (Bank `Send` /
   CW20 `Transfer`) to `recipient`, then calls `recipient` with
   `FlashCallbackMsg::FlashCallback { fee0, fee1, data }` as a `reply_on_success`
   submessage (pool's own `REPLY_FLASH = 100`).
2. The borrower must leave `snapshot + fee` of each borrowed token back in the pool
   **before that callback's message tree returns**. Repayment is verified by
   balance delta in the pool's `reply_flash`.
3. Repayment must be a **direct transfer** (Bank `Send` / CW20 `Transfer`) — *never*
   CW20 `Send`, which would re-enter the reentrancy-locked pool. A reentrancy lock
   blocks every pool mutator (incl. the same pool's swap) for the whole callback.

Because the aggregator's whole route runs **depth-first inside the `FlashCallback`
Execute**, the existing reply-driven state machine (`proceed_to_next_step`) drives
the cycle unchanged. It only has to be kicked off from inside `FlashCallback`, and
end by repaying the pool instead of paying the user. The aggregator's
`create_send_msg` already uses Bank `Send` / CW20 `Transfer` — exactly the
repayment method the pool requires.

## Message flow

```
caller ──ExecuteMsg::FlashRoute{ flash_pool, flash_asset, flash_amount, stages, min_profit }──▶ aggregator
  aggregator: validate + guard cycle + map flash_asset→token0/token1
              save PENDING_FLASH ctx
              emit  WasmMsg::Execute(flash_pool, Flash{ recipient: self, amount0/amount1, data:"" })
  pool: lend tokens to aggregator
        SubMsg.reply_on_success(REPLY_FLASH) → ExecuteMsg::FlashCallback{ fee0, fee1, data } on aggregator
    aggregator FlashCallback:
        load+remove PENDING_FLASH (absent ⇒ forged call, reject)
        assert info.sender == ctx.flash_pool
        fee = ctx.flash_is_token0 ? fee0 : fee1 ;  repay = principal + fee
        build ExecutionState{ accumulated=[flash_asset, principal],
                              plan.flash_repayment=Some{pool, asset, repay, min_profit} }
        proceed_to_next_step(...)            ← existing engine runs the cycle
        ... reply chain ...
        handle_final_stage / final-conversion → finalize_route():
            assert total ≥ repay + min_profit  (else FlashProfitNotMet ⇒ whole tx reverts)
            send repay   → flash_pool   (direct transfer)
            send surplus → initiator
  pool reply_flash: balance ≥ snapshot+fee ✓, accrue fee, release lock
```

`data` is unused: both ends are controlled via `PENDING_FLASH` storage, so we pass
an empty `Binary`. Storage (an `Item`) is correct because the whole flow is one
atomic tx — only one flash is ever in flight.

## Code changes

### `src/msg.rs`
- `clmm` module: add `ClmmPoolFlashMsg::Flash { recipient, amount0, amount1, data }`
  (wire-matches `choice_clmm_common::pool::ExecuteMsg::Flash`).
- `ExecuteMsg`: add
  - `FlashRoute { flash_pool, flash_asset, flash_amount, stages, min_profit }`
  - `FlashCallback { fee0, fee1, data }` — variant tag `flash_callback`, fields in
    the same order as `FlashCallbackMsg::FlashCallback`, so the pool's serialized
    callback decodes straight into it.

### `src/state.rs`
- `RoutePlan`: add `flash_repayment: Option<FlashRepayment>`.
- `FlashRepayment { pool: Addr, asset: amm::AssetInfo, repay_amount: Uint128, min_profit: Uint128 }`.
- `PendingFlashCtx { flash_pool, flash_asset, flash_is_token0, principal, stages, min_profit, initiator }`
  + `PENDING_FLASH: Item<PendingFlashCtx>`.

### `src/execute.rs`
- `execute_flash_route(...)`: validate stages/percents; **guard** that no `ClmmSwap`
  op routes through `flash_pool` (reentrancy lock would revert the tx); query the
  pool's `GetConfig {}` to map `flash_asset → token0/token1` and set
  `amount0`/`amount1`; save `PENDING_FLASH`; emit the `Flash` message.
- `execute_flash_callback(...)`: load+remove `PENDING_FLASH` (absence ⇒
  `NoPendingFlash`); assert `info.sender == ctx.flash_pool`; pick `fee`; build the
  `ExecutionState` with `flash_repayment`; call `proceed_to_next_step`.

### `src/reply.rs`
- Extract `finalize_route(deps, reply_id, exec_state, total, asset_info)`:
  - flash branch: assert `total ≥ repay_amount + min_profit`; repay pool; surplus → sender.
  - normal branch: existing `minimum_receive` check; total → sender.
- `handle_final_stage`: force the normalization target to `flash_repayment.asset`
  when present; route both scenario A and the empty-accumulated case through
  `finalize_route` / a `FlashProfitNotMet` error.
- `handle_final_conversion_reply`: call `finalize_route` on completion.
- `execute_aggregate_swaps_internal`: set `flash_repayment: None`.

### `src/error.rs`
- `NoPendingFlash`, `FlashPoolInCycle`, `FlashAssetNotInPool`,
  `FlashProfitNotMet { required, actual }`.

### `src/contract.rs`
- `execute`: dispatch `FlashRoute` / `FlashCallback`.

## Safety notes / review checklist

- **Forged callback**: `FlashCallback` only runs if `PENDING_FLASH` is set (by our
  own `FlashRoute`) *and* `info.sender == ctx.flash_pool`. Without a real loan the
  borrowed funds aren't present and the route fails anyway, but the storage gate
  blocks the call up-front so no idle aggregator dust can be spent.
- **Reentrancy**: the cycle must not touch `flash_pool`; guarded at `FlashRoute`.
- **Same-asset close**: `finalize_route` forces the normalization target to the
  borrowed asset and asserts the repayment+profit floor; a cycle that fails to
  return the borrowed asset can't repay → the pool's `reply_flash` reverts the tx.
- **Fee is dynamic**: `fee0/fee1` come from the pool at callback time — never
  precomputed in `FlashRoute`. `min_profit` must exceed the flash fee or every
  fire reverts.
- **Reply namespace**: the pool's `REPLY_FLASH = 100` lives in the pool's reply
  space; the aggregator sees `FlashCallback` as a plain Execute, so it never
  collides with the aggregator's `REPLY_ID_COUNTER`.

## Tests (deferred — heaviest lift)

`mock_swap` can't lend, so flash tests need the **real `choice_clmm_pool.wasm`**
`include_bytes!`'d into the aggregator harness, instantiated with seeded in-range
liquidity:
- happy path: borrow from a cheap pool, profitable cycle through a mock AMM/OB,
  surplus ≥ `min_profit`;
- reverts: no-profit cycle, repay shortfall, cycle that touches `flash_pool`,
  `min_profit` below the flash fee, forged `FlashCallback` (no `PENDING_FLASH`).
