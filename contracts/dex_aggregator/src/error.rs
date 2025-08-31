use cosmwasm_std::StdError;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("Minimum receive amount not met")]
    MinimumReceiveNotMet {},

    #[error("Route cannot be empty")]
    EmptyRoute {},

    #[error("Execution state not found for sender")]
    ExecutionStateNotFound {},

    #[error("Input amount must be greater than zero")]
    ZeroAmount {},

    #[error("Percentages in a stage must sum to 100")]
    InvalidPercentageSum {},

    #[error("No stages provided for the swap")]
    NoStages {},

    #[error("Failed to parse submessage reply result: {error}")]
    SubmessageResultError { error: String },

    #[error("Failed to parse reply: wasm event did not contain a return amount attribute")]
    NoAmountInReply {},

    #[error("Failed to parse reply: amount attribute has a malformed value '{value}'")]
    MalformedAmountInReply { value: String },

    #[error("Failed to parse conversion reply: could not find a valid 'transfer' or 'wasm' event")]
    NoConversionEventInReply {},

    #[error("AggregateSwaps requires exactly one type of coin to be sent")]
    InvalidFunds {},
}
