# Optimized builds
#
# Toolchain constraint (cosmwasm-std 3.0 migration): the optimizer image below
# pins Rust 1.86. The cosmwasm-std 3.0.7 stack pulls cw-schema -> serde_with ->
# darling, whose LATEST releases (serde_with 3.20 / darling 0.23) require rustc
# >=1.88 and will FAIL to build in this image. The committed Cargo.lock pins them
# down to serde_with 3.12.0 / darling 0.20.11 (still satisfy cw-schema's ">=3.9").
# => Keep Cargo.lock committed and DO NOT `cargo update` those crates upward until
#    the cosmwasm optimizer ships an image with rustc >=1.88. The produced wasm is
#    MVP-clean (verified: `wasm-tools validate --features=-reference-types`).
docker run --rm -v "$(pwd)":/code \
  --mount type=volume,source="$(basename "$(pwd)")_cache",target=/code/target \
  --mount type=volume,source=registry_cache,target=/usr/local/cargo/registry \
  cosmwasm/workspace-optimizer:0.17.0
