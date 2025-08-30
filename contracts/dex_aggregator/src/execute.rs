use cosmwasm_std::{
    to_json_binary, Addr, Coin, CosmosMsg, DepsMut, Env, MessageInfo, Response, StdError, SubMsg,
    Uint128, WasmMsg,
};
use cw20::Cw20ExecuteMsg;
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};
use injective_math::FPDecimal;
use std::str::FromStr;

use crate::error::ContractError;
use crate::msg::{external, AmmPairExecuteMsg, Operation, OrderbookExecuteMsg, Route, Stage};
use crate::state::{Awaiting, ReplyState, REPLY_ID_COUNTER, REPLY_STATES};

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
    env: Env,
    _info: MessageInfo,
    stages: Vec<Stage>,
    minimum_receive_str: Option<String>,
    offer_asset: external::Asset,
    initiator: Addr,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if offer_asset.amount.is_zero() {
        return Err(ContractError::ZeroAmount {});
    }
    let first_stage = stages.first().ok_or(ContractError::NoStages {})?;
    let total_percentage: u8 = first_stage.splits.iter().map(|s| s.percent).sum();
    if total_percentage != 100 {
        return Err(ContractError::InvalidPercentageSum {});
    }

    let reply_id = REPLY_ID_COUNTER.may_load(deps.storage)?.unwrap_or(0) + 1;
    REPLY_ID_COUNTER.save(deps.storage, &reply_id)?;

    let minimum_receive = match minimum_receive_str {
        Some(s) => Uint128::from_str(&s)?,
        None => Uint128::zero(),
    };

    let initial_state = ReplyState {
        sender: initiator.clone(),
        minimum_receive,
        stages: stages.clone(),
        // Initialize the state machine
        awaiting: Awaiting::Swaps,
        current_stage_index: 0,
        replies_expected: first_stage.splits.len() as u64,
        // Initialize accumulators
        accumulated_assets: vec![],
        ready_for_next_stage_amount: Uint128::zero(),
    };
    REPLY_STATES.save(deps.storage, reply_id, &initial_state)?;

    let mut submessages: Vec<SubMsg<InjectiveMsgWrapper>> = vec![];
    for split in &first_stage.splits {
        let split_amount = offer_asset
            .amount
            .multiply_ratio(split.percent as u128, 100u128);

        let msg = create_swap_cosmos_msg(
            &split.operation,
            &offer_asset.info,
            split_amount,
            &initiator,
            &env,
            &stages,
            0,
        )?;
        submessages.push(SubMsg::reply_on_success(msg, reply_id));
    }

    Ok(Response::new()
        .add_submessages(submessages)
        .add_attribute("action", "multi_stage_swap_started")
        .add_attribute("initiator", initiator)
        .add_attribute("reply_id", reply_id.to_string()))
}

pub fn create_swap_cosmos_msg(
    operation: &Operation,
    offer_asset_info: &external::AssetInfo,
    amount: Uint128,
    initiator: &Addr,
    env: &Env,
    stages: &[Stage],
    current_stage_index: usize,
) -> Result<CosmosMsg<InjectiveMsgWrapper>, ContractError> {
    let is_last_stage = current_stage_index == stages.len() - 1;

    let recipient = if is_last_stage {
        initiator.to_string()
    } else {
        env.contract.address.to_string()
    };

    let cosmos_msg = match operation {
        Operation::AmmSwap(amm_op) => {
            let amm_swap_msg = AmmPairExecuteMsg::Swap {
                offer_asset: external::Asset {
                    info: offer_asset_info.clone(),
                    amount,
                },
                belief_price: None,
                max_spread: None,
                to: Some(recipient),
                deadline: None,
            };

            match offer_asset_info {
                external::AssetInfo::NativeToken { denom } => CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: amm_op.pool_address.clone(),
                    msg: to_json_binary(&amm_swap_msg)?,
                    funds: vec![Coin {
                        denom: denom.clone(),
                        amount,
                    }],
                }),
                external::AssetInfo::Token { contract_addr } => {
                    let cw20_send_msg = Cw20ExecuteMsg::Send {
                        contract: amm_op.pool_address.clone(),
                        amount,
                        msg: to_json_binary(&amm_swap_msg)?,
                    };

                    CosmosMsg::Wasm(WasmMsg::Execute {
                        contract_addr: contract_addr.clone(),
                        msg: to_json_binary(&cw20_send_msg)?,
                        funds: vec![],
                    })
                }
            }
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

            let min_output_quantity = FPDecimal::from_str(&ob_op.min_output)?;

            let swap_msg = OrderbookExecuteMsg::SwapMinOutput {
                target_denom,
                min_output_quantity,
            };

            let funds = if let external::AssetInfo::NativeToken { denom } = offer_asset_info {
                vec![Coin {
                    denom: denom.clone(),
                    amount,
                }]
            } else {
                vec![]
            };

            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: ob_op.swap_contract.clone(),
                msg: to_json_binary(&swap_msg)?,
                funds,
            })
        }
    };

    Ok(cosmos_msg)
}
