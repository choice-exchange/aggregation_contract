# v2.2.1 — pay in the asked-for form, White Whale quotes, form changes in the gate

Three changes, all additive. **A message without the new field behaves exactly as
2.2.0** (code id 2067): this is pinned by a test that runs every route shape on the
live 2.2.0 bytecode and on 2.2.1 and compares the two runs verbatim (see Tests).
The arb bot and v1 callers do not need to change anything.

Release: [RELEASE_v2_2_1_RUNBOOK.md](RELEASE_v2_2_1_RUNBOOK.md).

---

## 1. `receive_as`: pay the output in the form the caller asks for

### The problem

`handle_final_stage` normalizes a route's output onto the form of the **first asset
the last stage accumulated**, and pays that. A route whose last hop is a CW20 AMM
pool therefore pays the CW20, even when the user holds the token as its cw20-adapter
bank denom (`factory/<adapter>/<cw20>`).

Choice v2's EVM users hold the bank form, because the x/erc20 (MTS) pair is on the
bank denom. Two tokens are affected today:

| token | CW20 | bank (adapter `inj14ejqj…`) |
|---|---|---|
| SHROOM | `inj1300xcg9naqy00fujsr9r8alwk7dh65uqu87xm8` | `factory/inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk/inj1300x…` (EVM `0x4e03d02a…cd8d`) |
| NONJA | `inj1fu5u29slsg2xtsj7v5la22vl4mr4ywl7wlqeck` | `factory/inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk/inj1fu5u…` |

Every SHROOM/INJ AMM pool pays CW20, so today the v2 router can only pay bank SHROOM
through Helix's book (market `0xc6b6d662…f934`, `min_notional` 0.01 INJ). Below
about 0.02 INJ there is no route at all.

### The field

```jsonc
// ExecuteMsg::ExecuteRoute, Cw20HookMsg::ExecuteRoute and QueryMsg::SimulateRoute
"receive_as": { "native_token": { "denom": "factory/inj14ejq…/inj1300x…" } }   // or
"receive_as": { "token": { "contract_addr": "inj1300x…" } }
```

`Option<AssetInfo>`, `#[serde(default)]`. Absent or `null` means the old behaviour.

**Why an asset, not a `cw20 | bank` flag.** A flag is shorter, but then the contract
picks the denom and the caller can only hope it is the one they meant. With an asset
the caller states the exact denom it expects to receive and the contract checks it.
Two mistakes a flag would let through are refused instead:

- a route that does not end where the caller thinks (the router built a USDT route,
  the frontend thinks it is buying SHROOM);
- a bank denom minted by a different adapter than the one in `Config`, for example
  after an `UpdateCw20Adapter`.

It is also the shape both callers already have in hand. The router knows the user's
token as an `AssetInfo`, and the frontend's `vetWasmRoute` already checks "the last
op pays this bank denom"; it can check "`receive_as` is this bank denom" instead.

**Why `receive_as`.** It reads as what it does, and pairs with `minimum_receive`.

### Semantics

When `receive_as` is set:

1. Every output the last stage accumulated must be the **same asset** as
   `receive_as` under adapter identity, otherwise the route fails with the new
   `ReceiveAsMismatch`. It chooses the payout's form, never its asset.
   "Same asset" is `same_asset_under_adapter` (`reply.rs`): identical, or a CW20 and
   `factory/<configured adapter>/<that cw20>`. It is built on the existing
   `canonical_key` but is stricter in one place: a CW20 and a native only match when
   the native really carries the adapter prefix. `canonical_key` alone would also
   pair `Token{X}` with a bank denom literally named `X`, which no conversion connects.
2. The output is normalized onto `receive_as` by the same adapter conversion
   `handle_final_stage` already used for routes ending in mixed forms
   (`create_conversion_msg`, `Awaiting::FinalConversions`). No new `Awaiting` state,
   and no new transition.
3. `minimum_receive` applies to what is paid. The conversion is 1:1, so that is the
   tracked output.
4. Flash cycles are unaffected: they always repay in the borrowed asset, and
   `FlashRoute` has no such field.

Asking for the form the route already pays in is a no-op: same payout, same events,
and the adapter is not called.

### Fund safety

Both invariants hold through the new path, and both needed something:

- **A route may only move what it brought in.** When `receive_as` is the adapter
  sibling of the route's output, no op touches it, so `snapshot_entry_balances` would
  not have baselined it, and the payout clamp in `finalize_route` would have failed
  closed (`UnsnapshottedAsset`). It is now snapshotted at entry alongside the ops'
  assets. The conversion's input is clamped (`clamp_to_route_funds`) exactly like any
  other final normalization, and the converted payout is bounded by
  `route_spendable(receive_as)` (`OutputNotBacked`).
- **A successful route leaves nothing behind.** `receive_as` is in `entry_balances`,
  so `build_residue_sweep` covers it and nets the payout out of it.

Pinned by `test_receive_as_pays_a_cw20_output_in_bank_form` (idle balances of BOTH
forms sit in the contract and are neither paid out nor moved) and
`test_receive_as_cannot_pay_out_an_unbacked_output` (a phantom pool that reports a
CW20 output it never delivers, paired with `receive_as` = bank: the idle balance of
neither form moves, and the route dies at its own `minimum_receive`).

`receive_as` is caller-controlled on the Choice instance, like everything else in the
message. The worst a hostile value can do is fail the route: it can name only the
route's own output asset, converted through the configured adapter, and the amount
paid is bounded by the real balance delta of that form.

### ⚠️ Roll-out hazard: 2.2.0 silently ignores the field

`#[cw_serde]` here does not deny unknown fields. A **2.2.0** instance therefore
*accepts* a message carrying `receive_as`, ignores it, and pays the CW20. Callers
must only send the field to an instance that reports cw2 `2.2.1`. This is pinned by
`test_migrate_live_v2_2_0_instance_to_v2_2_1`. Deploying a new 2.2.1 instance for
Choice v2 (runbook option A) avoids the problem entirely.

### Gas

`test_receive_as_gas`, measured on injective-test-tube. Each route is a typical
Choice swap: 1 INJ through one AMM hop into SHROOM. Each instance runs a warm-up
route first, so every measured run updates storage rather than creating it.

| route | v2.2.0 | v2.2.1 | v2.2.1 vs no field |
|---|---:|---:|---:|
| INJ → CW20 SHROOM, no field | 858,682 | 858,644 | — |
| same, `receive_as` = CW20 (no-op) | | 862,368 | +3,724 (+0.4%) |
| same, `receive_as` = bank (CW20 → bank) | | 1,083,133 | +224,489 (+26%) |
| INJ → bank SHROOM, no field | 544,533 | 544,576 | — |
| same, `receive_as` = CW20 (bank → CW20) | | 1,148,142 | +603,566 (+111%) |
| USDT → INJ orderbook buy, no field (mean of 2 alternated rounds) | 497,661 | 497,678 | — |

- **No field: no measurable difference** (−38, +43 and +17 gas). The arb bot's
  routes cost what they cost on 2.2.0. In the orderbook rows, whichever version
  runs second pays about +1,750 gas, because the book has moved. That is why
  those rounds are alternated and averaged.
- **No-op `receive_as`: +3.7k.** This is the field's bytes in the tx and in each
  `ACTIVE_ROUTES` write.
- **CW20 → bank: +224k.** One adapter conversion (CW20 `Send` → adapter mint) and its
  reply, plus cheap bank balance checks on the bank form. This is the direction
  Choice v2 needs.
- **bank → CW20: +604k.** The conversion (`RedeemAndTransfer` → CW20 `Transfer`)
  and its reply. On top of that, the CW20 form's balance checks (entry snapshot,
  payout clamp, residue sweep) are CW20 `Balance` smart queries, which a route
  paying bank never makes. Expensive, but needed only when someone asks for CW20
  out of a bank-paying route.

## 2. `SimulateRoute` on White Whale pools (`msg.rs`)

`amm::SimulationResponse` required `return_amount`, `spread_amount` and
`commission_amount`. White Whale's simulation reply has no `commission_amount`
(it reports `swap_fee_amount`, `protocol_fee_amount` and `burn_fee_amount`
instead), so `SimulateRoute` failed on every White Whale pool while execution, which
never reads this type, worked.

Confirmed against mainnet on 2026-10-29. White Whale bINJ/INJ pair
`inj1gk4thnx4t9y4ltnau60mpst5xyu0u57hqfl0qs` (code 772), one of the three White
Whale hops in the working tx `6F067FFE1E4E895754B9AACEBA93903D670356ECFBE0A0F6F809DAFC8BFFFC2E`:

```json
{"return_amount":"950126981510811866","spread_amount":"1822556014244424",
 "swap_fee_amount":"1905971878657596","protocol_fee_amount":"952985939328798","burn_fee_amount":"0"}
```

The live Choice instance on that pool:

```text
kind: Serialization, error: missing field `commission_amount` at line 1 column 172
```

Only `return_amount` is read, so `spread_amount` and `commission_amount` are now
`#[serde(default)]`. Unknown fields were already ignored: the same instance decodes
Choice pairs' `Pair {}` replies, which carry fields `PairInfo` does not model. White
Whale's `Pair {}` reply has the standard `asset_infos`, so the output-asset
derivation needs no change.

Pinned by the unit test `test_simulate_white_whale_pool_reply`, which decodes both of
that pool's replies verbatim, and by the integration test
`test_simulate_route_on_a_white_whale_pool`. There, `mock_swap` answers in White
Whale's shape (new test-only `SetWhiteWhaleSimulation {}`): 2.2.0 fails with the
mainnet error, and 2.2.1 quotes exactly what execution delivers.

## 3. `SimulateRoute` converts CW20 ⇄ bank between hops, as execution does (`query.rs`)

Execution hands each hop its input in the form that hop takes: `handle_swap_reply`
compares the carried asset with `get_operation_input(next_op)` and, when they differ,
converts it through the adapter (`Awaiting::PathConversion`). `simulate_route` passed
each hop the previous hop's output as-is, so every route that changes form mid-path
failed to simulate although it executes.

Confirmed live. Choice SHROOM/INJ pair `inj1uyjjnykz0slq0w4n6k2xgleykqk9k5qkfctmw5`
(pays CW20) into the Helix SHROOM/INJ book:

```text
kind: Other, error: Orderbook simulation only supports native token inputs
```

`relabel_for_hop` now does the same thing between hops. The conversion is 1:1, so
only the label changes. It shares the executor's code rather than mirroring it:

- the next hop's input comes from the executor's own `get_operation_input`, which
  also replaces the query path's duplicate `get_path_start_info`;
- the "can these be converted" test is the same `same_asset_under_adapter` used by
  `receive_as`.

A relabel between two assets the adapter cannot connect is **refused** rather than
quoted. Execution would either have the adapter reject it or mint a third asset the
next hop cannot spend.

Cost: one `load_market` per non-first orderbook hop in the query, which is exactly
what execution already pays in `handle_swap_reply`. AMM and CLMM hops read their
input from the op, at no cost.

Pinned by `test_simulate_matches_execution_when_a_path_changes_form`, with both
directions (a CW20 leg into an orderbook hop, and a bank leg into a CW20 AMM). On
each, 2.2.0 fails and 2.2.1 quotes exactly what execution then produces. Unit tests:
`test_simulate_relabels_between_hops_like_execution` and
`test_simulate_rejects_mid_path_relabel_between_different_assets`.

---

## Backward compatibility

- **Wire:** all three fields are `#[serde(default)]`, and every existing message
  deserializes unchanged.
- **Behaviour without the field:** `test_without_receive_as_behaves_byte_for_byte_like_v2_2_0`.
  The test runs five route shapes on an instance of the live 2.2.0 bytecode and on
  2.2.1, sending the JSON exactly as a pre-2.2.1 caller does (key absent, not
  `null`). For each it asserts identical contract events, identical transfers to and
  from the aggregator, identical user balance changes and identical contract
  balances. The five shapes are native in to CW20 out, a last stage ending in both
  forms, a CW20-to-bank conversion between hops, an orderbook buy (price-improvement
  carve plus residue sweep), and the CW20 `Receive` entry point.
- **State:** `RoutePlan.receive_as` is `skip_serializing_if = "Option::is_none"`, so
  a route without the field writes byte-identical `ACTIVE_ROUTES` entries and pays
  the same storage gas. Persistent stores are unchanged, so `migrate` stays a no-op
  version bump (2.2.0 → 2.2.1, `test_migrate_live_v2_2_0_instance_to_v2_2_1`).
- **Query error strings** for malformed orderbook ops in `SimulateRoute` now come
  from the executor's `InvalidOrderbookDenom`, a consequence of sharing
  `get_operation_input`.

## Not changed, deliberately

- **The gate cannot see an adapter refusal.** `SimulateRoute` does not ask the
  adapter whether it would convert, either mid-path (as in 2.2.0) or for
  `receive_as`. The realistic case, a CW20 the adapter has never registered, cannot
  arise for `receive_as`: a user only holds the bank form of a registered CW20. If it
  does happen the whole route reverts and nothing moves
  (`test_receive_as_reverts_atomically_when_the_adapter_refuses`).
- **Tax (reflection) tokens and the adapter.** Adapter conversions use a plain
  CW20 `Send`, as they always have mid-path. The payout amount is read back from the
  conversion's events, so a taxed conversion is paid correctly, but the gate's 1:1
  quote overstates it by the tax. FOOL (`inj155mtz…amtwz`) is the only tax token
  registered on the Choice instance. Its bank form exists (about 42,256 FOOL
  wrapped), so a FOOL route with `receive_as` would be paid correctly but quoted
  slightly high. `minimum_receive` is what protects the user there, as it does on
  every FOOL hop today.
- **Mid-path conversions in execution are untouched.** Execution still hands any
  form mismatch to the adapter and lets the clamps catch nonsense. Tightening that
  would change behaviour for messages without the new field.

## Tests

**24 unit and 68 integration tests pass** (19 + 55 in 2.2.0; 5 new unit, 13 new
integration). Every existing test is unchanged apart from the new `receive_as: None`
field. `cargo clippy --all-targets` is clean.

⚠️ Run the integration suite **one test per process** on a near-full disk. Each
test-tube chain keeps a ~32 MB wasm cache until its process exits, so a plain
`cargo test` holds about 2 GB at its peak and fails with
`No space left on device` when less is free.

`./build_release.sh` is reproducible: two builds produced the same
`dex_aggregator.wasm`, sha256 `712ac695107fbe5fa098adbd1476c8ec1543495bf7c38422ef05d9e85d83a224`,
902,681 bytes.

New integration tests (`tests/integration.rs`):

| test | pins |
|---|---|
| `test_receive_as_pays_a_cw20_output_in_bank_form` | §1 CW20 → bank, `minimum_receive` on what is paid, gate parity, idle balances of both forms untouched |
| `test_receive_as_on_the_cw20_receive_hook` | §1 on the CW20 `Receive` entry point |
| `test_receive_as_pays_a_bank_output_in_cw20_form` | §1 bank → CW20 |
| `test_receive_as_normalizes_mixed_forms_onto_the_requested_one` | §1 last stage ending in both forms |
| `test_receive_as_in_the_form_already_paid_is_a_no_op` | §1 no-op: identical events, adapter never called |
| `test_receive_as_naming_another_asset_is_rejected` | §1 `ReceiveAsMismatch`, in the gate and in execution |
| `test_receive_as_reverts_atomically_when_the_adapter_refuses` | §1 adapter refusal reverts everything |
| `test_receive_as_cannot_pay_out_an_unbacked_output` | §1 invariant 1 through the new path |
| `test_without_receive_as_behaves_byte_for_byte_like_v2_2_0` | no-field parity with the live code |
| `test_migrate_live_v2_2_0_instance_to_v2_2_1` | the migration, and the roll-out hazard |
| `test_simulate_route_on_a_white_whale_pool` | §2 |
| `test_simulate_matches_execution_when_a_path_changes_form` | §3 |
| `test_receive_as_gas` | the gas table above |

`tests/fixtures/dex_aggregator_v2_2_0.wasm` is the live bytecode (sha256
`8e86385b…09fb4`, equal to code 2067's on-chain `data_hash`). It is the baseline for
the parity, migration and "2.2.0 fails this" checks.
