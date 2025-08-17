use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, Coin, Uint128};
#[allow(unused_imports)]
use crate::state::Config;

// This `external` module now represents the messages we will CONSTRUCT.
// It also includes the QueryMsg for the external router.
pub mod external {
    use super::*;

    #[cw_serde]
    pub enum SwapOperation {
        Choice { // We'll use Choice as the example AMM type
            offer_asset_info: AssetInfo,
            ask_asset_info: AssetInfo,
        },
    }

    #[cw_serde]
    pub enum AssetInfo {
        Token { contract_addr: String },
        NativeToken { denom: String },
    }

    // Query messages for an external AMM router contract
    #[cw_serde]
    pub enum QueryMsg {
        SimulateSwapOperations {
            offer_amount: Uint128,
            operations: Vec<SwapOperation>,
        }
    }

    // The response from an external AMM router's simulation query
    #[cw_serde]
    pub struct SimulateSwapOperationsResponse {
        pub amount: Uint128,
    }
}

#[cw_serde]
pub struct InstantiateMsg {
    pub admin: String,
}

#[cw_serde]
pub enum ExecuteMsg {
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
    SimulateRoute {
        route: Route,
        amount_in: Coin,
    },
    #[returns(Config)]
    Config {},
}

#[cw_serde]
pub struct SimulateRouteResponse {
    pub output_amount: Uint128,
}

// --- NEW DESCRIPTIVE DATA STRUCTURES ---

#[cw_serde]
pub enum AssetType {
    Cw20(Addr),
    Bank(String),
}

// An enum to identify the protocol, so our contract knows which query/execute format to use.
#[cw_serde]
pub enum AmmProtocol {
    Choice,
    // Add other protocols like DojoSwap here
}

// This is a DESCRIPTION of the action, not the action itself.
#[cw_serde]
pub enum ActionDescription {
    AmmSwap {
        protocol: AmmProtocol,
        offer_asset_info: external::AssetInfo,
        ask_asset_info: external::AssetInfo,
    },
    // Future descriptions for OrderbookSwap, etc., would go here
}

#[cw_serde]
pub struct Step {
    // The address of the contract that will perform the action (e.g., the AMM Router address)
    pub protocol_address: Addr,
    // The description of what should happen at that address
    pub description: ActionDescription,
    pub amount_in_percentage: u8,
    pub next_steps: Vec<u32>,
}

#[cw_serde]
pub struct Route {
    pub steps: Vec<Step>,
    // Note: The `asset_in` on the route is descriptive, but the actual
    // asset type for a given step comes from its `ActionDescription`.
}