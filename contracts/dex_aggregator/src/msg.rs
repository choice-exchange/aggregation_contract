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
        /// Registers a new CW-20 contract that will be handled by the adapter
        RegisterCw20Contract { addr: Addr },
        ///  Impl of Receiver CW-20 interface. Should be called by CW-20 contract only!! (never directly). Msg is ignored
        Receive {
            sender: String,
            amount: Uint128,
            msg: Binary,
        },
        /// Called to redeem TF tokens. Will send CW-20 tokens to "recipient" address (or sender if not provided). Will use transfer method
        RedeemAndTransfer { recipient: Option<String> },
        /// Called to redeem TF tokens. Will call Send method of CW:20 to send CW-20 tokens to "recipient" address. Submessage will be passed to send method (can be empty)
        RedeemAndSend { recipient: String, submsg: Binary },
        /// Updates stored metadata
        UpdateMetadata { addr: Addr },
    }

    #[cw_serde]
    pub enum QueryMsg {
        /// Return a list of registered CW-20 contracts
        RegisteredContracts {},
        /// Returns a fee required to register a new token-factory denom
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
        // ReverseSimulation { ask_asset: Asset },
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
    pub operation: Operation,
    pub percent: u8,
}

#[cw_serde]
pub struct Stage {
    pub splits: Vec<Split>,
}

#[cw_serde] // Add this if you plan to store it in state
pub struct PlannedSwap {
    pub operation: Operation,
    pub amount: Uint128,
}

// The complete plan for a stage, produced by the planner.
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
