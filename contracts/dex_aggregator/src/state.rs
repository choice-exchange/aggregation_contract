use crate::msg::{amm, Operation, PlannedSwap, Stage};
use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Decimal, StdError, Storage, Uint128};
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

/// `FEE_MAP` denominator: fees are stored as `Decimal` (18-dp), so the raw
/// `atomics()` are scaled by 1e18.
const DECIMAL_FRACTIONAL: u128 = 1_000_000_000_000_000_000;

/// The aggregator's per-pool fee for `pool_addr` applied to `amount`, returning
/// `(amount_after_fee, fee)`. SHARED by the executor (`reply.rs`, at each split
/// path's terminal hop) and the simulator (`query.rs`) so a `SimulateRoute`
/// quote can't silently diverge from the executed fill. Orderbook hops carry no
/// aggregator fee (the exchange takes its own trading fee) and must not call
/// this. A pool with no `FEE_MAP` entry yields a zero fee.
pub fn apply_fee(
    storage: &dyn Storage,
    pool_addr: &Addr,
    amount: Uint128,
) -> Result<(Uint128, Uint128), StdError> {
    let fee = match FEE_MAP.may_load(storage, pool_addr)? {
        Some(fee_percent) => amount.multiply_ratio(fee_percent.atomics(), DECIMAL_FRACTIONAL),
        None => Uint128::zero(),
    };
    let amount_after_fee = amount.checked_sub(fee)?;
    Ok((amount_after_fee, fee))
}

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
    /// The route's original input (offer) asset, preserved so the terminal
    /// `aggregator_swap` event can report what the user put in. For a flash cycle
    /// this is the borrowed principal.
    pub offer: amm::Asset,
    /// `Some` for flash-arb cycles, `None` for ordinary user swaps.
    pub flash_repayment: Option<FlashRepayment>,
}

/// One executed venue trade within a route, recorded for indexing. Serialized into
/// the `swap_results` attribute of the terminal `aggregator_swap` event so an
/// indexer can attribute per-venue volume without indexing each pool/market's own
/// native event. Conversions (CW20<->native adapter wraps) are NOT legs.
#[cw_serde]
pub struct SwapLeg {
    /// Venue kind: `"amm"`, `"clmm"`, or `"orderbook"`.
    pub kind: String,
    /// Pool contract address (AMM/CLMM) or spot market id (orderbook).
    pub venue: String,
    pub offer_denom: String,
    pub offer_amount: Uint128,
    pub ask_denom: String,
    /// Gross output the venue produced, before any aggregator fee.
    pub ask_amount: Uint128,
    /// Aggregator fee taken on this leg (in `ask_denom`); zero for hops that carry
    /// no aggregator fee (orderbook, and non-terminal hops).
    pub fee_amount: Uint128,
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
    /// Executed venue trades, accumulated across the whole route for the terminal
    /// `aggregator_swap` event.
    pub legs: Vec<SwapLeg>,
}

#[cw_serde]
pub struct SubmsgReplyState {
    pub master_reply_id: u64,
    pub split_index: usize,
    pub op_index: usize,
    /// The asset this hop was dispatched with — carried so its reply can record a
    /// complete [`SwapLeg`] (offer side) without re-deriving it.
    pub in_denom: String,
    pub in_amount: Uint128,
}

pub const ACTIVE_ROUTES: Map<u64, ExecutionState> = Map::new("execution_states");
pub const SUBMSG_REPLY_STATES: Map<u64, SubmsgReplyState> = Map::new("submsg_reply_states");
pub const REPLY_ID_COUNTER: Item<u64> = Item::new("reply_id_counter");

/// A registry of known tax tokens that require special handling.
/// The key is the token's contract address.
/// The value is a simple boolean `true` to indicate it's registered.
pub const TAX_TOKEN_REGISTRY: Map<&Addr, bool> = Map::new("tax_tokens");

/// Allowlist of signers (EOAs) permitted to call `FlashRoute`. The key is the
/// signer address; presence == authorized. Mutated by the admin via
/// `AuthorizeFlashSigner` / `RevokeFlashSigner` and checked in
/// `execute_flash_route`. An empty map means no one may flash-borrow through the
/// aggregator (deny-all) unless `FLASH_UNRESTRICTED` is set.
pub const FLASH_SIGNERS: Map<&Addr, ()> = Map::new("flash_signers");

/// Escape hatch: when `true`, the `FLASH_SIGNERS` gate in `execute_flash_route`
/// is skipped and `FlashRoute` is permissionless again. Admin-settable via
/// `SetFlashUnrestricted`. Absent == `false` (gate enforced).
pub const FLASH_UNRESTRICTED: Item<bool> = Item::new("flash_unrestricted");
