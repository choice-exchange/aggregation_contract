use cosmwasm_std::StdError;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("Unauthorized")]
    Unauthorized {},

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
}