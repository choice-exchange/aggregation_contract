use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, Coin, Uint128};
#[allow(unused_imports)]
use crate::state::Config;

// This `external` module now represents the messages we will CONSTRUCT.
// It also includes the QueryMsg for the external router.
pub mod external {
    use cosmwasm_std::Binary;

    use super::*;

    #[cw_serde]
    pub enum ChoiceSwapOperation {
        Choice { 
            offer_asset_info: AssetInfo,
            ask_asset_info: AssetInfo,
        },
    }

    #[cw_serde]
    pub enum DojoSwapOperation {
        DojoSwap { 
            offer_asset_info: AssetInfo,
            ask_asset_info: AssetInfo,
        },
    }

    #[cw_serde]
    pub enum TerraSwapOperation {
        TerraSwap { 
            offer_asset_info: AssetInfo,
            ask_asset_info: AssetInfo,
        },
    }

    #[cw_serde]
    pub enum AstroSwapOperation {
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
    pub enum QueryMsg {
        SimulateSwapOperations {
            offer_amount: Uint128,
            operations: Binary,
        }
    }

    #[cw_serde]
    pub struct SimulateSwapOperationsResponse {
        pub amount: Uint128,
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