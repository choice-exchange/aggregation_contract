#[allow(unused_imports)]
use crate::state::Config;
use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, Coin, Decimal, Uint128};
use cw20::Cw20ReceiveMsg;

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
            deadline: Option<u64>,
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
}

#[cw_serde]
pub enum Operation {
    AmmSwap(AmmSwapOp),
    OrderbookSwap(OrderbookSwapOp),
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
}

pub struct StagePlan {
    pub swaps_to_execute: Vec<PlannedSwap>,
    pub conversions_needed: Vec<(amm::Asset, amm::AssetInfo)>,
}

#[cw_serde]
pub enum Cw20HookMsg {
    ExecuteRoute {
        stages: Vec<Stage>,
        minimum_receive: Option<String>,
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
        minimum_receive: Option<String>,
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
