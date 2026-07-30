# v2.1.0 — gate-blindness, fund-safety, and route ergonomics

> ⚠️ **SUPERSEDED and never shipped.** v2.1.0 was built but not uploaded; a security audit found
> five reproducing defects in it (two of which it had introduced) and the tree moved to
> **v2.2.0** — see [v2_2_0_changes.md](v2_2_0_changes.md), which is the current reference.
>
> This document is kept because the reasoning below is still correct and still worth reading:
> §1–3 and §7–11 shipped essentially unchanged in v2.2.0. But note that §2's `min_notional`
> check in `build_swap_order_msg` returned an error that reverted the whole route (v2.2.0 §6
> makes it a zero-value path), §4's residue sweep did not yet bound the payout (v2.2.0 §2),
> and §5's `ob_buy_surplus` took the user's unspent input as well as the price improvement
> (v2.2.0 §4).

Supersedes v2.0.1. Eleven changes from one review pass over the whole contract, plus two
new regression tests.

Read this before touching `orderbook_exec.rs`, `reply.rs::finalize_route`, or
`create_swap_cosmos_msg` — several of the changes exist to close bugs that were invisible
to every check we had, and the reasoning matters more than the diff.

## Why the version moved

v2.1.0 is a **minor** bump: `ExecuteMsg` gained a variant and two ops gained optional
fields, all additive. Existing route JSON deserializes unchanged.

The bump is not cosmetic. `migrate` calls `cw2::ensure_from_older_version`, which *permits*
migrating between equal versions but only writes the stored version when the new one is
strictly greater. Shipping this bytecode as `2.0.1` would migrate silently and leave
`contract_info.version` reading `2.0.1` — leaving no on-chain way to tell whether the
migration landed.

## The theme: the pre-fire gate could not see what reverted

`SimulateRoute` is the pre-broadcast gate for the arb bot: a route that quotes clean is
broadcast, and one that doesn't is dropped. Several things reverted on chain that the gate
structurally could not model, so it kept clearing routes that were guaranteed to fail. Four
of the changes below are that one bug wearing different clothes.

The general rule this leaves behind: **anything that changes what the executor actually
submits must be visible to the query path, or the gate is lying.** The two paths
deliberately share `estimate_single_swap_execution`, `sell_base_quantity`,
`meets_min_notional`, and `apply_fee` for exactly this reason. Adding a new execution-side
adjustment without routing it through shared code re-opens the hole.

---

## 1. Sell-side tick flooring (`orderbook_exec.rs`)

`build_swap_order_msg` floored an orderbook SELL's base input to
`min_quantity_tick_size`, but `estimate_execution_sell_from_source` priced the **raw,
unfloored** input. The quote therefore over-reported by exactly the flooring loss — a
measured 2.03% on a 1.939378 ATOM leg at a 0.1 ATOM tick — and the gate passed routes the
chain then rejected at the min-receive floor. Buys were never affected;
`estimate_execution_buy_from_source` already rounded its derived quantity.

Both sides now call one helper, `sell_base_quantity(input, tick)`, so they cannot drift
apart again. A sub-tick input short-circuits to a no-fill estimate rather than walking the
book, because `get_minimum_liquidity_levels` rounds a partial level back **up** to one full
tick and would otherwise quote liquidity no order ever trades.

## 2. `min_notional` enforcement (`orderbook_exec.rs`)

The field was read nowhere in the repo. The chain rejects any spot order whose notional
(price × quantity) falls below the market's floor — 1e6, i.e. $1.00, on every INJ/ATOM
major — and neither the tick rounding nor the zero-quantity guard catches it: a leg can be
an exact multiple of the quantity tick, non-zero, and still be worth less than the floor.

`meets_min_notional` is checked in **`build_swap_order_msg` as well as both estimators**.
The order builder is the load-bearing one: direct mode supplies its own quantity and price
and never touches the estimators, so an estimator-only check would have left the arb path
uncovered.

## 3. `max_spread` is caller-settable and never `None` (`execute.rs`, `msg.rs`)

`AmmSwapOp` gained `max_spread: Option<Decimal>`.

Choice's own `assert_max_spread` is a genuine no-op when both it and `belief_price` are
`None`, so Choice-only routes were never exposed. **Astroport substitutes its own 0.5%
default and asserts on it**, while `SimulateRoute`'s `Simulation` query never applies that
assert — so any route with an Astroport leg could clear the gate and revert.

The wire value is now never `None`. Omitting the field yields `DEFAULT_AMM_MAX_SPREAD`
(49%), and a supplied value is clamped to `MAX_AMM_MAX_SPREAD` (also 49%). 49 rather than
the 50 Astroport permits, so a pair bounding with `>=` instead of `>` cannot reject every
swap. The route's mandatory `minimum_receive` / `min_profit` remains the real guard;
the pool-level assert only ever produced spurious reverts.

## 4. The residue sweep no longer skips the final asset (`reply.rs`)

**This one silently accumulated funds.** An orderbook BUY is sized at `worst_price`, fills
cheaper, and the chain refunds the difference. `accumulate_fee` accrues that refund in the
buy hop's **offer** denom. `pending_fees` is paid out in exactly one place —
`build_residue_sweep` — whose first act was `if info == final_asset { continue }`.

On a **closed cycle** (INJ → buy X/INJ → … → INJ) the buy's offer denom *is* the final
asset. So the surplus was paid to nobody, and it wasn't part of `total_amount` either — a
buy's tracked output is base, the refund is quote. It sat in the contract until an
`EmergencyWithdraw`, breaking the v2.0.1 invariant that a successful route leaves nothing
behind.

`build_residue_sweep` now takes `final_payout` and nets it out of that denom's residue
instead of skipping the denom. Fee-vs-user split is unchanged.

Why no test caught it: `test_orderbook_surplus_to_fee_collector_and_contract_drains`
exercises an **open** route (USDT → buy → INJ), where offer ≠ final and the sweep pays out
normally. See the new test in §12.

## 5. Flash cycles credit the buy surplus as profit and take no carve (`reply.rs`)

Consequence of §4, and a deliberate policy choice.

A flash cycle opening on an orderbook BUY is sized at `worst_price`, so most of its edge
returns as the **refund** rather than as tracked output. `finalize_route` measured only the
tracked amount against `repay + min_profit`, so a genuinely profitable cycle reverted with
`FlashProfitNotMet` — and the refund was then carved off to the fee collector rather than
being the caller's arb profit.

When `flash_repayment.is_some()`, `finalize_route` now adds the surplus accrued in the
output denom to `total_amount` *before* the profit gate, and `build_residue_sweep(…,
is_flash = true)` zeroes the protocol carve so all residue goes to `plan.sender`. A flash
caller is an allowlisted signer running their own capital; the orderbook surplus is their
arb profit, not aggregator revenue.

Double-payment is closed by construction: the credited surplus is inside `final_payout`, so
the sweep subtracts it from the same denom's residue.

> ### ⚠️ `ob_buy_surplus` overestimates — every consumer must clamp
>
> It derives the refund in `FPDecimal` from the decoded fill and lands a few atomic units
> **above** what the exchange actually returned (measured: 237 µUSDT on a ~938 USDT
> surplus). `build_residue_sweep` was always immune because it does `.min(residue)`.
>
> The first cut of this change credited `pending_fees` raw and blew up in `finalize_route`
> with `spendable balance 938552375usdt is smaller than 938552612usdt`, killing the whole
> cycle. It does not lose money — it makes **every flash cycle with a buy hop revert**.
> `cargo build`, `clippy` and all 17 unit tests were green at that moment; only the new
> integration test caught it.
>
> The credit is now bounded by the same residue the sweep would have paid:
> `accrued.min(current − entry_baseline − total_amount)`.

## 6. `UpdateCw20Adapter` (`msg.rs`, `execute.rs`, `contract.rs`)

Admin-only. The adapter address was fixed at instantiate with no setter, so a redeployed
adapter could only be picked up by a contract migration.

## 7. Path-conversion index recovery (`state.rs`, `reply.rs`)

`handle_path_conversion_reply` recovered `(split_index, op_index)` by searching the plan for
the **first** `Operation` equal to the pending one. An identical op can legitimately appear
in more than one split — `test_multi_split_to_same_orderbook_contract` proves that shape is
supported — so the resumed hop was attributed to the wrong split, and the reply then read
the wrong path entry.

`PendingPathOp` now carries `split_index` / `op_index`, recorded at dispatch. Reachable only
with a mid-path CW20↔native conversion, so this is a Choice-frontend concern rather than an
arb one.

## 8. `ClmmSwapOp::slippage_bps` (`msg.rs`, `execute.rs`)

The estimation-mode CLMM floor was a hardcoded `multiply_ratio(995, 1000)`. Now
`Option<u16>`, defaulting to `DEFAULT_CLMM_SLIPPAGE_BPS` (50 = the old 0.5%) and clamped to
10000. Ignored in direct mode. At 10000 the floor is disabled and the route's own
`minimum_receive` / `min_profit` is the only guard.

## 9. `MixedAssetsInStage` (`error.rs`, `reply.rs`)

`plan_next_stage` sums the native and CW20 piles into one `total_logical_amount`, which is
valid only under CW20-adapter identity — a bank denom and its wrapper are the same asset.
Two genuinely different natives (or CW20s) arriving in one stage were added together and
then allocated to splits as though interchangeable. That now rejects instead of silently
mis-routing.

## 10. One fewer chain query per orderbook hop (`execute.rs`, `state.rs`, `reply.rs`)

`handle_swap_reply` re-ran `load_market` purely to recompute one bool (`is_buy`). The market
is already loaded at dispatch, so `DispatchedSwap` and `SubmsgReplyState` now carry
`ob_is_buy`. A missing value **errors** rather than defaulting — guessing wrong inverts the
fill accounting.

Considered and rejected: caching the whole `SpotMarket` in `ExecutionState`. That state is
serialized and re-read on every reply for the route's whole life, so it likely costs more
than the queries it saves. The remaining `load_market` calls each genuinely need the market.

## 11. Same-pool splits: documented, deliberately not rejected (`msg.rs`)

Every split in a stage has its message built in `execute_planned_swaps` **before any of them
execute**, so two splits hitting the same venue price the second as though the first had not
traded. `simulate_route` is blind identically, being a single-snapshot query. The result is
an over-quote the gate cannot see.

Not rejected: same-venue splits are a supported shape with a passing test. Routers should
merge two splits that share a pool, or sequence them into separate stages; where a shared
CLMM pool is unavoidable, widen `slippage_bps` to cover the self-impact. Documented on
`Stage`.

## 12. Tests

- `test_closed_cycle_ob_buy_surplus_not_stranded` — USDT → OB buy INJ → OB sell INJ → USDT.
  Asserts the aggregator drains to zero in both denoms and the collector still receives the
  surplus when it is denominated in the final asset. Fails on pre-§4 code with ~1000 USDT
  stranded.
- `test_flash_route_credits_ob_buy_surplus_as_profit` — borrow 14k USDT → OB buy → AMM sell.
  The buy crosses asks 10 and 11, so it is sized at 11, fills ~1000 INJ at 10, and returns
  ~1k USDT as refund against a 14.042k repayment: **the cycle clears its floor only once the
  refund is credited**. Also asserts the fee collector gains exactly zero. Fails on pre-§5
  code with `profit floor not met`.

`setup_for_flash_test` gained a real INJ/USDT spot market (same book shape as `setup()`:
asks 10/11/12, bids 9/8/7) and a `fee_collector_addr`, since a flash cycle needs an
orderbook hop to produce a refund at all.

> Adding fields to `AmmSwapOp` / `ClmmSwapOp` breaks **Rust struct literals** — `#[serde(default)]`
> only helps JSON. 53 sites needed patching. Field order is free in Rust, so the cheap fix is
> `perl -pi -e 's/AmmSwapOp \{/AmmSwapOp { max_spread: None,/g'`.

## Verification

`cargo build` and `cargo clippy --all-targets` clean; 17/17 unit tests; 47/47 integration
tests.

**Not covered:** `min_notional` has no end-to-end test. `register_min_notionals` in the
harness registers a floor of `1`, so the rejection path never triggers at test sizes — only
the `test_meets_min_notional` unit test covers the predicate. It is the least-verified change
here.

## Applies to both deployments — check each

The same bytecode runs under two instantiations with different threat models. §1–3 and §7–9
matter most to the Choice frontend, where callers are untrusted and route fields are
attacker-controlled; §4, §5 and §10 matter most to the arb bot. Everything in §4 (fund
retention) affects both.

## Migration

Both instances currently run code id 2060 (v2.0.1). Deploying this needs a code upload and
then a migrate of **both**, through the Choice Admin Timelock
`inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9` (48h delay):

- Choice frontend — `inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0`
- Arb — `inj1vhu5z87dcuyyuz9e725kasecqygprl6jpkj7hx`

No state migration is required. Confirm `contract_info.version` reads `2.1.0` afterwards on
both — that is the check the version bump exists to make possible.

Unrelated but still pending: flash borrowing is **built and deployed yet switched off**.
`flash_signers` is empty on both instances and the CLMM factory's `is_flash_borrower` returns
false for the aggregator, so `FlashRoute` cannot execute until an `AuthorizeFlashBorrower`
on the factory and an `AuthorizeFlashSigner` here.
