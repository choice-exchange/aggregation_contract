# Aggregator v2.2.1 release — runbook

**Status: PREPARED, NOTHING EXECUTED.** Every step below that writes to chain needs
Dan's explicit go for that step. What changed and why:
[v2_2_1_changes.md](v2_2_1_changes.md).

| Item | Value |
|---|---|
| Source | [PR #19](https://github.com/choice-exchange/aggregation_contract/pull/19), code commit `c1b1ad5`. Later commits are docs only; verify by the wasm sha256 below, not by commit |
| Build | `cosmwasm/workspace-optimizer:0.17.0` (`./build_release.sh`) |
| wasm sha256 | `712ac695107fbe5fa098adbd1476c8ec1543495bf7c38422ef05d9e85d83a224` |
| Size | 902,681 bytes (2.2.0: 901,007) |
| cw2 | `crates.io:dex-aggregator` `2.2.1` |

## Instances, as they are on chain (verified 2026-10-29)

The brief this release was prepared from got two of these wrong. These are the
values the chain returned:

| instance | code | cw2 | wasm admin | `config.admin` | role |
|---|---|---|---|---|---|
| `inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0` | 2067 | 2.2.0 | **timelock** `inj14tm9…v9` | `choicedev` `inj1yrg4…sak` | Choice frontend (permissionless) |
| `inj1msu93x59wkgjnyh7z2ulx0hy7tmk2uxx0vzzcw` | 2067 | 2.2.0 | **`choicedev` EOA** | `choicedev` | the live arb bot (`inj_rust_arb_v2/config/base.toml:262`) |
| `inj1vhu5z87dcuyyuz9e725kasecqygprl6jpkj7hx` | **2060** | **2.0.1** | `inj1w0ul…455t` | `inj1w0ul…455t` | legacy rollback anchor, not the bot |

- Code 2067 has four more instances: `inj1saxg5…48re` and `inj1lr0k4…525n`, which
  are earlier arb instances, plus `inj1uraz…mqw` and `inj12rmy…duw`, which belong to
  third parties. None of them is part of this release.
- **Only the Choice instance is behind the timelock.** The arb instance can be
  migrated by `choicedev` directly, with no wait. Its bot sends no `receive_as` and
  needs nothing from 2.2.1 unless it wants White Whale or form-changing routes to
  pass its gate.
- Timelock `inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9` runs code 1999 ("lazy"), the
  **older** interface: `Propose { contract, code_id, msg }` and the
  `pending_migration {}` query. The newer `Propose { action }` / `pending_action`
  shape in `choice_exchange` HEAD does **not** apply to it. Delay 172800 s; owner
  `inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz` (keyring key `testnet` as of the
  2.0.1 release). Nothing is pending today.
- State the Choice instance holds: `FEE_MAP` empty, flash signers empty with
  `unrestricted: false`, and **one tax token registered: FOOL**
  `inj155mtzkc0366gnudw9v6wyc7x37dprfcl8amtwz`.

## ⚠️ Read first: 2.2.0 ignores `receive_as`

A 2.2.0 instance *accepts* a message carrying `receive_as`, ignores it, and pays the
CW20 (`test_migrate_live_v2_2_0_instance_to_v2_2_1`). The router and the frontend
must only send the field to an address that reports cw2 `2.2.1`. Option A below
makes that trivial: only the new address ever gets the field.

## Which path

- **A. New 2.2.1 instance for Choice v2 (recommended).** It is live within minutes,
  with no 48h wait, and the roll-out hazard above cannot arise. Instantiate it with
  the **timelock as wasm admin** from the start, so its future migrations are gated
  exactly like the current instance's. v1 keeps using `inj1520rss…` on 2.2.0,
  unchanged.
- **B. Migrate `inj1520rss…` through the timelock.** Do this if the v1 frontend
  should get the White Whale and form-change gate fixes, or to keep a single Choice
  address. It can run in parallel with A, or later.

The arb instance needs neither. Leave it on 2.2.0 unless the bot wants the gate
fixes. In that case, step C.

Shared environment for every step:

```bash
cd choice/aggregation_contract
export NODE=https://sentry.tm.injective.network:443
export LCD=https://sentry.lcd.injective.network
export CHAIN=injective-1
export TIMELOCK=inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9
export ADAPTER=inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk
export SHROOM=inj1300xcg9naqy00fujsr9r8alwk7dh65uqu87xm8
export SHROOM_BANK="factory/$ADAPTER/$SHROOM"
# export PASSWORD='<keyring passphrase>'   # never committed, never echoed

q() { curl -s "$LCD/cosmwasm/wasm/v1/contract/$1/smart/$(printf '%s' "$2" | base64 -w0)"; echo; }
txcode() { curl -s "$LCD/cosmos/tx/v1beta1/txs/$1" | python3 -c "import sys,json;r=json.load(sys.stdin)['tx_response'];print('code',r['code'],'height',r['height'],'gas',r['gas_used']);print(r.get('raw_log','')[:300])"; }
```

Always check a tx's `code` with `txcode`, not just that a txhash came back.

---

## 1. Store the code — needs Dan's go

> ⛔ **BLOCKED (2026-10-29): `choicedev` can no longer store code on mainnet.**
> Governance prop 697 (passed 2026-09-05) set `code_upload_access` to
> `AnyOfAddresses [inj1ez42atafr3ujpudsuk666jpjj9t53sehcynh3a]`. That address
> deploys Injective's "SigAuth Registry" (code 2093) and "RFQ Contract" (code 2094).
> The attempt `FD5273658F79CF7A3A0118815D74C93ADCF38428E6AE9F2D48F04F0F08BEF99C`
> failed with code 4, `can not create code: unauthorized`, after 3.19M gas, and cost
> 0.0045 INJ.
>
> Two ways to get the code stored:
>
> - **That uploader stores the wasm.** Verify the resulting code's `data_hash`
>   equals the sha256 below.
> - **A governance `MsgStoreCode` proposal.** Deposit 100 INJ, voting period 4 days.
>   Third-party store proposals 680 and 702 were rejected.
>
> Instantiation is still `Everybody`, so A1 onward work from any key once a code id
> exists.

```bash
sha256sum artifacts/dex_aggregator.wasm      # must print 712ac695107fbe5fa098adbd1476c8ec1543495bf7c38422ef05d9e85d83a224
GAS=9000000 FEES=4000000000000000inj ./scripts/upload_code_mainnet.sh
```

`GAS=3800000`, the script's default, is too low. 2.0.1 needed about 5.83M and the
first store ran out of gas. The script never checks the tx `code`, so an out-of-gas
store shows up as "Could not find Code ID". Run `txcode <store txhash>`, then:

```bash
export CODE=<new code id>
curl -s "$LCD/cosmwasm/wasm/v1/code/$CODE" | python3 -c "import sys,json;print(json.load(sys.stdin)['code_info']['data_hash'])"
# must equal 712ac695107fbe5fa098adbd1476c8ec1543495bf7c38422ef05d9e85d83a224 (upper-case on chain)
```

## A. New instance for Choice v2 — needs Dan's go

### A1. Instantiate, timelock as wasm admin

Same `Config` as the current Choice instance:

```bash
yes "$PASSWORD" | injectived tx wasm instantiate "$CODE" \
  '{"admin":"inj1yrg4pg8hcu0sw5rjlrcqfmw2ewf2uztlmdysak","cw20_adapter_address":"inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk","fee_collector_address":"inj1c2yleauy9say73tsx3dk5tvlgwwzdh96r76zv4"}' \
  --label "Choice DEX Aggregator v2.2.1" \
  --admin "$TIMELOCK" \
  --from choicedev --chain-id "$CHAIN" --node "$NODE" \
  --gas 600000 --fees 300000000000000inj --yes
```

`config.admin` stays the hot `choicedev` key, as on `inj1520rss…`. That is the
existing operational decision (CLAUDE.md, "The timelock is only the *wasm* admin"),
not a new one. Change it here if a different admin is wanted. Only `--admin` can
never be changed by anyone but the timelock afterwards.

```bash
export NEW_AGG=<address from the instantiate tx>
```

### A2. Register FOOL as a tax token

FOOL is registered on the current instance. Without it, the new instance routes FOOL
with plain CW20 `Send`/`Transfer` and the reflection tax hits every hop.

```bash
yes "$PASSWORD" | injectived tx wasm execute "$NEW_AGG" \
  '{"register_tax_token":{"contract_addr":"inj155mtzkc0366gnudw9v6wyc7x37dprfcl8amtwz"}}' \
  --from choicedev --chain-id "$CHAIN" --node "$NODE" \
  --gas 300000 --fees 150000000000000inj --yes
```

### A3. Verify the instance

```bash
curl -s "$LCD/cosmwasm/wasm/v1/contract/$NEW_AGG" | python3 -c "import sys,json;c=json.load(sys.stdin)['contract_info'];print(c['code_id'],c['admin'],c['label'])"
#   expect: $CODE  inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9  Choice DEX Aggregator v2.2.1
curl -s "$LCD/cosmwasm/wasm/v1/contract/$NEW_AGG/raw/$(printf contract_info | base64 -w0)" \
  | python3 -c "import sys,json,base64;print(base64.b64decode(json.load(sys.stdin)['data']).decode())"
#   expect: {"contract":"crates.io:dex-aggregator","version":"2.2.1"}
q "$NEW_AGG" '{"config":{}}'          # same three addresses as inj1520rss…
q "$NEW_AGG" '{"all_fees":{}}'        # {"fees":[]}
q "$NEW_AGG" '{"flash_signers":{}}'   # {"signers":[],"unrestricted":false}
```

Then run the live verification (section V) against `AGG=$NEW_AGG`.

### A4. Switch Choice v2 over — after V passes

These are outside this repo; listed so nothing is missed:

- `choice_v2_backend` router: the aggregator address, then send `receive_as` when the
  user's token is the bank form, and stop restricting bank-form payouts to
  Helix-ending routes.
- Router: drop the "unsimulable" marks for White Whale and mid-path form changes,
  **for this address only**. `inj1520rss…` still has both bugs until B lands.
- `choice_v2_frontend` `src/routing/wasm.ts` `vetWasmRoute`: the allowed aggregator
  address, and accept `receive_as` = the user's bank denom instead of requiring the
  last op to pay it.
- Backend volume attribution: add the new address to `AGGREGATOR_CONTRACTS`, and to
  whatever indexes `wasm-aggregator_swap`, or v2 volume disappears from the metrics.

## B. Migrate `inj1520rss…` through the timelock — needs Dan's go

### B1. Confirm nothing is queued

A later `Propose` overwrites the pending one and resets the timer.

```bash
q "$TIMELOCK" '{"pending_migration":{}}'
#   expect: {"data":{"contract":null,"code_id":null,"msg":null,"effective_at":null}}
```

### B2. Propose (owner key)

```bash
yes "$PASSWORD" | injectived tx wasm execute "$TIMELOCK" \
  "{\"propose\":{\"contract\":\"inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0\",\"code_id\":$CODE,\"msg\":\"e30=\"}}" \
  --from testnet --chain-id "$CHAIN" --node "$NODE" \
  --gas 500000 --fees 250000000000000inj --yes
q "$TIMELOCK" '{"pending_migration":{}}'
#   expect contract inj1520rss…, code_id $CODE, msg "e30=" (base64 of {}), effective_at = now + 172800
```

`e30=` is the aggregator's empty `MigrateMsg`. The migration is a no-op version bump
(2.2.0 → 2.2.1): persistent state is unchanged. **Wait 48h.**

To abort, `{"cancel":{}}` from the owner key works any time before someone applies.
Once the window opens, anyone can apply.

### B3. Apply (any funded key)

```bash
yes "$PASSWORD" | injectived tx wasm execute "$TIMELOCK" '{"apply":{}}' \
  --from choicedev --chain-id "$CHAIN" --node "$NODE" \
  --gas 2000000 --fees 1000000000000000inj --yes
txcode <apply txhash>
```

### B4. Verify

```bash
curl -s "$LCD/cosmwasm/wasm/v1/contract/inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0" | python3 -c "import sys,json;print(json.load(sys.stdin)['contract_info']['code_id'])"   # $CODE
curl -s "$LCD/cosmwasm/wasm/v1/contract/inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0/raw/$(printf contract_info | base64 -w0)" \
  | python3 -c "import sys,json,base64;print(base64.b64decode(json.load(sys.stdin)['data']).decode())"            # 2.2.1
q inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0 '{"config":{}}'   # unchanged
```

Then run section V with `AGG=inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0`.

## C. (Optional) the arb instance — needs Dan's go

Only if the bot wants White Whale or form-changing routes to pass its gate. The wasm
admin is `choicedev`, so there is no timelock and no wait:

```bash
yes "$PASSWORD" | injectived tx wasm migrate inj1msu93x59wkgjnyh7z2ulx0hy7tmk2uxx0vzzcw "$CODE" '{}' \
  --from choicedev --chain-id "$CHAIN" --node "$NODE" \
  --gas 1000000 --fees 500000000000000inj --yes
```

Behaviour for the bot's own messages, none of which carry the field, is identical
(`test_without_receive_as_behaves_byte_for_byte_like_v2_2_0`). The gas difference is
in the table in v2_2_1_changes.md.

---

## V. Live verification

Run it against whichever instance just changed (`export AGG=…`). Use small amounts
from any funded wallet (`choicedev` below). Each swap pins one change, and every one
checks the residue invariant: **the contract's balances are byte-for-byte unchanged
across the swap.**

```bash
snap() {   # every bank balance + CW20 SHROOM held by the aggregator
  curl -s "$LCD/cosmos/bank/v1beta1/balances/$AGG?pagination.limit=500" | python3 -c "import sys,json;print(sorted((b['denom'],b['amount']) for b in json.load(sys.stdin)['balances']))"
  q "$SHROOM" "{\"balance\":{\"address\":\"$AGG\"}}"
}
exec_route() {   # $1 = execute_route JSON, $2 = funds
  yes "$PASSWORD" | injectived tx wasm execute "$AGG" "$1" --amount "$2" \
    --from choicedev --chain-id "$CHAIN" --node "$NODE" \
    --gas 3000000 --fees 1500000000000000inj --yes
}
```

`final_received` and `swap_final_denom` are in the tx's `wasm` and
`wasm-aggregator_swap` events.

### V1. `receive_as`: INJ → SHROOM, paid as bank SHROOM

Choice SHROOM/INJ pair `inj1uyjjnykz0slq0w4n6k2xgleykqk9k5qkfctmw5` pays CW20, so
this is exactly the route that was impossible to pay in bank form.

```bash
STAGES='[{"splits":[{"percent":100,"path":[{"amm_swap":{"pool_address":"inj1uyjjnykz0slq0w4n6k2xgleykqk9k5qkfctmw5","offer_asset_info":{"native_token":{"denom":"inj"}},"ask_asset_info":{"token":{"contract_addr":"'$SHROOM'"}}}}]}]}]'
RA='{"native_token":{"denom":"'$SHROOM_BANK'"}}'
q "$AGG" '{"simulate_route":{"stages":'$STAGES',"amount_in":{"denom":"inj","amount":"10000000000000000"},"receive_as":'$RA'}}'
#   -> output_amount X  (≈1.36e21 at 2026-10-29 prices)
snap > /tmp/agg_before.txt
exec_route '{"execute_route":{"stages":'$STAGES',"minimum_receive":"<99% of X>","receive_as":'$RA'}}' 10000000000000000inj
snap > /tmp/agg_after.txt && diff /tmp/agg_before.txt /tmp/agg_after.txt && echo "residue invariant holds"
```

Expect `swap_final_denom` = `$SHROOM_BANK`, and the wallet's **bank** SHROOM (not
CW20) up by `final_received`. A mismatched `receive_as`, for example
`{"native_token":{"denom":"inj"}}`, must fail the simulate with "is not the asset
this route pays out".

### V2. White Whale: the gate quotes it

```bash
WW='[{"splits":[{"percent":100,"path":[{"amm_swap":{"pool_address":"inj1gk4thnx4t9y4ltnau60mpst5xyu0u57hqfl0qs","offer_asset_info":{"native_token":{"denom":"inj"}}}}]}]}]'
q "$AGG" '{"simulate_route":{"stages":'$WW',"amount_in":{"denom":"inj","amount":"10000000000000000"}}}'
#   2.2.0: "missing field `commission_amount`" — 2.2.1: an output_amount Y
snap > /tmp/agg_before.txt
exec_route '{"execute_route":{"stages":'$WW',"minimum_receive":"<99% of Y>"}}' 10000000000000000inj
snap > /tmp/agg_after.txt && diff /tmp/agg_before.txt /tmp/agg_after.txt && echo "residue invariant holds"
```

`final_received` should equal `Y` unless the pool traded in between.

### V3. Form change between hops: CW20 SHROOM leg into the Helix book

The Choice pair pays CW20 SHROOM, and the Helix SHROOM/INJ market
`0xc6b6d6627aeed8b9c29810163bed47d25c695d51a2aa8599fc5e39b2d88ef934` takes the bank
form. Execution converts between the two hops. This is a closed INJ → INJ cycle, so
it loses the two venues' fees: keep it small.

```bash
FC='[{"splits":[{"percent":100,"path":[{"amm_swap":{"pool_address":"inj1uyjjnykz0slq0w4n6k2xgleykqk9k5qkfctmw5","offer_asset_info":{"native_token":{"denom":"inj"}},"ask_asset_info":{"token":{"contract_addr":"'$SHROOM'"}}}},{"orderbook_swap":{"market_id":"0xc6b6d6627aeed8b9c29810163bed47d25c695d51a2aa8599fc5e39b2d88ef934","target_denom":"inj"}}]}]}]'
q "$AGG" '{"simulate_route":{"stages":'$FC',"amount_in":{"denom":"inj","amount":"50000000000000000"}}}'
#   2.2.0: "Orderbook simulation only supports native token inputs" — 2.2.1: an output_amount Z
snap > /tmp/agg_before.txt
exec_route '{"execute_route":{"stages":'$FC',"minimum_receive":"<97% of Z>"}}' 50000000000000000inj
snap > /tmp/agg_after.txt && diff /tmp/agg_before.txt /tmp/agg_after.txt && echo "residue invariant holds"
```

### V4. A route without the field still works

Run any route the instance served before, for example V1's `STAGES` with no
`receive_as`. It must pay **CW20** SHROOM, as 2.2.0 does.

## Rollback

- **A:** nothing to roll back. Point the router and frontend back at `inj1520rss…`.
  The new instance holds no user state.
- **B and C:** migrating back onto code 2067 is **refused**. The aggregator's
  `migrate` calls `cw2::ensure_from_older_version`, and 2067 would see a stored
  `2.2.1` above its own `2.2.0`. A rollback needs a new build of the old behaviour
  with a higher version (`2.2.2`), stored and then proposed like any upgrade. For B
  that means another 48h. This is the strongest reason to do A first: it can be
  abandoned instantly, and B can wait until A has run in production.
