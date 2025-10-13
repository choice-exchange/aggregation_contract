use crate::msg::{amm, Operation, PlannedSwap, Stage};
use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Decimal, Uint128};
use cw_storage_plus::{Item, Map};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Config {
    pub admin: Addr,
    pub cw20_adapter_address: Addr,
    pub fee_collector: Addr,
}

#[cw_serde]
pub enum Awaiting {
    Swaps,
    Conversions,
    FinalConversions,
    PathConversion,
}

pub const CONFIG: Item<Config> = Item::new("config");
pub const FEE_MAP: Map<&Addr, Decimal> = Map::new("fee_map");

#[cw_serde]
pub struct PendingPathOp {
    pub operation: Operation,
    pub amount: Uint128,
}

#[cw_serde]
pub struct RoutePlan {
    pub sender: Addr,
    pub minimum_receive: Uint128,
    pub stages: Vec<Stage>,
}

#[cw_serde]
pub struct ExecutionState {
    pub plan: RoutePlan,
    pub awaiting: Awaiting,
    pub current_stage_index: u64,
    pub replies_expected: u64,
    pub accumulated_assets: Vec<amm::Asset>,
    pub pending_swaps: Vec<PlannedSwap>,
    pub pending_path_op: Option<PendingPathOp>,
}

#[cw_serde]
pub struct SubmsgReplyState {
    pub master_reply_id: u64,
    pub split_index: usize,
    pub op_index: usize,
}

pub const ACTIVE_ROUTES: Map<u64, ExecutionState> = Map::new("execution_states");
pub const SUBMSG_REPLY_STATES: Map<u64, SubmsgReplyState> = Map::new("submsg_reply_states");
pub const REPLY_ID_COUNTER: Item<u64> = Item::new("reply_id_counter");
