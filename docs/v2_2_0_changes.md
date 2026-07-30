# v2.2.0 — fund safety, fee ordering, and gate parity

Supersedes v2.1.0 (which was never uploaded). Nine changes from a security and
correctness audit of the whole contract, plus eight regression tests. Every finding
below reproduced against the v2.1.0 tree with a failing test before it was fixed.

Read this before touching `reply.rs::finalize_route`, `snapshot_entry_balances`, or
`orderbook_exec::build_swap_order_msg`.

## Why the version moved again

`migrate` calls `cw2::ensure_from_older_version`, which only writes the stored
version when the new one is strictly greater. Shipping this as `2.1.0` would leave
no on-chain way to tell which of the two trees actually landed. Mainnet is currently
`2.0.1` (code id 2060) on both instances.

## The theme: the engine trusted numbers it had no way to check

The route engine tracks amounts **virtually** — a hop's output is a number the venue
reported, in a denom the venue named. `pool_address` is caller-supplied and there is
no allowlist. So "how much did this hop produce, and of what" was, in the end,
whatever an arbitrary contract chose to write into its own event log.

Three of the changes below are that one problem in different clothes, and the fix is
a single invariant: **a route may only ever move what the route itself brought in.**

---

## 1. The output asset is resolved up front, never read back from the reply (`msg.rs`, `reply.rs`, `execute.rs`)

`get_operation_output` used to be `parse_ask_asset_from_events` — it took the first
`ask_asset` attribute across the reply's wasm events. Combined with
`parse_amount_from_swap_reply` taking the first `return_amount`/`amount_out`, a
contract the caller wrote could emit

```
wasm: action=swap  ask_asset=<any denom>  return_amount=<the aggregator's balance>
```

while transferring nothing, and `finalize_route` would pay that out. Cost of the
attack: **1 wei of INJ**. `minimum_receive` did not contain it — the attacker sets
it to 1.

`AmmSwapOp` and `ClmmSwapOp` gained `ask_asset_info: Option<AssetInfo>`. Supplied,
it is used directly (free, and what the arb bot should do); omitted, it is derived
from the pool's `Pair {}` / `GetConfig {}` — the side that isn't the offer, exactly
as `SimulateRoute` has always derived it. Resolution is **strict**: an op whose
output cannot be determined fails the route rather than proceeding unbounded.

This is additive on the wire; existing route JSON deserializes unchanged.

## 2. Every outgoing amount is bounded by what the route brought in (`reply.rs`)

`snapshot_entry_balances` now records the offer denom plus every op's input **and
output**. `route_spendable(info) = current_balance − entry_baseline` is then applied
to every amount that leaves the contract:

- each split's swap input (with a running per-denom budget across the stage, so two
  splits cannot both draw against the same real funds),
- each mid-path carry between hops of a multi-hop path,
- each adapter conversion,
- the final payout in `finalize_route`, which now errors with `OutputNotBacked`
  rather than promising more than it holds.

It fails closed: an asset with no baseline cannot be bounded, so it cannot be moved
(`UnsnapshottedAsset`).

The clamp is a **no-op on an honest route** — the contract necessarily holds at least
the tracked output. On a forged one the fabricated credit collapses to zero and the
route dies at its own `minimum_receive` floor.

> Why both the dispatch clamp and the payout clamp are needed: clamping only the
> payout still lets a route fabricate an intermediate credit and *spend* it through a
> real pool on the next stage, converting the aggregator's idle balance into
> something the payout clamp then finds legitimately backed.

**Cost.** One balance query per dispatched hop, plus one `Pair {}`/`GetConfig {}` per
AMM/CLMM op at route entry when `ask_asset_info` is omitted. Supplying
`ask_asset_info` removes the latter entirely — the arb path should.

## 3. `FEE_MAP` fees are accrued, not sent from the reply that charged them (`state.rs`, `reply.rs`)

The fee transfer was `add_message`d **after** `proceed_to_next_step`. Submessages
recurse depth-first, so the whole rest of the route — including `finalize_route` and
its residue sweep — executed first. The sweep saw the still-unsent fee as residue in
a snapshotted denom and paid it **to the user**; the queued transfer then found an
empty balance:

```
spendable balance 0usdt is smaller than 1000000usdt: insufficient funds
```

Any `SetFee` on a pool used anywhere except a route's last stage was enough — and
every closed cycle (offer denom == final asset) too. Latent only because `FEE_MAP`
is empty on both mainnet instances; the first `SetFee` would have bricked those
routes.

`ExecutionState` gained `pending_pool_fees`, paid by `build_residue_sweep` out of the
same residue it is part of. Kept separate from `pending_fees` because `is_flash`
suppresses the orderbook surplus carve but must **not** suppress the per-pool fee.

## 4. An orderbook BUY's unspent input returns to the user (`reply.rs`)

`ob_buy_surplus` computed the user's share as
`reserved · (order_qty − filled) / order_qty` — exactly **zero** whenever the order
fills completely. So on every fully-filled buy the entire leftover went to the fee
collector, regardless of how little of the input the order had actually spent.

Measured on a single-level fill, where the order is placed at the resting ask and
price improvement is provably zero: **1.002249 USDT of a 1000.999999 USDT buy to the
collector, 0 back to the user** — about 10 bps, on an instance whose `AllFees`
reports none.

The protocol's share is now **price improvement on the filled quantity and nothing
else**: `(order_price − fill.price) · filled`, still capped at what the chain
actually refunded. Everything else — the quantity-tick flooring loss, the `(1 + fee)`
sizing headroom, the relayer-fee rebate, the margin against unfilled base — is the
user's and is returned by the residue sweep. `SubmsgReplyState` carries
`ob_order_price` to make this measurable.

## 5. Direct mode snaps to the market's ticks and to what the hop holds (`orderbook_exec.rs`)

`build_swap_order_msg`'s direct branch was `(Some(q), Some(p)) => (p, q)` — it
checked `min_notional` and zero-quantity, but applied neither
`min_quantity_tick_size` nor `min_price_tick_size`, and ignored `input_amount`
entirely. The chain rejects an off-grid order outright:

```
quantity 0.001500000000000000 must be a multiple of the minimum quantity tick size 0.001000000000000000
```

…taking the whole route with it. Direct mode is the arb path.

New `direct_order_params` applies three adjustments, each of which can only produce
an order no worse than the caller asked for:

1. Price snapped to the price tick, in the safe direction — a BUY's bound is a
   ceiling (round down), a SELL's a floor (round up).
2. Quantity floored to the quantity tick — never more than asked, never off-grid.
3. Quantity bounded by what the hop actually holds. A mid-route leg's true input is
   only known on chain, so a caller-fixed quantity is routinely too large by the time
   it runs; this turns a certain revert into a partial fill.

The BUY affordability bound costs one `query_market_atomic_execution_fee_multiplier`
(a params lookup, not a book walk); SELL needs no query at all.

## 6. A hop that cannot place an order is a zero-value path, not a revert (`execute.rs`)

`create_swap_cosmos_msg` turned `build_swap_order_msg`'s `Ok(None)` (sub-tick, or
below `min_notional`) into `ContractError::AmountTooSmall`, which `?`-propagated out
of `execute_planned_swaps` and killed the **entire** route. Meanwhile the estimators
return `no_fill_estimate`, so `SimulateRoute` priced that hop at zero and quoted the
rest of the route as fine. Measured: gate says `742500`, chain says

```
Orderbook order quantity rounds to zero (input below one tick)
```

It is now `Ok(None)` — a graceful zero-value path, matching the CLMM zero-quote arm
and matching what the gate quoted. The split's allocation is returned by the residue
sweep. `AmountTooSmall` is retained in the error enum for API compatibility but is no
longer returned.

## 7. `SimulateRoute` quotes the order the executor submits (`query.rs`)

Three separate divergences:

- **Direct mode was ignored entirely.** The simulator read neither `quantity` nor
  `worst_price` and walked the book instead, describing a different order than the
  one that gets placed. It now sizes through the same `direct_order_params` the
  executor builds with (`direct_mode_estimate`).
- **Stage allocation differed.** The simulator drew each split from the pile matching
  that split's own input asset; the executor sums the native and CW20 piles into one
  `total_logical_amount` under adapter identity and gives the last split the
  remainder. So the gate quoted multi-denom stages the executor rejects with
  `MixedAssetsInStage`, and quoted ~zero for any split whose input the executor would
  have produced by an adapter conversion. Now identical, including the mixed-asset
  rejection.
- **Outputs were summed across denoms.** `current_assets.iter().map(|a| a.amount).sum()`
  reported 100 USDT plus 5 INJ as `105`. Summing is only valid when the outputs are
  the same asset under adapter identity (a CW20 and its `factory/<adapter>/<cw20>`
  wrapper), which is what `handle_final_stage` normalizes; anything else now errors,
  as the executor would.

## 8. Percentage sums are validated on every stage (`execute.rs`, `query.rs`)

Only `stages[0]` was checked, and only in the executor. A later stage summing to less
than 100 silently handed the shortfall to its last split (which absorbs the
remainder); one summing to more underflowed that split's `checked_sub`. `SimulateRoute`
did neither. `validate_stages` is now shared by `ExecuteRoute`, `FlashRoute` and
`SimulateRoute`, and also rejects empty stages and empty paths.

## 9. Two smaller corrections

- **Stage allocation vs. conversion sizing (`reply.rs`).** `plan_next_stage` sized
  conversions from `floor(pct · total / 100)` for every split, but allocated the last
  split `total − Σ(earlier)`. Those differ by the rounding remainder
  `R ∈ [0, splits−1]`, so when the last split sat on the side being converted *away*
  from, R atomic units of its input had already been converted into the other asset —
  and the hop reverted for insufficient funds, or silently consumed the contract's own
  dust. Both are now derived from one allocation vector.
- **`FlashPoolInCycle` (`execute.rs`).** Only `ClmmSwap` ops were checked against the
  flash pool, on the assumption that "AMM/orderbook venues have distinct addresses" —
  an assumption about well-formed routes, which the caller supplies. Now every venue
  carrying a contract address is checked.

## Tests

Eight new, all failing on the pre-fix tree with the chain errors quoted above:

| test | pins |
|---|---|
| `test_hostile_pool_cannot_drain_contract_balance` | §1 + §2 |
| `test_fee_on_non_final_stage_is_paid_once_to_collector` | §3 |
| `test_ob_buy_unspent_input_returns_to_user` | §4 |
| `test_direct_mode_orderbook_quantity_is_tick_rounded` | §5 |
| `test_subtick_orderbook_split_is_a_zero_value_path_not_a_revert` | §6 |
| `test_simulate_matches_execution_for_direct_mode_orderbook` | §7 |
| `test_mixed_cw20_native_stage_converts_exactly_what_it_allocates` | §9 |
| `test_flash_route_cycle_through_flash_pool_rejected_for_amm_op` | §9 |

Plus three unit tests in `query.rs` for the gate's new rejections, and
`test_multi_stage_aggregate_swap_success` recalibrated: the user now receives
1.00375 USDT more, being the part of their buy input the order never spent (§4).

`contracts/mock_swap` gained a **test-only** `SetPhantom` mode — a pool that emits a
normal-looking swap event while transferring nothing. It is inert unless called.

Suite: 19 unit + 55 integration, all passing.

## Not fixed, deliberately

- **Splits sharing a pool within one stage are still mis-quoted** by both engines
  (every message in a stage is built before any executes). Documented on `Stage`;
  same-venue splits are a supported shape.
- **`config.admin` on the Choice instance is a hot EOA**
  (`inj1yrg4pg8hcu0sw5rjlrcqfmw2ewf2uztlmdysak`), not the 48h timelock — the timelock
  is only the *wasm* admin. `EmergencyWithdraw`, `UpdateCw20Adapter`,
  `UpdateFeeCollector` and `UpdateAdmin` are one key away, and `UpdateCw20Adapter`
  can reach users' in-flight funds. That is an operational decision, not a code one.
