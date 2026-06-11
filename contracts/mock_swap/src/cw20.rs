//! Minimal vendored CW20 message types (mock_swap subset).
//!
//! See dex_aggregator's `cw20.rs` for why these are vendored rather than pulled
//! from the `cw20` crate (cosmwasm-std 2 vs 3 conflict). JSON wire format is
//! byte-identical to upstream `cw20` v2.

use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Binary, Uint128};

/// Hook payload delivered to a CW20 receiver. Mirrors `cw20::Cw20ReceiveMsg`.
#[cw_serde]
pub struct Cw20ReceiveMsg {
    pub sender: String,
    pub amount: Uint128,
    pub msg: Binary,
}

/// Subset of `cw20::Cw20ExecuteMsg` constructed by this mock (transfer only).
#[cw_serde]
pub enum Cw20ExecuteMsg {
    Transfer { recipient: String, amount: Uint128 },
}
