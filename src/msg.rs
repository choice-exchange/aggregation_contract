use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, Coin, Uint128};
#[allow(unused_imports)] 
use crate::state::Config;

// Re-exporting external types we need to interact with.
// NOTE: You should replace these with actual imports from the crates when available.
pub mod external {
    use super::*;
    
    // From the AMM Router
    #[cw_serde]
    pub enum SwapOperation {
        Choice {
            offer_asset_info: AssetInfo,
            ask_asset_info: AssetInfo,
        },
    }

    #[cw_serde]
    pub enum AssetInfo {
        Token { contract_addr: String },
        NativeToken { denom: String },
    }
}

#[cw_serde]
pub struct InstantiateMsg {
    pub admin: String,
}

#[cw_serde]
pub enum ExecuteMsg {
    /// The primary entry point for executing a trade
    ExecuteRoute {
        route: Route,
        // Optional minimum receive amount for the final token to protect against slippage.
        minimum_receive: Option<Uint128>,
    },
    /// Admin function to update the administrator
    UpdateAdmin {
        new_admin: String,
    },
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    /// Simulates a route execution to preview the expected output
    #[returns(SimulateRouteResponse)]
    SimulateRoute {
        route: Route,
        amount_in: Coin,
    },
    /// Returns the current contract configuration
    #[returns(Config)]
    Config {},
}

#[cw_serde]
pub struct SimulateRouteResponse {
    pub output_amount: Uint128,
}


// --- Core Route Data Structures ---

#[cw_serde]
pub enum AssetType {
    Cw20(Addr),   // The value is the cw20 token contract address
    Bank(String), // The value is the native denomination (e.g., "inj" or "factory/...")
}

#[cw_serde]
pub enum Action {
    /// A trade on a Choice AMM
    AmmSwap {
        // The address of the Choice AMM Router contract
        router_address: Addr,
        // The full path of swaps to perform within the AMM
        operations: Vec<external::SwapOperation>,
    },
    /// A trade on the Injective Order Book
    OrderbookSwap {
        // The address of the order book swap contract
        contract_address: Addr,
        // The market route to take (placeholder as MarketId might not be serde-friendly for all contexts)
        route_markets: Vec<String>,
        target_denom: String,
    },
    /// A conversion between a cw20 token and its native bank equivalent
    Convert {
        // The address of the adapter contract that handles the minting/burning
        adapter_address: Addr,
        asset_in: AssetType,
        asset_out: AssetType,
    },
}

#[cw_serde]
pub struct Step {
    /// The action to execute in this step
    pub action: Action,
    /// Percentage of the available funds from previous steps to use for this action
    pub amount_in_percentage: u8,
    /// Indices pointing to the next steps, enabling splits and rejoins
    pub next_steps: Vec<u32>,
    /// The asset this step is expected to receive. Used for balance checks in replies.
    pub asset_out: AssetType,
}

#[cw_serde]
pub struct Route {
    pub steps: Vec<Step>,
    /// The initial asset being provided by the user.
    pub asset_in: AssetType,
}

