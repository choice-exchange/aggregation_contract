use cosmwasm_schema::cw_serde;
use cosmwasm_std::{
    entry_point, from_json, to_json_binary, BankMsg, Binary, Coin, CosmosMsg, Decimal, Deps,
    DepsMut, Env, Event, MessageInfo, Response, StdError, StdResult, Uint128, WasmMsg,
};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
use cw_storage_plus::Item;
use injective_cosmwasm::InjectiveQueryWrapper;
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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct SwapEstimationResult {
    pub result_quantity: FPDecimal,
    // For a mock, we can return a dummy fee estimate
    pub expected_fees: Vec<FPCoin>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct FPCoin {
    pub amount: FPDecimal,
    pub denom: String,
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
    Receive(Cw20ReceiveMsg),
}

#[cw_serde]
pub enum ProtocolType {
    Amm,
    Orderbook,
}

#[cw_serde]
pub struct SwapConfig {
    pub input_asset_info: AssetInfo,
    pub output_asset_info: AssetInfo,
    pub rate: String,
    pub protocol_type: ProtocolType,
    pub input_decimals: u8,
    pub output_decimals: u8,
}

#[cw_serde]
pub struct InstantiateMsg {
    pub config: SwapConfig,
}

#[cw_serde]
pub struct MockSwapHookMsg {
    pub swap: MockSwapHookSwapField,
}

#[cw_serde]
pub struct MockSwapHookSwapField {
    pub offer_asset: Option<Asset>,
    pub belief_price: Option<Decimal>,
    pub max_spread: Option<Decimal>,
    pub to: Option<String>,
    pub deadline: Option<u64>,
}

#[cw_serde]
pub enum QueryMsg {
    GetOutputQuantity {
        from_quantity: FPDecimal,
        source_denom: String,
        target_denom: String,
    },
}

pub const CONFIG: Item<SwapConfig> = Item::new("config");
const DECIMAL_PRECISION: u32 = 18;

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
    let config = CONFIG.load(deps.storage)?;
    let mut recipient = info.sender.to_string();

    let (offer_amount, offer_info) = match msg {
        ExecuteMsg::Swap {
            offer_asset, to, ..
        } => {
            if let Some(to_addr) = to {
                recipient = to_addr;
            }
            (offer_asset.amount, offer_asset.info)
        }
        ExecuteMsg::SwapMinOutput { .. } => (
            info.funds[0].amount,
            AssetInfo::NativeToken {
                denom: info.funds[0].denom.clone(),
            },
        ),
        ExecuteMsg::Receive(Cw20ReceiveMsg {
            sender,
            amount,
            msg,
        }) => {
            if let Ok(hook) = from_json::<MockSwapHookMsg>(&msg) {
                recipient = hook.swap.to.unwrap_or(sender);
            } else {
                recipient = sender;
            }
            (
                amount,
                AssetInfo::Token {
                    contract_addr: info.sender.to_string(),
                },
            )
        }
    };

    let final_return_amount = if offer_info == config.input_asset_info {
        let offer_decimal = Decimal::from_atomics(offer_amount, config.input_decimals as u32)
            .map_err(|_| StdError::generic_err("Failed to create decimal from offer amount"))?;

        let rate_decimal = Decimal::from_str(&config.rate)?;
        let return_decimal = offer_decimal * rate_decimal;
        let decimal_diff = DECIMAL_PRECISION.saturating_sub(config.output_decimals as u32);
        let scaling_factor = Uint128::from(10u128.pow(decimal_diff));

        return_decimal
            .atomics()
            .checked_div(scaling_factor)
            .unwrap_or_default()
    } else {
        Uint128::zero()
    };

    if final_return_amount.is_zero() {
        return Ok(Response::new().add_attribute("action", "swap_skipped_or_zero_amount"));
    }

    let send_msg: CosmosMsg = match &config.output_asset_info {
        AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: recipient.clone(),
                amount: final_return_amount,
            })?,
            funds: vec![],
        }),
        AssetInfo::NativeToken { denom } => CosmosMsg::Bank(BankMsg::Send {
            to_address: recipient,
            amount: vec![Coin {
                denom: denom.clone(),
                amount: final_return_amount,
            }],
        }),
    };

    let (input_denom_str, _) = get_denom_and_addr(&config.input_asset_info);
    let (output_denom_str, _) = get_denom_and_addr(&config.output_asset_info);

    let event = match config.protocol_type {
        ProtocolType::Amm => Event::new("wasm")
            .add_attribute("action", "swap")
            .add_attribute("return_amount", final_return_amount.to_string()),
        ProtocolType::Orderbook => Event::new("atomic_swap_execution")
            .add_attribute("sender", info.sender.to_string())
            .add_attribute("swap_input_amount", offer_amount)
            .add_attribute("swap_input_denom", input_denom_str)
            .add_attribute("refund_amount", "0")
            .add_attribute("swap_final_amount", final_return_amount)
            .add_attribute("swap_final_denom", output_denom_str),
    };

    Ok(Response::new().add_message(send_msg).add_event(event))
}

fn get_denom_and_addr(asset_info: &AssetInfo) -> (String, String) {
    match asset_info {
        AssetInfo::NativeToken { denom } => (denom.clone(), "".to_string()),
        AssetInfo::Token { contract_addr } => (contract_addr.clone(), contract_addr.clone()),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(
    deps: Deps<InjectiveQueryWrapper>,
    _env: Env,
    msg: QueryMsg,
) -> Result<Binary, StdError> {
    match msg {
        QueryMsg::GetOutputQuantity {
            from_quantity,
            source_denom,
            target_denom,
        } => {
            let config = CONFIG.load(deps.storage)?;

            // 1. Validation: Ensure the query matches the contract's configured trading pair.
            let config_source_denom = match config.input_asset_info {
                AssetInfo::NativeToken { denom } => denom,
                AssetInfo::Token { contract_addr } => contract_addr,
            };
            let config_target_denom = match config.output_asset_info {
                AssetInfo::NativeToken { denom } => denom,
                AssetInfo::Token { contract_addr } => contract_addr,
            };

            if source_denom != config_source_denom || target_denom != config_target_denom {
                return Err(StdError::generic_err(format!(
                    "Invalid trading pair for this mock contract. Expected {} -> {}, got {} -> {}",
                    config_source_denom, config_target_denom, source_denom, target_denom
                )));
            }

            // 2. The Core Mock Logic: Perform the simple rate calculation.
            let rate = FPDecimal::from_str(&config.rate)?;
            let result_quantity = from_quantity * rate;

            // 3. Construct the response object that the aggregator expects.
            let response = SwapEstimationResult {
                result_quantity,
                // For a mock, we can return an empty or zero fee.
                expected_fees: vec![FPCoin {
                    amount: FPDecimal::ZERO,
                    denom: target_denom, // The fee is in the output currency
                }],
            };

            to_json_binary(&response)
        }
    }
}
