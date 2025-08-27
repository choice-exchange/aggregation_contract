use crate::msg::Route;
use cosmwasm_std::{Addr, Uint128};
use cw_storage_plus::{Item, Map};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use cosmwasm_schema::cw_serde;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Config {
    pub admin: Addr,
}

// Stores the contract's configuration
pub const CONFIG: Item<Config> = Item::new("config");

// Stores the state of an in-progress execution, keyed by the user's address.
// This is crucial for the reply mechanism to continue a multi-step route.
pub const EXECUTION_STATE: Map<&Addr, Route> = Map::new("execution_state");


#[cw_serde]
pub struct ReplyState {
    // --- Unchanged Fields ---
    pub sender: Addr,
    pub minimum_receive: Uint128,

    // --- NEW & ENHANCED FIELDS ---

    /// The entire multi-stage plan for this execution.
    pub stages: Vec<crate::msg::Stage>,

    /// The index of the stage we are currently executing.
    pub current_stage_index: u64,

    /// The number of submessages we are waiting for from the CURRENT stage.
    pub replies_expected_for_current_stage: u64,

    /// The total output from the JUST-COMPLETED stage. This becomes the input for the next stage.
    pub accumulated_amount_for_current_stage: Uint128,
}

// A map from a unique reply ID to its state
pub const REPLY_STATES: Map<u64, ReplyState> = Map::new("reply_states");

// A counter to generate unique IDs for each batch of swaps
pub const REPLY_ID_COUNTER: Item<u64> = Item::new("reply_id_counter");