use cosmwasm_std::Addr;
use cw_storage_plus::{Item, Map};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use crate::msg::Route;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Config {
    pub admin: Addr,
}

// Stores the contract's configuration
pub const CONFIG: Item<Config> = Item::new("config");

// Stores the state of an in-progress execution, keyed by the user's address.
// This is crucial for the reply mechanism to continue a multi-step route.
pub const EXECUTION_STATE: Map<&Addr, Route> = Map::new("execution_state");