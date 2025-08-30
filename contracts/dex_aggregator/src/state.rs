use crate::msg::{external, Route};
use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Uint128};
use cw_storage_plus::{Item, Map};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Config {
    pub admin: Addr,
    pub cw20_adapter_address: Addr,
}

#[cw_serde]
pub enum Awaiting {
    /// Waiting for replies from DEX swaps (AMMs or Orderbooks).
    Swaps,
    /// Waiting for replies from the CW20 Adapter after dispatching conversion messages.
    Conversions,
}

// Stores the contract's configuration
pub const CONFIG: Item<Config> = Item::new("config");

// Stores the state of an in-progress execution, keyed by the user's address.
// This is crucial for the reply mechanism to continue a multi-step route.
pub const EXECUTION_STATE: Map<&Addr, Route> = Map::new("execution_state");

#[cw_serde]
pub struct ReplyState {
    pub sender: Addr,
    /// The minimum final amount the user must receive.
    pub minimum_receive: Uint128,
    /// The entire, pre-defined plan of swaps.
    pub stages: Vec<crate::msg::Stage>,

    // --- State Machine Fields (Dynamic) ---
    /// What kind of reply are we currently waiting for?
    pub awaiting: Awaiting,
    /// The index of the swap stage we are currently processing.
    pub current_stage_index: u64,
    /// The number of submessage replies we are waiting for in the current state.
    pub replies_expected: u64,

    // --- Data Accumulators (Dynamic) ---
    /// Assets collected from a completed swap stage. This is a temporary holding area
    /// before the "Normalization Phase" unifies them.
    pub accumulated_assets: Vec<external::Asset>,
    /// The total balance that has been unified into the correct asset type and is
    /// ready to be used as input for the next swap stage.
    pub ready_for_next_stage_amount: Uint128,
}

// A map from a unique reply ID to its state
pub const REPLY_STATES: Map<u64, ReplyState> = Map::new("reply_states");

// A counter to generate unique IDs for each batch of swaps
pub const REPLY_ID_COUNTER: Item<u64> = Item::new("reply_id_counter");
