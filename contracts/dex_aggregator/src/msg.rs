#[allow(unused_imports)]
use crate::state::Config;
use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, Coin, Decimal, Uint128};
use crate::cw20::Cw20ReceiveMsg;

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
        Simulation { offer_asset: Asset },
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

pub mod orderbook {
    use super::*;
    use injective_math::FPDecimal;

    #[cw_serde]
    pub struct FPCoin {
        pub amount: FPDecimal,
        pub denom: String,
    }

    #[cw_serde]
    pub enum QueryMsg {
        GetOutputQuantity {
            from_quantity: FPDecimal,
            source_denom: String,
            target_denom: String,
        },
    }

    #[cw_serde]
    pub struct SwapEstimationResult {
        pub expected_fees: Vec<FPCoin>,
        pub result_quantity: FPDecimal,
    }

    #[cw_serde]
    pub enum OrderbookExecuteMsg {
        SwapMinOutput {
            target_denom: String,
            min_output_quantity: FPDecimal,
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
        /// Exact-output swap. `zero_for_one` pays token0 / receives token1.
        /// Native input is attached as funds (pool refunds any surplus over the
        /// actual cost); the aggregator only attaches the quoted cost, so no
        /// refund is expected. Reverts if the cost exceeds `maximum_amount_in`
        /// or the full `amount_out` can't be delivered.
        SwapExactOutput {
            zero_for_one: bool,
            amount_out: Uint128,
            maximum_amount_in: Uint128,
            recipient: Option<String>,
            deadline: Option<u64>,
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
        /// Exact-output quote: given a desired `amount_out` of `token_out`,
        /// returns the input cost in `amount_in_consumed` (and the actually
        /// deliverable `amount_out`, which is `< amount_out` only if the pool
        /// is liquidity/price-limit bound).
        QuoteExactOutput {
            token_out: amm::AssetInfo,
            amount_out: Uint128,
        },
        /// Pool config. We only deserialize `token0`/`token1` (serde ignores the
        /// rest) to resolve the `zero_for_one` direction for `SwapExactOutput`.
        GetConfig {},
    }

    #[cw_serde]
    pub struct QuoteResponse {
        pub amount_out: Uint128,
        pub amount_in_consumed: Uint128,
        pub fee_amount: Uint128,
    }

    /// Partial view of the pool's `PoolConfig` — only the fields we need. The
    /// pool's `AssetInfo` is wire-compatible with [`amm::AssetInfo`] (same
    /// `native_token`/`token` snake_case tags), and serde drops the unmodeled
    /// `factory`/`tick_spacing`/`fee_config`/`hook`/... fields on deserialize.
    #[cw_serde]
    pub struct ConfigResponse {
        pub token0: amm::AssetInfo,
        pub token1: amm::AssetInfo,
    }
}

#[cw_serde]
pub struct AmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
    pub ask_asset_info: amm::AssetInfo,
}

#[cw_serde]
pub struct OrderbookSwapOp {
    pub swap_contract: String,
    pub offer_asset_info: amm::AssetInfo,
    pub ask_asset_info: amm::AssetInfo,
    pub min_quantity_tick_size: Uint128,
    /// Per-op slippage tolerance in basis points applied to the simulated
    /// output to derive `min_output_quantity`. `None` defaults to
    /// [`DEFAULT_SLIPPAGE_BPS`](crate::execute::DEFAULT_SLIPPAGE_BPS) (50 =
    /// 0.5%), preserving the previous hardcoded behavior.
    pub max_slippage_bps: Option<u16>,
}

#[cw_serde]
pub struct ClmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
    pub ask_asset_info: amm::AssetInfo,
    /// Per-op slippage tolerance in basis points applied to the quoted output
    /// to derive `minimum_amount_out`. `None` defaults to
    /// [`DEFAULT_SLIPPAGE_BPS`](crate::execute::DEFAULT_SLIPPAGE_BPS) (50 =
    /// 0.5%), preserving the previous hardcoded behavior.
    pub max_slippage_bps: Option<u16>,
}

/// Exact-output CLMM leg: receive exactly `amount_out` of `ask_asset_info`,
/// paying the pool's quoted cost in `offer_asset_info` and refunding the
/// unspent portion of the leg's input budget to the route initiator.
/// **Native input only** for now (`offer_asset_info` must be `NativeToken`);
/// the struct is CW20-capable so allowance-based CW20 input can be added later
/// without a message migration.
#[cw_serde]
pub struct ClmmSwapExactOutputOp {
    pub pool_address: String,
    pub offer_asset_info: amm::AssetInfo,
    pub ask_asset_info: amm::AssetInfo,
    pub amount_out: Uint128,
}

#[cw_serde]
pub enum Operation {
    AmmSwap(AmmSwapOp),
    OrderbookSwap(OrderbookSwapOp),
    ClmmSwap(ClmmSwapOp),
    ClmmSwapExactOutput(ClmmSwapExactOutputOp),
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
