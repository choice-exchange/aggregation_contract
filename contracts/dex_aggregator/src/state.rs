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

/// Repayment obligation attached to a flash-arb route. Present only on routes
/// kicked off by `FlashRoute`; when set, the final stage repays `repay_amount`
/// (principal + flash fee) of `asset` to `pool` and forwards the surplus to the
/// route's `sender`, instead of sending the whole output to `sender`.
#[cw_serde]
pub struct FlashRepayment {
    pub pool: Addr,
    pub asset: amm::AssetInfo,
    pub repay_amount: Uint128,
    pub min_profit: Uint128,
}

#[cw_serde]
pub struct RoutePlan {
    pub sender: Addr,
    pub minimum_receive: Uint128,
    pub stages: Vec<Stage>,
    /// `Some` for flash-arb cycles, `None` for ordinary user swaps.
    pub flash_repayment: Option<FlashRepayment>,
}

/// Transient context for an in-flight flash, saved by `execute_flash_route` and
/// consumed by `execute_flash_callback`. Its presence is the authorization gate
/// for `FlashCallback` (a callback with no pending context is forged). Only one
/// flash is ever in flight, since the whole flow is a single atomic transaction.
#[cw_serde]
pub struct PendingFlashCtx {
    pub flash_pool: Addr,
    pub flash_asset: amm::AssetInfo,
    /// Whether `flash_asset` is the pool's token0 (selects `fee0` vs `fee1`).
    pub flash_is_token0: bool,
    pub principal: Uint128,
    pub stages: Vec<Stage>,
    pub min_profit: Uint128,
    pub initiator: Addr,
}

pub const PENDING_FLASH: Item<PendingFlashCtx> = Item::new("pending_flash");

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

/// A registry of known tax tokens that require special handling.
/// The key is the token's contract address.
/// The value is a simple boolean `true` to indicate it's registered.
pub const TAX_TOKEN_REGISTRY: Map<&Addr, bool> = Map::new("tax_tokens");
