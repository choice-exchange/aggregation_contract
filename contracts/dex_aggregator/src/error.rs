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

    #[error("Invalid Cw20 Hook message")]
    InvalidCw20HookMsg {},

    #[error("Route cannot be empty")]
    EmptyRoute {},

    #[error("Invalid reply ID: {id}")]
    InvalidReplyId { id: u64 },

    #[error("Unrecognized reply ID")]
    UnrecognizedReplyId {},

    #[error("Execution state not found for sender")]
    ExecutionStateNotFound {},

    #[error("The provided funds do not match the first step of the route")]
    MismatchedInitialFunds {},

    #[error("Input amount must be greater than zero")]
    ZeroAmount {},

    #[error("Percentages in a stage must sum to 100")]
    InvalidPercentageSum {},

    #[error("No stages provided for the swap")]
    NoStages {},

    #[error("Failed to parse reply from submessage")]
    ReplyParseError {},

    #[error("AggregateSwaps requires exactly one type of coin to be sent")]
    InvalidFunds {},
}
