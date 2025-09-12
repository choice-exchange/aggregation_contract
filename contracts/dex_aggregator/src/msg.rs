#[allow(unused_imports)]
use crate::state::Config;
use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, Coin, Decimal, Uint128};
use cw20::Cw20ReceiveMsg;
use injective_math::FPDecimal;

pub mod cw20_adapter {
    use cosmwasm_std::Binary;

    use super::*;

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

pub mod external {

    use super::*;

    #[cw_serde]
    pub enum SwapOperation {
        Choice {
            offer_asset_info: AssetInfo,
            ask_asset_info: AssetInfo,
        },
        DojoSwap {
            offer_asset_info: AssetInfo,
            ask_asset_info: AssetInfo,
        },
        TerraSwap {
            offer_asset_info: AssetInfo,
            ask_asset_info: AssetInfo,
        },
        AstroSwap {
            offer_asset_info: AssetInfo,
            ask_asset_info: AssetInfo,
        },
    }

    #[cw_serde]
    pub enum AssetInfo {
        Token { contract_addr: String },
        NativeToken { denom: String },
    }

    #[cw_serde]
    pub struct SimulateSwapOperationsResponse {
        pub amount: Uint128,
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
    pub struct SimulationResponse {
        pub return_amount: Uint128,
        pub spread_amount: Uint128,
        pub commission_amount: Uint128,
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
}

#[cw_serde]
pub struct AmmSwapOp {
    pub pool_address: String,
    pub offer_asset_info: external::AssetInfo,
    pub ask_asset_info: external::AssetInfo,
}

#[cw_serde]
pub struct OrderbookSwapOp {
    pub swap_contract: String,
    pub offer_asset_info: external::AssetInfo,
    pub ask_asset_info: external::AssetInfo,
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
    pub conversions_needed: Vec<(external::Asset, external::AssetInfo)>,
}

#[cw_serde]
pub enum Cw20HookMsg {
    AggregateSwaps {
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
    AggregateSwaps {
        stages: Vec<Stage>,
        minimum_receive: Option<String>,
    },
    Receive(Cw20ReceiveMsg),
    ExecuteRoute {
        route: Route,
        minimum_receive: Option<Uint128>,
    },
    UpdateAdmin {
        new_admin: String,
    },
    SetFee {
        pool_address: String,
        fee_percent: Decimal,
    },
    /// Admin-only. Removes the fee configuration for a specific pool.
    RemoveFee {
        pool_address: String,
    },
    /// Admin-only. Updates the address where fees are sent.
    UpdateFeeCollector {
        new_fee_collector: String,
    },
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(SimulateRouteResponse)]
    SimulateRoute { route: Route, amount_in: Coin },
    #[returns(Config)]
    Config {},
}

#[cw_serde]
pub struct SimulateRouteResponse {
    pub output_amount: Uint128,
}

#[cw_serde]
pub enum AssetType {
    Cw20(Addr),
    Bank(String),
}

// An enum to identify the protocol, so our contract knows which query/execute format to use.
#[cw_serde]
pub enum AmmProtocol {
    Choice,
    DojoSwap,
    AstroSwap,
    TerraSwap,
}

// This is a DESCRIPTION of the action, not the action itself.
#[cw_serde]
pub enum ActionDescription {
    AmmSwap {
        protocol: AmmProtocol,
        offer_asset_info: external::AssetInfo,
        ask_asset_info: external::AssetInfo,
    },
    OrderbookSwap {
        source_denom: String,
        target_denom: String,
    },
}

#[cw_serde]
pub struct Step {
    // The address of the contract that will perform the action (e.g., the AMM Router address)
    pub protocol_address: Addr,
    // The description of what should happen at that address
    pub description: ActionDescription,
    pub amount_in_percentage: u8,
    pub next_steps: Vec<usize>,
}

#[cw_serde]
pub struct Route {
    pub steps: Vec<Step>,
    // Note: The `asset_in` on the route is descriptive, but the actual
    // asset type for a given step comes from its `ActionDescription`.
}

#[cosmwasm_schema::cw_serde]
pub enum AmmPairExecuteMsg {
    Swap {
        offer_asset: external::Asset,
        belief_price: Option<Decimal>,
        max_spread: Option<Decimal>,
        to: Option<String>,
        deadline: Option<u64>,
    },
}

/// The ExecuteMsg format for the Orderbook swap contract.
#[cosmwasm_schema::cw_serde]
pub enum OrderbookExecuteMsg {
    SwapMinOutput {
        target_denom: String,
        min_output_quantity: FPDecimal,
    },
}
