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
/// funds vs `Cw20::Send` vs tax-exempt send) and per-stage allocation. The output
/// (ask) asset is *not* carried: during execution it's read from the pair's swap
/// event (`ask_asset` attribute), and during `SimulateRoute` it's derived from the
/// pair's `Pair {}` query (the pair side that isn't the offer).
#[cw_serde]
pub struct AmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
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

/// A single CLMM hop. As with [`AmmSwapOp`], only `offer_asset_info` is carried:
/// the output (ask) asset is read from the pool's swap event (`ask_asset`
/// attribute) during execution, and from the pool's `GetConfig {}` query (the
/// pool token that isn't the offer) during `SimulateRoute`.
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
    /// Direct mode: `SwapExactInput`'s `minimum_amount_out`, passed straight
    /// through. `None` => estimate it from the pool (`Quote` + 0.5%).
    #[serde(default)]
    pub minimum_amount_out: Option<Uint128>,
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
        fee_percent: Decimal,
    },
    RemoveFee {
        pool_address: String,
    },
    UpdateFeeCollector {
        new_fee_collector: String,
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
    pub fee_percent: Decimal,
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
}

#[cw_serde]
pub struct SimulateRouteResponse {
    pub output_amount: Uint128,
}
