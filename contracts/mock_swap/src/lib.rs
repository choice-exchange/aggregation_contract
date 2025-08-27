use cosmwasm_std::{
    entry_point, Binary, Decimal, Deps, DepsMut, Env, Event, MessageInfo, Response, StdResult, Uint128
};
use injective_math::FPDecimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use cosmwasm_schema::cw_serde;
use std::str::FromStr;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssetInfo {
    Token { contract_addr: String },
    NativeToken { denom: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Asset {
    pub info: AssetInfo,
    pub amount: Uint128,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    Swap {
        offer_asset: Asset,
        belief_price: Option<Decimal>,
        max_spread: Option<Decimal>,
        to: Option<String>,
        deadline: Option<u64>,
    },
    SwapMinOutput {
        target_denom: String, 
        min_output_quantity: String 
    },
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct InstantiateMsg {}

#[cw_serde]
pub enum QueryMsg {}

#[entry_point] 
pub fn instantiate(_deps: DepsMut, _env: Env, _info: MessageInfo, _msg: InstantiateMsg) -> StdResult<Response> {
    Ok(Response::new())
}

#[entry_point]
pub fn execute(
    _deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response> {
    // The logic is now much simpler and more correct.
    let return_amount = match msg {
        // AMM swaps are differentiated by the funds sent.
        ExecuteMsg::Swap { .. } => {
            let sent_amount = info.funds[0].amount;
            if sent_amount == Uint128::from(33_000_000_000_000_000_000u128) {
                 Uint128::from(5_200_000_000_000_000_000_000_000u128)
            } else if sent_amount == Uint128::from(42_000_000_000_000_000_000u128) {
                 Uint128::from(6_600_000_000_000_000_000_000_000u128)
            } else {
                // This case should not be hit by the 3-way split test,
                // but it's good practice to have a default.
                Uint128::zero()
            }
        }
        // Orderbook swap now correctly parses the min_output from its own message type.
        ExecuteMsg::SwapMinOutput { target_denom: _, min_output_quantity } => {
            FPDecimal::from_str(&min_output_quantity)
                .map_err(|e| cosmwasm_std::StdError::generic_err(e.to_string()))?
                .into()
        }
    };

    let event = Event::new("wasm")
        .add_attribute("action", "swap")
        .add_attribute("return_amount", return_amount.to_string());

    Ok(Response::new().add_event(event))
}

#[entry_point]
pub fn query(_deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {

    match msg {}
}