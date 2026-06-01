//! Minimal vendored CW20 message/response types.
//!
//! The `cw20` crate (v2.0.0, latest published) is locked to cosmwasm-std 2 via
//! `cw_utils`, which conflicts with the cosmwasm-std 3 stack pulled in by
//! `injective-cosmwasm 0.3.6`. No cw20 release is cosmwasm-std-3 compatible yet,
//! so we vendor only the surface this contract uses. The JSON wire format here is
//! byte-identical to the upstream `cw20` v2 types, so on-chain CW20 contracts are
//! unaffected.

use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Binary, Uint128};

/// Hook payload delivered to a CW20 receiver via `Cw20ExecuteMsg::Send`.
/// Mirrors `cw20::Cw20ReceiveMsg`. Deserialized from incoming `Receive` execute.
#[cw_serde]
pub struct Cw20ReceiveMsg {
    pub sender: String,
    pub amount: Uint128,
    pub msg: Binary,
}

/// Subset of `cw20::Cw20ExecuteMsg` actually constructed by this contract.
/// Only outgoing (serialized) — never deserialized — so the omitted variants
/// are irrelevant. Variant names match upstream snake_case JSON tags.
#[cw_serde]
pub enum Cw20ExecuteMsg {
    /// Transfer tokens to a recipient address.
    Transfer { recipient: String, amount: Uint128 },
    /// Send tokens to a contract, triggering its `Receive` hook with `msg`.
    Send {
        contract: String,
        amount: Uint128,
        msg: Binary,
    },
}

/// Subset of `cw20::Cw20QueryMsg` used by this contract (balance lookups only).
#[cw_serde]
pub enum Cw20QueryMsg {
    /// Returns the current balance of `address` as a [`BalanceResponse`].
    Balance { address: String },
}

/// Response to `Cw20QueryMsg::Balance`. Mirrors `cw20::BalanceResponse`.
#[cw_serde]
pub struct BalanceResponse {
    pub balance: Uint128,
}
