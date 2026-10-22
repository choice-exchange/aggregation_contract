# Aggregator v2.0.1 migration — runbook

**Status: ✅ COMPLETE 2026-07-21.** Migrated + dust swept. Kept for the record
and for the next follow-up (step 4b, router re-rank — still open).

Completed:
- **Apply** tx `155327566D4B9F252A87854E66649136AD3C9195693C5C9BEA822E59419EAD59`
  (h175149051, code 0). Live aggregator now code_id **2060**, cw2
  `dex-aggregator 2.0.1`.
- **Dust sweep** (`inj_scripts/aggregation/sweep_to_treasury.js`, 5 txs
  `08928B74…`, `D458EF90…`, `CECEDD4F…`, `1938136D…`, `4CB33381…`). Aggregator
  balance now **0 denoms**; treasury `inj1c2yle…76zv4` funded. v1 had nothing.

Everything below is the original plan, left intact for reference.

---

## Why

The live aggregator (v2.0, code 2042) tracks route amounts *virtually* and
`finalize_route` only sends the tracked output. Anything the chain hands back —
orderbook price-improvement refunds, unfilled remainders, dropped intermediates,
un-spent split inputs, CLMM dead-pool skips — lingers in the contract.

v2.0.1 (`9746a8a`) adds `build_residue_sweep`: every touched denom's
`current − entry_baseline` goes to the fee collector (the `pending_fees` portion,
i.e. orderbook buy surplus) or back to the user (everything else). Plus
`minimum_receive == 0` is now rejected, and a CW20 `Receive` whose hook fails to
deserialize reverts instead of silently keeping the tokens.

**Routing consequence.** Because the current contract strands the unspent quote,
the router is *correct* to rank orderbook routes below AMMs on small trades — the
0.001 INJ `min_quantity_tick_size` floor discards up to ~$0.005, which is ~0.18% of
a $2 trade but ~0.003% of a $30 trade. That fixed cost is exactly why
USDC→INJ flips venue with size. See the follow-up in step 4.

## Already done and verified

| Item | Value |
|---|---|
| Source commit | `9746a8a` (HEAD, tree clean) |
| Build | `cosmwasm/workspace-optimizer:0.17.0`, reproduced byte-identical |
| wasm sha256 | `84987f4d403442a334ab1718612805fe5e877d21b65c56e38e6a912e53e38e2a` |
| Tests | 45 passed / 0 failed, incl. all three fund-safety regressions |
| Stored as | **code_id 2060** — on-chain `data_hash` matches the sha256 above |
| Store tx | `8DFED2253D5634895D8970CC9D85C1BC9952987332AE053DA361A97F1B6FB956` |
| Propose tx | `77C33CD66DF2489AEBA2E73A5F6C03F80E5C088AEB01010316346CADB2CB658D` (h174867244) |

Addresses:

- aggregator (target) `inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0` — cw2 currently `2.0.0`
- timelock (wasm admin) `inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9`, delay 172800s
- timelock owner — keyring key named **`testnet`** = `inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz`
- `config.admin` (for `emergency_withdraw`) — key `choicedev` = `inj1yrg4pg8hcu0sw5rjlrcqfmw2ewf2uztlmdysak`

---

## 1. Confirm the queue is still what we expect

Run this FIRST. A later `Propose` from anyone with the owner key overwrites the
pending one and resets the timer.

```bash
TL=inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9
B=$(printf '{"pending_migration":{}}' | base64 | tr -d '\n')
curl -s "https://sentry.lcd.injective.network/cosmwasm/wasm/v1/contract/$TL/smart/$B"
```

Expect exactly:

```json
{"contract":"inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0",
 "code_id":2060,"msg":"e30=","effective_at":1784633767}
```

`e30=` is base64 `{}` (the aggregator's empty `MigrateMsg`). If `code_id` is not
2060, **stop** — someone re-proposed.

## 2. Apply

`Apply {}` is **permissionless** — any wallet can settle it once the delay elapses.
Use any funded key; `choicedev` is fine.

```bash
export PASSWORD='<keyring passphrase>'
yes "$PASSWORD" | injectived tx wasm execute \
  inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9 \
  '{"apply":{}}' \
  --from choicedev \
  --chain-id injective-1 \
  --node https://sentry.tm.injective.network:443 \
  --gas 2000000 --fees 1000000000000000inj --yes
```

Then confirm the tx actually succeeded — **check `code`, not just that a txhash
came back**. The upload script's failure mode taught us this the hard way:

```bash
curl -s "https://sentry.lcd.injective.network/cosmos/tx/v1beta1/txs/<TXHASH>" \
  | python3 -c "import sys,json;r=json.load(sys.stdin)['tx_response'];print(r['code'],r.get('raw_log','')[:300])"
```

## 3. Verify the migration landed

```bash
# cw2 version must now read 2.0.1
K=$(printf 'contract_info' | base64 | tr -d '\n')
curl -s "https://sentry.lcd.injective.network/cosmwasm/wasm/v1/contract/inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0/raw/$K" \
  | python3 -c "import sys,json,base64;print(base64.b64decode(json.load(sys.stdin)['data']).decode())"

# code_id must now be 2060
curl -s "https://sentry.lcd.injective.network/cosmwasm/wasm/v1/contract/inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0" \
  | python3 -c "import sys,json;print(json.load(sys.stdin)['contract_info']['code_id'])"
```

Then run one small real swap and confirm the contract's balance of the input denom
does **not** grow (that's the whole point of the sweep).

## 4. Follow-ups, in order

**a. Sweep the pre-existing dust.** The migration does NOT retroactively return
residue already stranded — it only affects routes executed after it lands. As of
2026-07-19 that is 26 denoms including ~12.13 INJ, 9.40 USDC, 2.59 USDT.
**Re-query before sweeping** — the list grows with every route until the migration
lands.

`emergency_withdraw` is admin-only, takes one asset per call, and pays out to
`info.sender`, so it must be signed by `choicedev`.

```bash
export PASSWORD='<keyring passphrase>'
AGG=inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0

curl -s "https://sentry.lcd.injective.network/cosmos/bank/v1beta1/balances/$AGG?pagination.limit=500" \
  | python3 -c "import sys,json;[print(b['denom']) for b in json.load(sys.stdin)['balances']]" \
  > /tmp/agg_dust_denoms.txt

while read -r DENOM; do
  echo ">>> $DENOM"
  yes "$PASSWORD" | injectived tx wasm execute "$AGG" \
    "{\"emergency_withdraw\":{\"asset_info\":{\"native_token\":{\"denom\":\"$DENOM\"}}}}" \
    --from choicedev --chain-id injective-1 \
    --node https://sentry.tm.injective.network:443 \
    --gas 800000 --fees 400000000000000inj --yes
  sleep 6
done < /tmp/agg_dust_denoms.txt
```

All 26 current denoms are bank denoms, so `native_token` is right for every one.
A CW20 would need `{"token":{"contract_addr":"inj1..."}}` instead.

**b. Re-rank orderbook routes in the router.** Only after (2) is live. Today
`_ob_buy_output` in `choice_exchange_backend/choice_django/liquidity/modules/routing_v3/pool_state.py`
scores the OB hop as base-out for the *full* input. Once the refund reaches the
user, the route really only consumes `qty * worst_price * (1 + taker*2)`, so
scoring against full input under-ranks orderbook legs — the mirror image of
today's bug. Rank on spent, not input. This is what finally makes 2 USDC → INJ
choose the orderbook.

Do NOT ship (b) before (2) lands, or the router goes back to over-quoting.

## Aborting

`Cancel {}` is **owner-only** (key `testnet`) and works any time before someone
applies. After the window opens, anyone can settle it — cancelling is a race at
that point, so decide before 2026-07-21 11:36 UTC.

```bash
yes "$PASSWORD" | injectived tx wasm execute \
  inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9 '{"cancel":{}}' \
  --from testnet --chain-id injective-1 \
  --node https://sentry.tm.injective.network:443 \
  --gas 500000 --fees 250000000000000inj --yes
```

## Gotchas hit while doing this

- **`upload_code_mainnet.sh` defaults `GAS=3800000` — too low.** The v2.0.1 wasm
  needs ~5.83M; the first store OOG'd and burned the fee. Use
  `GAS=9000000 FEES=4000000000000000inj`. The script greps tx logs for `code_id`
  and never checks the tx `code`, so an OOG surfaces as the misleading
  `❌ Could not find Code ID`. Always query the tx.
- **v1 (`inj1a4qvqym6ajewepa7v8y2rtxuz9f92kyq2zsg26`) is deliberately NOT being
  migrated,** despite the top-level CLAUDE.md saying "both instances". It holds
  zero balance, its wasm admin is the plain `choicedev` key (no timelock), and
  it's code 1892 — pre-CLMM-merge. Putting 2.0.1 code on it is a behaviour change,
  not a patch. It survives only as a volume-attribution entry in the backend's
  `AGGREGATOR_CONTRACTS`.

## Security — unrelated to the migration, but found doing it

The keyring key named **`testnet`** is the mainnet Choice Admin Timelock owner —
the only signer that can propose a migration of the live aggregator. Its
file-backend passphrase is `12345678`, which is in **public git history** (the
deploy scripts were hardened to read `PASSWORD` from env in June 2026, but the
value was already committed).

A mainnet-privileged key, guarded by a publicly-known passphrase, behind a name
that reads as disposable. Rotate this key's passphrase first, and rename the key.

## Separately noticed

`AGGREGATOR_CONTRACTS` in
`choice_exchange_backend/choice_django/choice_exchange/constants.py` defaults to
**only v1**. It drives orderbook-fill volume attribution in `window_metrics.py`
and `track_metrics.py`. If the server `.env` doesn't override it to include v2
(`inj1520rss…u3eu0`), Choice has been under-reporting its own orderbook volume for
v2's entire life. Check with `grep AGGREGATOR_CONTRACTS .env` on the box.
