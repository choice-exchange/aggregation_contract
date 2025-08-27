use cosmwasm_std::{to_json_binary, Addr, Coin, CosmosMsg, Decimal, DepsMut, Env, MessageInfo, Response, StdError, SubMsg, Uint128, WasmMsg};
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};
use injective_math::FPDecimal;
use std::str::FromStr;

use crate::error::ContractError;
use crate::msg::{external, Route, Stage, Operation};
use crate::state::{ReplyState, REPLY_ID_COUNTER, REPLY_STATES};

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


pub fn execute_route(
    _deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    _info: MessageInfo,
    _route: Route,
    _minimum_receive: Option<Uint128>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    // ... implementation logic ...

    Err(ContractError::Std(StdError::generic_err("Not implemented"))) // Reverted to generic_err
}

pub fn update_admin(
    _deps: DepsMut<InjectiveQueryWrapper>,
    _info: MessageInfo,
    _new_admin: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    // ... implementation logic ...

    Err(ContractError::Unauthorized {}) // Reverted to generic_err
}

pub fn execute_aggregate_swaps_internal(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    _info: MessageInfo,
    stages: Vec<Stage>,
    minimum_receive_str: Option<String>,
    offer_asset: external::Asset,
    initiator: Addr, // The actual user who started the swap
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if offer_asset.amount.is_zero() {
        return Err(ContractError::ZeroAmount {});
    }

    let first_stage = stages.get(0).ok_or(ContractError::NoStages {})?;

    let total_percentage: u8 = first_stage.splits.iter().map(|s| s.percent).sum();
    if total_percentage != 100 {
        return Err(ContractError::InvalidPercentageSum {});
    }

    let mut submessages: Vec<SubMsg<InjectiveMsgWrapper>> = vec![];

    // --- Setup for Reply Handling ---
    let reply_id = REPLY_ID_COUNTER.may_load(deps.storage)?.unwrap_or(0) + 1;
    REPLY_ID_COUNTER.save(deps.storage, &reply_id)?;

    let minimum_receive = match minimum_receive_str {
        Some(s) => Uint128::from_str(&s)?,
        None => Uint128::zero(),
    };

    let reply_state = ReplyState {
        sender: initiator.clone(),
        minimum_receive,
        expected_replies: first_stage.splits.len() as u64,
        accumulated_amount: Uint128::zero(),
    };
    REPLY_STATES.save(deps.storage, reply_id, &reply_state)?;


    for split in &first_stage.splits {
        let split_amount = offer_asset.amount.multiply_ratio(split.percent as u128, 100u128);

        let msg = match &split.operation {
            Operation::AmmSwap(amm_op) => {
                let offer_asset_for_split = external::Asset {
                    info: offer_asset.info.clone(),
                    amount: split_amount,
                };

                let swap_msg = AmmPairExecuteMsg::Swap {
                    offer_asset: offer_asset_for_split,
                    belief_price: None, max_spread: None,
                    to: Some(initiator.to_string()),
                    deadline: None,
                };
                
                // Funds are only sent if the offer asset is a native token
                let funds = if let external::AssetInfo::NativeToken { denom } = &offer_asset.info {
                    vec![Coin { denom: denom.clone(), amount: split_amount }]
                } else {
                    vec![]
                };

                CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: amm_op.pool_address.clone(),
                    msg: to_json_binary(&swap_msg)?,
                    funds,
                })
            }
            Operation::OrderbookSwap(ob_op) => {
                let target_denom = match &ob_op.ask_asset_info {
                    external::AssetInfo::NativeToken { denom } => denom.clone(),
                    external::AssetInfo::Token { .. } => {
                        return Err(ContractError::Std(StdError::generic_err(
                            "Orderbook swaps only support native token (bank) outputs.",
                        )));
                    }
                };

                // 2. Parse the min_output string into the required FPDecimal type.
                let min_output_quantity = FPDecimal::from_str(&ob_op.min_output)?;

                // 3. Construct the execute message for the orderbook swap contract.
                let swap_msg = OrderbookExecuteMsg::SwapMinOutput {
                    target_denom,
                    min_output_quantity,
                };

                // 4. Determine funds to send, same as in the AMM logic.
                let funds = if let external::AssetInfo::NativeToken { denom } = &offer_asset.info {
                    vec![Coin { denom: denom.clone(), amount: split_amount }]
                } else {
                    vec![]
                };

                // 5. Assemble the final WasmMsg.
                CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: ob_op.swap_contract.clone(),
                    msg: to_json_binary(&swap_msg)?,
                    funds,
                })
            }
        };

        // Create a submessage that will call our reply endpoint
        submessages.push(SubMsg::reply_on_success(msg, reply_id));
    }

    Ok(Response::new()
        .add_submessages(submessages)
        .add_attribute("action", "aggregate_swaps_dispatched")
        .add_attribute("initiator", initiator)
        .add_attribute("reply_id", reply_id.to_string()))
}