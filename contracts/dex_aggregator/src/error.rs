use cosmwasm_std::{StdError, Uint128};
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
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
}
