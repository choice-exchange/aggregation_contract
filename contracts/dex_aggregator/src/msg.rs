use crate::cw20::Cw20ReceiveMsg;
#[allow(unused_imports)]
use crate::state::Config;
use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, Binary, Coin, Decimal, Uint128};
use injective_cosmwasm::MarketId;
use injective_math::FPDecimal;

pub mod cw20_adapter {
    use super::*;
    use cosmwasm_std::Binary;

    #[cw_serde]
    pub struct InstantiateMsg {}

    #[cw_serde]
    pub struct ReceiveSubmsg {
        pub(crate) recipient: String,
    }

    #[cw_serde]
    pub enum ExecuteMsg {
        RegisterCw20Contract {
            addr: Addr,
        },
        Receive {
            sender: String,
            amount: Uint128,
            msg: Binary,
        },
        RedeemAndTransfer {
            recipient: Option<String>,
        },
        RedeemAndSend {
            recipient: String,
            submsg: Binary,
        },
        UpdateMetadata {
            addr: Addr,
        },
    }

    #[cw_serde]
    pub enum QueryMsg {
        RegisteredContracts {},
        NewDenomFee {},
    }
}

pub mod amm {
    use super::*;

    #[cw_serde]
    pub enum AssetInfo {
        Token { contract_addr: String },
        NativeToken { denom: String },
    }

    #[cw_serde]
    pub struct Asset {
        pub info: AssetInfo,
        pub amount: Uint128,
    }

    #[cw_serde]
    pub enum QueryMsg {
        Simulation {
            offer_asset: Asset,
        },
        /// Pool pair info. Used by `SimulateRoute` to derive a hop's output asset
        /// (the pair side that isn't the offer) without an explicit `ask_asset_info`
        /// on the op. We only model `asset_infos`; serde drops the pair's other
        /// fields (`contract_addr`, `liquidity_token`, decimals, ...) on decode.
        Pair {},
    }

    /// Partial view of the pair's `PairInfo` — only the two asset infos.
    #[cw_serde]
    pub struct PairInfo {
        pub asset_infos: [AssetInfo; 2],
    }

    #[cw_serde]
    #[derive(Default)]
    pub struct SimulationResponse {
        pub return_amount: Uint128,
        pub spread_amount: Uint128,
        pub commission_amount: Uint128,
    }

    #[cw_serde]
    pub enum AmmPairExecuteMsg {
        Swap {
            offer_asset: amm::Asset,
            belief_price: Option<Decimal>,
            max_spread: Option<Decimal>,
            to: Option<String>,
        },
    }
}

pub mod reflection {
    use super::*;
    use cosmwasm_std::Binary;

    #[cw_serde]
    pub enum ExecuteMsg {
        TaxExemptTransfer {
            recipient: String,
            amount: Uint128,
        },
        TaxExemptSend {
            contract: String,
            amount: Uint128,
            msg: Binary,
        },
    }
}

pub mod clmm {
    use super::*;

    #[cw_serde]
    pub enum ClmmPoolExecuteMsg {
        SwapExactInput {
            minimum_amount_out: Uint128,
            recipient: Option<String>,
            deadline: Option<u64>,
        },
    }

    /// Flash-loan entry point on the CLMM pool. Wire-compatible with
    /// `choice_clmm_common::pool::ExecuteMsg::Flash` (variant tag `flash`). The
    /// pool lends `amount0`/`amount1` of token0/token1 to `recipient` and then
    /// calls `recipient` back with `FlashCallbackMsg::FlashCallback`. `data` is
    /// echoed into that callback unchanged.
    #[cw_serde]
    pub enum ClmmPoolFlashMsg {
        Flash {
            recipient: String,
            amount0: Uint128,
            amount1: Uint128,
            data: Binary,
        },
    }

    #[cw_serde]
    pub enum Cw20HookMsg {
        SwapExactInput {
            minimum_amount_out: Uint128,
            recipient: Option<String>,
            deadline: Option<u64>,
        },
    }

    #[cw_serde]
    pub enum ClmmPoolQueryMsg {
        Quote {
            token_in: amm::AssetInfo,
            amount_in: Uint128,
        },
        /// Pool config. Used by `SimulateRoute` to derive a hop's output asset
        /// (the pool token that isn't the offer). The pool's `AssetInfo` is
        /// wire-compatible with [`amm::AssetInfo`] (same `native_token`/`token`
        /// snake_case tags); serde drops the unmodeled `tick_spacing`/`fee_config`/
        /// `hook`/... fields on decode.
        GetConfig {},
    }

    #[cw_serde]
    pub struct QuoteResponse {
        pub amount_out: Uint128,
        pub amount_in_consumed: Uint128,
        pub fee_amount: Uint128,
    }

    /// Partial view of the pool's `PoolConfig` — only the two token infos.
    #[cw_serde]
    pub struct ConfigResponse {
        pub token0: amm::AssetInfo,
        pub token1: amm::AssetInfo,
    }
}

/// A single legacy-XYK AMM hop. `offer_asset_info` drives the dispatch (native
/// funds vs `Cw20::Send` vs tax-exempt send) and per-stage allocation.
#[cw_serde]
pub struct AmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
    /// The asset this hop produces. Resolved BEFORE the hop runs, because the
    /// engine snapshots its balance to bound what the route may spend and pay out
    /// (see `snapshot_entry_balances`). `None` => derived from the pair's
    /// `Pair {}` query (the side that isn't the offer), costing one extra query
    /// per hop; supply it to skip that.
    ///
    /// It is deliberately NOT read back from the swap event any more. Trusting the
    /// reply's `ask_asset` let a caller-authored "pool" name any asset it liked and
    /// have the aggregator pay it out of balances the route never brought in.
    #[serde(default)]
    pub ask_asset_info: Option<amm::AssetInfo>,
    /// `max_spread` passed to the pair's `Swap`. **Never sent as `None`**: Choice's
    /// `assert_max_spread` no-ops when both it and `belief_price` are `None`, but
    /// Astroport substitutes its OWN 0.5% default and asserts on it — and
    /// `SimulateRoute`'s `Simulation` query never applies that assert, so the
    /// pre-fire gate cannot see the revert coming.
    ///
    /// `None` here => 49%, i.e. effectively unbounded, leaving the route's
    /// mandatory `minimum_receive` / `min_profit` as the real slippage guard. 49
    /// rather than the 50 Astroport permits, so a pair bounding with `>=` instead
    /// of `>` cannot reject every swap. Clamped to 50% for the same reason.
    #[serde(default)]
    pub max_spread: Option<Decimal>,
}

/// A single Injective spot-market hop, executed natively by the aggregator (it
/// places the atomic spot order itself — there is no external swap contract).
///
/// `market_id` + `target_denom` are sufficient: the offer denom is the market's
/// *other* side, `is_buy = (target_denom == market.base_denom)`, and the ticks
/// come from the market — all derived on-chain.
///
/// - **Estimation mode** (`quantity`/`worst_price` omitted): the contract walks
///   the book to size the order. Used by the Choice dApp (backs `SimulateRoute`).
/// - **Direct mode** (both supplied): the caller fixes the base `quantity` and the
///   `worst_price` bound, so no orderbook-walk queries run. Used by the arb bot;
///   the route-level `minimum_receive` is the only net.
#[cw_serde]
pub struct OrderbookSwapOp {
    pub market_id: MarketId,
    /// Native denom this hop must produce (the market's base for a buy, quote for a sell).
    pub target_denom: String,
    /// Direct mode: base quantity to trade. `None` => estimate from the book.
    #[serde(default)]
    pub quantity: Option<FPDecimal>,
    /// Direct mode: worst acceptable price bound. `None` => estimate from the book.
    #[serde(default)]
    pub worst_price: Option<FPDecimal>,
}

/// A single CLMM hop.
///
/// - **Estimation mode** (`minimum_amount_out` omitted): the contract runs a
///   per-hop `Quote` query and applies 0.5% slippage. Used by the Choice dApp.
/// - **Direct mode** (`minimum_amount_out` supplied): the caller fixes the swap
///   floor, so the per-hop `Quote` re-simulation is skipped entirely. Used by the
///   arb bot; the route-level `minimum_receive` is the real net guard. An
///   unfillable hop reverts the atomic route (it does not zero out gracefully).
#[cw_serde]
pub struct ClmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
    /// The asset this hop produces — see [`AmmSwapOp::ask_asset_info`]. `None` =>
    /// derived from the pool's `GetConfig {}` (the token that isn't the offer).
    #[serde(default)]
    pub ask_asset_info: Option<amm::AssetInfo>,
    /// Direct mode: `SwapExactInput`'s `minimum_amount_out`, passed straight
    /// through. `None` => estimate it from the pool (`Quote` less `slippage_bps`).
    #[serde(default)]
    pub minimum_amount_out: Option<Uint128>,
    /// Estimation-mode slippage tolerance in basis points. `None` => 50 (0.5%),
    /// the value that used to be hardcoded. Ignored when `minimum_amount_out` is
    /// supplied. Clamped to 10000; at 10000 the floor is effectively disabled and
    /// the route's own `minimum_receive` / `min_profit` is the only guard.
    ///
    /// Worth raising when a stage splits across the SAME pool: every split's quote
    /// is taken before any of them execute, so later splits are priced against
    /// pre-trade state and this cushion is what absorbs the difference.
    #[serde(default)]
    pub slippage_bps: Option<u16>,
}

#[cw_serde]
pub enum Operation {
    AmmSwap(AmmSwapOp),
    OrderbookSwap(OrderbookSwapOp),
    ClmmSwap(ClmmSwapOp),
}

#[cw_serde]
pub struct Split {
    pub path: Vec<Operation>,
    pub percent: u8,
}

/// One parallel step of a route. Every split in a stage is dispatched together and
/// its outputs are pooled before the next stage runs.
///
/// ⚠️ **Splits within a stage are quoted against the SAME pre-stage state.** All of
/// a stage's messages are built in `execute_planned_swaps` before any of them
/// execute, so if two splits hit the same venue the second one is priced as though
/// the first had not traded — and `simulate_route` has the identical blind spot,
/// being a single-snapshot query. The result is an over-quote that the pre-fire
/// gate cannot see.
///
/// It is not rejected, because same-venue splits are a supported shape (see
/// `test_multi_split_to_same_orderbook_contract`). But routers should merge two
/// splits that share a pool into one, and where a shared pool is unavoidable on a
/// CLMM hop, widen `ClmmSwapOp::slippage_bps` to cover the self-impact. Sequencing
/// the affected hops into separate stages removes the problem entirely.
#[cw_serde]
pub struct Stage {
    pub splits: Vec<Split>,
}

#[cw_serde]
pub struct PlannedSwap {
    pub operation: Operation,
    pub amount: Uint128,
    pub split_index: usize,
    pub op_index: usize,
    /// The hop's offer (input) asset, resolved once when the stage is planned.
    /// Carried so `execute_planned_swaps` doesn't re-derive it — for an orderbook
    /// op that re-derivation is a spot-market chain query (`load_market`).
    pub offer_info: amm::AssetInfo,
}

pub struct StagePlan {
    pub swaps_to_execute: Vec<PlannedSwap>,
    pub conversions_needed: Vec<(amm::Asset, amm::AssetInfo)>,
}

#[cw_serde]
pub enum Cw20HookMsg {
    ExecuteRoute {
        stages: Vec<Stage>,
        minimum_receive: Option<Uint128>,
    },
}

#[cw_serde]
pub struct InstantiateMsg {
    pub admin: String,
    pub cw20_adapter_address: String,
    pub fee_collector_address: String,
}

/// No-op migrate payload. The route engine's state (`ACTIVE_ROUTES`,
/// `SUBMSG_REPLY_STATES`, ...) is transient within a single atomic tx, and the
/// persistent stores (`CONFIG`, `FEE_MAP`, `FLASH_SIGNERS`, `TAX_TOKEN_REGISTRY`)
/// are structurally unchanged, so no data migration is required — the `migrate`
/// entry point only guards the contract identity and bumps the stored version.
#[cw_serde]
pub struct MigrateMsg {}

#[cw_serde]
pub enum ExecuteMsg {
    ExecuteRoute {
        stages: Vec<Stage>,
        minimum_receive: Option<Uint128>,
    },
    Receive(Cw20ReceiveMsg),
    // Admin-only
    UpdateAdmin {
        new_admin: String,
    },
    SetFee {
        pool_address: String,
        /// Aggregator fee as a decimal FRACTION of the hop's output (e.g.
        /// "0.003" = 0.3%), NOT a percent. Renamed from the misleading
        /// `fee_percent`. Must be < 1.
        fee_fraction: Decimal,
    },
    RemoveFee {
        pool_address: String,
    },
    UpdateFeeCollector {
        new_fee_collector: String,
    },
    /// Admin-only. Repoints the CW20<->native adapter used by every conversion
    /// (`create_conversion_msg`). Without this the address is fixed at instantiate
    /// and a redeployed adapter could only be picked up by a contract migration.
    UpdateCw20Adapter {
        new_cw20_adapter: String,
    },
    EmergencyWithdraw {
        asset_info: amm::AssetInfo,
    },
    /// Registers a new tax token that requires special handling.
    RegisterTaxToken {
        contract_addr: String,
    },
    /// Removes a tax token from the registry.
    DeregisterTaxToken {
        contract_addr: String,
    },
    /// Adds `signer` to the `FlashRoute` allowlist. Admin-only.
    AuthorizeFlashSigner {
        signer: String,
    },
    /// Removes `signer` from the `FlashRoute` allowlist. Admin-only.
    RevokeFlashSigner {
        signer: String,
    },
    /// Escape hatch: when `open` is `true`, `FlashRoute` is permissionless (the
    /// signer allowlist is bypassed). Admin-only.
    SetFlashUnrestricted {
        open: bool,
    },
    /// Capital-free CLMM flash-arb. Borrows `flash_amount` of `flash_asset` from
    /// `flash_pool`, runs the `stages` cycle (must end in `flash_asset` and must
    /// not route through `flash_pool`), repays principal + flash fee, and forwards
    /// the surplus to the caller. The cycle reverts atomically unless the surplus
    /// covers `min_profit`.
    FlashRoute {
        flash_pool: String,
        flash_asset: amm::AssetInfo,
        flash_amount: Uint128,
        stages: Vec<Stage>,
        min_profit: Uint128,
    },
    /// Borrower callback invoked by the CLMM pool mid-flash. Field layout matches
    /// `choice_clmm_common::pool::FlashCallbackMsg::FlashCallback` so the pool's
    /// serialized callback decodes straight into this variant. Only valid while a
    /// `FlashRoute`-initiated flash is in flight (gated by `PENDING_FLASH`).
    FlashCallback {
        fee0: Uint128,
        fee1: Uint128,
        data: Binary,
    },
}

#[cw_serde]
pub struct FeeInfo {
    pub pool_address: String,
    /// Decimal fraction of output (e.g. "0.003" = 0.3%), not a percent.
    pub fee_fraction: Decimal,
}

#[cw_serde]
pub struct FeeResponse {
    pub fee: Option<Decimal>,
}

#[cw_serde]
pub struct AllFeesResponse {
    pub fees: Vec<FeeInfo>,
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(SimulateRouteResponse)]
    SimulateRoute { stages: Vec<Stage>, amount_in: Coin },
    #[returns(Config)]
    Config {},
    #[returns(FeeResponse)]
    FeeForPool { pool_address: String },
    #[returns(AllFeesResponse)]
    AllFees {
        start_after: Option<String>,
        limit: Option<u32>,
    },
    /// Whether `signer` may call `FlashRoute` (true if explicitly allowlisted or
    /// if flash is unrestricted).
    #[returns(IsFlashSignerResponse)]
    IsFlashSigner { signer: String },
    /// All allowlisted flash signers, plus the unrestricted flag.
    #[returns(FlashSignersResponse)]
    FlashSigners {
        start_after: Option<String>,
        limit: Option<u32>,
    },
}

#[cw_serde]
pub struct IsFlashSignerResponse {
    pub authorized: bool,
}

#[cw_serde]
pub struct FlashSignersResponse {
    pub signers: Vec<String>,
    pub unrestricted: bool,
}

#[cw_serde]
pub struct SimulateRouteResponse {
    pub output_amount: Uint128,
}
