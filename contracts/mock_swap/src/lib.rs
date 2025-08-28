use cosmwasm_schema::cw_serde;
use cosmwasm_std::{
    entry_point, BankMsg, Binary, Coin, CosmosMsg, Decimal, Deps, DepsMut, Env, Event, MessageInfo,
    Response, StdResult, Uint128,
};
use injective_math::FPDecimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
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
        min_output_quantity: String,
    },
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct InstantiateMsg {}

#[cw_serde]
pub enum QueryMsg {}

#[entry_point]
pub fn instantiate(
    _deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    _msg: InstantiateMsg,
) -> StdResult<Response> {
    Ok(Response::new())
}

#[entry_point]
pub fn execute(
    _deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response> {
    let recipient = info.sender.to_string();

    let (return_amount, output_denom): (Uint128, String) = match msg {
        ExecuteMsg::Swap { offer_asset, .. } => {
            let output_denom = "usdt".to_string(); // In AMM swaps, we mock a final token
            let amount = if let AssetInfo::NativeToken { denom } = &offer_asset.info {
                if denom == "inj" {
                    let sent_amount = offer_asset.amount;
                    if sent_amount == Uint128::from(33_000_000_000_000_000_000u128) {
                        Uint128::from(5_200_000_000_000_000_000_000_000u128)
                    } else if sent_amount == Uint128::from(42_000_000_000_000_000_000u128) {
                        Uint128::from(6_600_000_000_000_000_000_000_000u128)
                    } else if sent_amount == Uint128::from(362181137498213706u128) {
                        Uint128::from(63174284362280640946506u128)
                    } else if sent_amount == Uint128::from(376964041069569367u128) {
                        Uint128::from(65736109058836791911471u128)
                    } else {
                        Uint128::zero()
                    }
                } else {
                    Uint128::zero()
                }
            } else {
                Uint128::zero()
            };
            (amount, output_denom) // Return the tuple
        }
        ExecuteMsg::SwapMinOutput {
            target_denom,
            min_output_quantity,
            ..
        } => {
            let output_denom = target_denom; // Use the denom passed in the message
            let amount = {
                let sent_denom = &info.funds[0].denom;
                if sent_denom == "usdt" {
                    Uint128::from(739145178567783074u128)
                } else if sent_denom == "inj" {
                    FPDecimal::from_str(&min_output_quantity)
                        .map_err(|e| cosmwasm_std::StdError::generic_err(e.to_string()))?
                        .into()
                } else {
                    Uint128::zero()
                }
            };
            (amount, output_denom) // Return the tuple
        }
    };

    let event = Event::new("wasm")
        .add_attribute("action", "swap")
        .add_attribute("return_amount", return_amount.to_string());

    let bank_send_msg = CosmosMsg::Bank(BankMsg::Send {
        to_address: recipient,
        amount: vec![Coin {
            denom: output_denom,
            amount: return_amount,
        }],
    });

    Ok(Response::new().add_message(bank_send_msg).add_event(event))
}

#[entry_point]
pub fn query(_deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {}
}
