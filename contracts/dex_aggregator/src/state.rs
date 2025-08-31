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
    Swaps,
    Conversions,
    FinalConversions,
}

pub const CONFIG: Item<Config> = Item::new("config");

pub const EXECUTION_STATE: Map<&Addr, Route> = Map::new("execution_state");

#[cw_serde]
pub struct ReplyState {
    pub sender: Addr,
    pub minimum_receive: Uint128,
    pub stages: Vec<crate::msg::Stage>,
    pub awaiting: Awaiting,
    pub current_stage_index: u64,
    pub replies_expected: u64,
    pub accumulated_assets: Vec<external::Asset>,
    pub ready_for_next_stage_amount: Uint128,
}

pub const REPLY_STATES: Map<u64, ReplyState> = Map::new("reply_states");

pub const REPLY_ID_COUNTER: Item<u64> = Item::new("reply_id_counter");
