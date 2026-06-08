use cosmwasm_std::{StdError, Uint128};
use thiserror::Error;

// cosmwasm-std 3.0 made `StdError` opaque (no longer `PartialEq`), so the
// `Std(StdError)` variant can no longer derive `PartialEq`. Tests that compared
// errors by value must switch to `matches!`.
#[derive(Error, Debug)]
pub enum ContractError {
    // --- Standard & Authorization Errors ---
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("Unauthorized")]
    Unauthorized {},

    // --- Input & Route Validation Errors ---
    #[error("Input amount must be greater than zero")]
    ZeroAmount {},

    #[error("No stages provided for the swap")]
    NoStages {},

    #[error("A stage or path within the route cannot be empty")]
    EmptyRoute {},

    #[error("Percentages in a stage must sum to 100")]
    InvalidPercentageSum {},

    #[error("Invalid funds for native token swap. Expected 1 coin, sent {sent}")]
    InvalidFunds { sent: usize },

    // --- Execution & Economic Outcome Errors ---
    #[error(
        "Minimum receive amount not met. Minimum: {minimum_receive}, Received: {actual_receive}"
    )]
    MinimumReceiveNotMet {
        minimum_receive: Uint128,
        actual_receive: Uint128,
    },

    // --- Submessage & Reply Handling Errors ---
    #[error(
        "Submessage from contract {contract_addr} for operation at [split:{split_index}, op:{op_index}] failed with: {error}"
    )]
    SubmessageFailed {
        split_index: usize,
        op_index: usize,
        contract_addr: String,
        error: String,
    },

    #[error("Asset conversion failed during the '{awaiting_state}' step: {error}")]
    ConversionFailed {
        awaiting_state: String,
        error: String,
    },

    // --- Reply Parsing Errors ---
    #[error("Failed to parse reply: wasm event did not contain a return amount attribute")]
    NoAmountInReply {},

    #[error("Failed to parse reply: amount attribute has a malformed value '{value}'")]
    MalformedAmountInReply { value: String },

    #[error("Failed to parse conversion reply: could not find a valid 'transfer' or 'wasm' event")]
    NoConversionEventInReply {},

    #[error("Failed to parse swap reply: wasm event did not contain an 'ask_asset' attribute")]
    NoAskAssetInReply {},

    // --- Orderbook (native spot-order) Errors ---
    #[error("Orderbook order quantity rounds to zero (input below one tick)")]
    AmountTooSmall {},

    #[error("Failed to decode spot market order response: {err}")]
    OrderResponseDecode { err: String },

    #[error("Denom '{denom}' is not part of orderbook market {market_id}")]
    InvalidOrderbookDenom { denom: String, market_id: String },

    // --- Flash-arb (FlashRoute) Errors ---
    #[error("FlashCallback received with no flash in flight (forged or stray call)")]
    NoPendingFlash {},

    #[error("Flash-arb cycle may not route through the flash-source pool")]
    FlashPoolInCycle {},

    #[error("flash_asset is neither token0 nor token1 of the flash pool")]
    FlashAssetNotInPool {},

    #[error("Flash-arb profit floor not met. Required (principal+fee+min_profit): {required}, produced: {actual}")]
    FlashProfitNotMet {
        required: Uint128,
        actual: Uint128,
    },
}
