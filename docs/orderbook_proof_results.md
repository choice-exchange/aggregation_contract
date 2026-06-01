# Orderbook-Merge Proof — Results

Test-tube proof for the merged-contract design, run on the **latest** injective
stack (injective-std 1.19 / injective-cosmwasm 0.3.6 / injective-math 0.3.6 →
cosmwasm-std 3.0, prost 0.13; test-tube 1.19 embeds the matching chain).

Harness: `choice/orderbook_merge_proof/` — a throwaway `probe` contract that fires
raw atomic spot market orders, plus `probe/tests/proof.rs` which builds a real
ATOM/USDT market (6/6 decimals), seeds a book, and observes behaviour. Run:

```
# wasm must be MVP (see "Build constraint" below)
cargo +nightly build -Z build-std=std,panic_abort -p probe \
  --target wasm32-unknown-unknown --release      # .cargo/config.toml sets target-cpu=mvp
cargo test -p probe --test proof -- --nocapture --test-threads=1
```

`test result: ok. 3 passed; 0 failed.`

## What was proven (all three green)

### Q1 — atomic spot market orders are immediate-or-cancel (partial fill, no revert)
Buying 1000 ATOM into a book holding only 600 **succeeded** and filled **550 ATOM**
(`received 550000000 base (IOC partial fill)`), reporting the actual filled
quantity in the typed reply. It did **not** revert.
→ Sizing an orderbook hop above available liquidity is safe; the contract gets the
real fill back and the route-level `minimum_receive` is the net. Confirms the
design's reliance on IOC semantics.

### Q2 — a loose `worst_price` is accepted, not band-rejected
Buying 50 ATOM at **5× the touch** succeeded and filled exactly 50 ATOM at the real
book price (recorded fill price `10…` = the seeded ask of 10), not at the 50× cap.
→ The "loose worst_price + tight end-of-route `minimum_receive`" pattern (§A/§B of
the optimization review, and direct mode §C) is valid. No price-band rejection for
an aggressive bound on this market. (Still clamp/limit on markets that define
bands; not exercised here.)

### Q3 / G6 — no-reply chaining works: order 2 spends order 1's proceeds in one tx
A probe with **0 ATOM** fired `[buy 50 ATOM, sell 50 ATOM]` as two plain messages
in a single Response with **no reply between them**. Result: **SUCCESS**, ending
with **0 ATOM** and USDT round-tripped (1,000,000 → 999,948.575 USDT, the spread +
fees). Because the probe started with zero ATOM, the sell could only have come from
the buy's proceeds — settled to the contract's bank balance synchronously within the
same tx.
→ **The "fire-and-check" gas optimization (G6) is viable.** For router-supplied
(direct-mode) arb, the merged contract can emit all orderbook orders in one execute
with no per-hop reply round-trips and check `minimum_receive` once at the end —
eliminating the per-reply state I/O and re-entry that dominate gas.

### Baseline — typed reply decode + reply-chaining works
`with_reply` round trip recorded two `MsgCreateSpotMarketOrderResponse` fills
(buy @10, sell @9), confirming the `v1beta1::MsgCreateSpotMarketOrderResponse`
decode path and `injective-cosmwasm 0.3.6` order bindings work end-to-end on the
v1.19 chain — the safe fallback design.

## Incidental confirmations
- **Scaling sanity:** for a 6/6-decimal market, chain price == human price (TOB
  returned `best_buy=9 best_sell=10`, exactly the seeded levels). The self-calibrated
  harness needed no scaling guesswork.
- Buy fee is taken in quote (base received == ordered quantity), so chaining a
  sell of the same base quantity is exact.

## Build/operational constraints surfaced (carry into the merge)
1. **cosmwasm-std 2 → 3** (latest injective stack): `StdError::generic_err` → `msg`,
   `abort` feature removed, `StdError` opaque. (See merge plan dep table.)
2. **v1.19 market launch prerequisites** (tests, and any deploy that launches/needs
   markets): the quote/base denoms must have **denom decimals** registered
   (`init_account_decimals` in tests) and a **denom min-notional** registered via a
   governance `BatchExchangeModificationProposal` — a market won't launch otherwise
   (`min notional for usdt does not exist`).
3. **Reference-types wasm feature.** Rust ≥ 1.82 emits the `reference-types` wasm
   feature (in codegen *and* in the precompiled std), which the chain's wasm VM
   rejects (`reference-types not enabled`). `-C target-feature=-reference-types` and
   `-C target-cpu=mvp` on the contract crate are **not** sufficient (std is
   precompiled). The working recipe in this repo:
   - `.cargo/config.toml`: `[target.wasm32-unknown-unknown] rustflags=["-C","target-cpu=mvp"]`, and
   - build with `cargo +nightly build -Z build-std=std,panic_abort` so std is
     recompiled under MVP.
   The production alternative is the cosmwasm optimizer image (pins a compatible
   toolchain). **The merged contract's `build_release.sh` must account for this** —
   verify the optimizer image's Rust version produces MVP wasm, or apply the
   build-std recipe.
   (Pinning an older toolchain — 1.81, last pre-reference-types — is blocked: the
   latest dep graph pulls edition2024 crates that need cargo ≥ 1.85.)

## Bottom line for the design
The two assumptions the design rested on are confirmed on the latest chain: atomic
orders are IOC and accept a loose price bound, so `minimum_receive` is a sufficient
net; and **G6 (no-reply fire-and-check) works**, unlocking the biggest gas win for
direct-mode arb. Proceed with the merge plan; make direct mode no-reply.
