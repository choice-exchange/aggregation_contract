use cosmwasm_schema::cw_serde;
use cosmwasm_std::{
    entry_point, BankMsg, Binary, Coin, CosmosMsg, Decimal, Deps, DepsMut, Env, Event, MessageInfo, Response, StdResult, Uint128
};
use injective_math::FPDecimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use cw_storage_plus::Item;

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

#[cw_serde]
pub enum ProtocolType {
    Amm,
    Orderbook,
}

#[cw_serde]
pub struct SwapConfig {
    pub input_denom: String,
    pub output_denom: String,
    pub rate: String,
    pub protocol_type: ProtocolType, // New field to determine event type
}

#[cw_serde]
pub struct InstantiateMsg {
    pub config: SwapConfig,
}

#[cw_serde]
pub enum QueryMsg {}

pub const CONFIG: Item<SwapConfig> = Item::new("config");


#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    CONFIG.save(deps.storage, &msg.config)?;
    Ok(Response::new())
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response> {
    let recipient = info.sender.to_string();
    let config = CONFIG.load(deps.storage)?;

    let (return_amount, output_denom, input_amount, input_denom) = match msg {
        ExecuteMsg::Swap { offer_asset, .. } => {
            let sent_amount = offer_asset.amount;
            let sent_denom = match offer_asset.info {
                AssetInfo::NativeToken { denom } => denom,
                AssetInfo::Token { contract_addr } => contract_addr,
            };
            let rate = FPDecimal::from_str(&config.rate).map_err(|e| cosmwasm_std::StdError::generic_err(e.to_string()))?;
            let return_amount = if sent_denom == config.input_denom { (FPDecimal::from(sent_amount.u128()) * rate).into() } else { Uint128::zero() };
            (return_amount, config.output_denom, sent_amount, sent_denom)
        }
        ExecuteMsg::SwapMinOutput { .. } => {
            let sent_coin = &info.funds[0];
            let rate = FPDecimal::from_str(&config.rate).map_err(|e| cosmwasm_std::StdError::generic_err(e.to_string()))?;
            let return_amount = if sent_coin.denom == config.input_denom { (FPDecimal::from(sent_coin.amount.u128()) * rate).into() } else { Uint128::zero() };
            (return_amount, config.output_denom, sent_coin.amount, sent_coin.denom.clone())
        }
    };

    let event = match config.protocol_type {
        ProtocolType::Amm => Event::new("wasm")
            .add_attribute("action", "swap")
            .add_attribute("return_amount", return_amount.to_string()),
        ProtocolType::Orderbook => {
            Event::new("atomic_swap_execution")
                .add_attribute("swap_input_amount", input_amount)
                .add_attribute("swap_input_denom", input_denom)
                .add_attribute("refund_amount", "0")
                .add_attribute("swap_final_amount", return_amount)
                .add_attribute("swap_final_denom", output_denom.clone())
        }
    };

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
