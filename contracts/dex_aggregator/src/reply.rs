// In dex-aggregator/src/reply.rs

use crate::error::ContractError;
use crate::execute::create_swap_cosmos_msg;
use crate::msg::{cw20_adapter, external, Operation, Stage};
use crate::state::{Awaiting, Config, ReplyState, CONFIG, REPLY_STATES};
use cosmwasm_std::{
    to_json_binary, Coin, CosmosMsg, DepsMut, Env, Reply, Response, StdError, SubMsg, Uint128, WasmMsg
};
use cw20::Cw20ExecuteMsg;
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};
use cosmwasm_std::{QueryRequest, BankQuery, WasmQuery};
use cw20::{BalanceResponse as Cw20BalanceResponse, Cw20QueryMsg};

pub fn handle_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut state = REPLY_STATES.load(deps.storage, msg.id)?;
    match state.awaiting {
        Awaiting::Swaps => handle_swap_reply(deps, env, msg, &mut state),
        Awaiting::Conversions => handle_conversion_reply(deps, env, msg, &mut state),
    }
}

fn handle_swap_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    state: &mut ReplyState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let master_reply_id = msg.id;

    let wasm_event = msg.result.clone().into_result().map_err(|_| ContractError::ReplyParseError {})?.events.into_iter().find(|e| e.ty.starts_with("wasm")).ok_or(ContractError::ReplyParseError {})?;
    let replying_contract_addr = wasm_event.attributes.iter().find(|attr| attr.key == "_contract_address").map(|attr| &attr.value).ok_or(ContractError::ReplyParseError {})?;
    let current_stage = &state.stages[state.current_stage_index as usize];
    let relevant_split = current_stage.splits.iter().find(|s| {
        let op_addr = match &s.operation {
            Operation::AmmSwap(op) => &op.pool_address,
            Operation::OrderbookSwap(op) => &op.swap_contract,
        };
        op_addr == replying_contract_addr
    }).ok_or_else(|| StdError::generic_err(format!("Could not find split for replying contract {}", replying_contract_addr)))?;
    
    let asset_info = get_operation_output(&relevant_split.operation)?;
    let amount = parse_amount_from_swap_reply(&msg)?;
    state.accumulated_assets.push(external::Asset { info: asset_info, amount });
    state.replies_expected -= 1;

    if state.replies_expected > 0 {
        REPLY_STATES.save(deps.storage, master_reply_id, state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_swap_outputs"));
    }

    if state.current_stage_index as usize >= state.stages.len() - 1 {
        return handle_final_stage(deps, env, msg.id, state);
    }

    let config = CONFIG.load(deps.storage)?;
    let target_input_asset = get_next_stage_input_asset(&state.stages, state.current_stage_index)?;
    let mut conversion_submsgs = vec![];

    for asset in &state.accumulated_assets {
        if asset.info == target_input_asset {
            state.ready_for_next_stage_amount += asset.amount;
        } else {
            conversion_submsgs.push(SubMsg::reply_on_success(create_conversion_msg(asset, &config, &env)?, master_reply_id));
        }
    }
    state.accumulated_assets.clear();

    if conversion_submsgs.is_empty() {
        state.current_stage_index += 1;
        execute_next_swap_stage(deps, env, state, master_reply_id)
    } else {
        state.awaiting = Awaiting::Conversions;
        state.replies_expected = conversion_submsgs.len() as u64;
        REPLY_STATES.save(deps.storage, master_reply_id, state)?;
        Ok(Response::new().add_submessages(conversion_submsgs).add_attribute("action", "normalizing_assets"))
    }
}

fn handle_conversion_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    state: &mut ReplyState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let master_reply_id = msg.id;
    state.ready_for_next_stage_amount += parse_amount_from_conversion_reply(&msg, &env)?;
    state.replies_expected -= 1;

    if state.replies_expected > 0 {
        REPLY_STATES.save(deps.storage, master_reply_id, state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_conversion_outputs"));
    }
    
    state.current_stage_index += 1;
    execute_next_swap_stage(deps, env, state, master_reply_id)
}

// All helper functions also return Response<InjectiveMsgWrapper>
fn handle_final_stage(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env, // We need Env to get the contract's own address
    reply_id: u64,
    state: &mut ReplyState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut response = Response::new();

    // 1. Calculate the TOTAL output from all splits by summing the event logs.
    // This represents the overall success of the transaction.
    let mut total_final_amount = Uint128::zero();
    for asset in &state.accumulated_assets {
        total_final_amount += asset.amount;
    }

    // 2. Check if this total meets the user's minimum requirement.
    if total_final_amount < state.minimum_receive {
        return Err(ContractError::MinimumReceiveNotMet {});
    }

    // 3. Determine what funds, if any, were sent to THIS aggregator contract.
    // We do this by querying our own balance of the final asset.
    let final_asset_info = state.accumulated_assets.first().map(|a| a.info.clone()).ok_or(ContractError::EmptyRoute {})?;

    let forwardable_balance: Uint128 = match &final_asset_info {
        external::AssetInfo::NativeToken { denom } => {
            let balance_query: cosmwasm_std::BalanceResponse = deps.querier.query(&cosmwasm_std::QueryRequest::Bank(BankQuery::Balance {
                address: env.contract.address.to_string(),
                denom: denom.clone(),
            }))?;
            balance_query.amount.amount
        }
        external::AssetInfo::Token { contract_addr } => {
            let balance_query: Cw20BalanceResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20QueryMsg::Balance {
                    address: env.contract.address.to_string(),
                })?,
            }))?;
            balance_query.balance
        }
    };

    // 4. If we are holding a balance, create a single message to forward it to the user.
    if !forwardable_balance.is_zero() {
        let send_msg: CosmosMsg<InjectiveMsgWrapper> = match &final_asset_info {
            external::AssetInfo::NativeToken { denom } => CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
                to_address: state.sender.to_string(),
                amount: vec![Coin { denom: denom.clone(), amount: forwardable_balance }],
            }),
            external::AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: state.sender.to_string(),
                    amount: forwardable_balance,
                })?,
                funds: vec![],
            }),
        };
        response = response.add_message(send_msg);
    }
    
    // 5. Clean up state and return the response.
    REPLY_STATES.remove(deps.storage, reply_id);

    Ok(response
        // The event log should reflect the TOTAL amount from all swaps.
        .add_attribute("action", "aggregate_swap_complete")
        .add_attribute("final_received", total_final_amount.to_string())
        .add_attribute("final_asset_type", format!("{:?}", final_asset_info)))
}

fn execute_next_swap_stage(deps: DepsMut<InjectiveQueryWrapper>, env: Env, state: &mut ReplyState, reply_id: u64) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let next_stage_idx = state.current_stage_index as usize;
    let next_stage = state.stages.get(next_stage_idx).ok_or(ContractError::EmptyRoute {})?;
    let offer_asset_info = get_operation_output(&state.stages[next_stage_idx-1].splits[0].operation)?;
    let mut submessages = vec![];
    for split in &next_stage.splits {
        let amount_for_split = state.ready_for_next_stage_amount.multiply_ratio(split.percent as u128, 100u128);
        let msg = create_swap_cosmos_msg(&split.operation, &offer_asset_info, amount_for_split, &state.sender, &env, &state.stages, next_stage_idx)?;
        submessages.push(SubMsg::reply_on_success(msg, reply_id));
    }
    state.awaiting = Awaiting::Swaps;
    state.replies_expected = submessages.len() as u64;
    state.ready_for_next_stage_amount = Uint128::zero();
    REPLY_STATES.save(deps.storage, reply_id, state)?;
    Ok(Response::new()
        .add_submessages(submessages)
        .add_attribute("action", "executing_next_stage")
        .add_attribute("stage_index", state.current_stage_index.to_string()))
}



fn create_conversion_msg(
    from: &external::Asset,
    config: &Config,
    env: &Env,
) -> Result<CosmosMsg<InjectiveMsgWrapper>, ContractError> {
    match &from.info {
        // Convert CW20 -> Native
        external::AssetInfo::Token { contract_addr } => {
            // This flow uses Cw20::Send which calls the adapter's `Receive` hook.
            let send_msg = Cw20ExecuteMsg::Send {
                contract: config.cw20_adapter_address.to_string(),
                amount: from.amount,
                msg: to_json_binary(&cw20_adapter::ReceiveSubmsg { recipient: env.contract.address.to_string() })?,
            };
            Ok(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&send_msg)?,
                funds: vec![],
            }))
        }
        // Convert Native -> CW20
        external::AssetInfo::NativeToken { denom } => {
            Ok(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: config.cw20_adapter_address.to_string(),
                msg: to_json_binary(&cw20_adapter::ExecuteMsg::RedeemAndTransfer {
                    recipient: Some(env.contract.address.to_string()),
                })?,
                funds: vec![Coin { denom: denom.clone(), amount: from.amount }],
            }))
        }
    }
}

fn get_operation_output(op: &Operation) -> Result<external::AssetInfo, ContractError> {
    Ok(match op {
        Operation::AmmSwap(o) => o.ask_asset_info.clone(),
        Operation::OrderbookSwap(o) => o.ask_asset_info.clone(),
    })
}

fn get_next_stage_input_asset(stages: &[Stage], current_stage_index: u64) -> Result<external::AssetInfo, ContractError> {
    let next_stage = stages.get((current_stage_index + 1) as usize).ok_or(ContractError::EmptyRoute {})?;
    let op = &next_stage.splits.first().ok_or(ContractError::EmptyRoute {})?.operation;
    Ok(match op {
        Operation::AmmSwap(o) => o.offer_asset_info.clone(),
        Operation::OrderbookSwap(o) => o.offer_asset_info.clone(),
    })
}

fn parse_amount_from_swap_reply(msg: &Reply) -> Result<Uint128, ContractError> {
    let event = msg.result.clone().into_result().map_err(|_| ContractError::ReplyParseError {})?.events.into_iter().find(|e| e.ty.starts_with("wasm")).ok_or(ContractError::ReplyParseError {})?;
    let key = if event.ty == "wasm-atomic_swap_execution" { "swap_final_amount" } else { "return_amount" };
    let amount_str = event.attributes.iter().find(|attr| attr.key == key).map(|attr| &attr.value).ok_or(ContractError::ReplyParseError {})?;
    amount_str.parse().map_err(Into::into)
}

fn parse_amount_from_conversion_reply(msg: &Reply, env: &Env) -> Result<Uint128, ContractError> {
    let events = &msg.result.clone().into_result().map_err(|_| ContractError::ReplyParseError {})?.events;
    if let Some(transfer_event) = events.iter().find(|e| e.ty == "transfer" && e.attributes.iter().any(|a| a.key == "recipient" && a.value == env.contract.address.to_string())) {
        let amount_attr = transfer_event.attributes.iter().find(|a| a.key == "amount").ok_or(ContractError::ReplyParseError {})?;
        let amount_val = amount_attr.value.trim_end_matches(|c: char| !c.is_digit(10));
        return amount_val.parse().map_err(Into::into);
    }
    if let Some(wasm_event) = events.iter().find(|e| e.ty.starts_with("wasm") && e.attributes.iter().any(|a| a.key == "action" && a.value == "transfer")) { // Adapter's Cw20 transfer fires a wasm event
        let amount_attr = wasm_event.attributes.iter().find(|a| a.key == "amount").ok_or(ContractError::ReplyParseError {})?;
        return amount_attr.value.parse().map_err(Into::into);
    }
    Err(ContractError::ReplyParseError {})
}