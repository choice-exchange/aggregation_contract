use crate::error::ContractError;
use crate::execute::create_swap_cosmos_msg;
use crate::msg::{cw20_adapter, external, Operation, Stage};
use crate::state::{Awaiting, Config, ReplyState, CONFIG, REPLY_STATES};
use cosmwasm_std::{
    to_json_binary, Addr, Coin, CosmosMsg, DepsMut, Env, Reply, Response, StdError, SubMsg,
    Uint128, WasmMsg,
};
use cw20::Cw20ExecuteMsg;
use cw20::{BalanceResponse as Cw20BalanceResponse, Cw20QueryMsg};
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};

pub fn handle_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut state = REPLY_STATES.load(deps.storage, msg.id)?;
    match state.awaiting {
        Awaiting::Swaps => handle_swap_reply(deps, env, msg, &mut state),
        Awaiting::Conversions => handle_conversion_reply(deps, env, msg, &mut state),
        Awaiting::FinalConversions => handle_final_conversion_reply(deps, env, msg, &mut state),
    }
}

fn handle_swap_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    state: &mut ReplyState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let master_reply_id = msg.id;

    let events = msg
        .result
        .clone()
        .into_result()
        .map_err(|e| ContractError::SubmessageResultError { error: e })?
        .events;
    let replying_addresses: Vec<&str> = events
        .iter()
        .filter(|e| e.ty.starts_with("wasm"))
        .flat_map(|e| &e.attributes)
        .filter_map(|attr| {
            if attr.key == "_contract_address" {
                Some(attr.value.as_str())
            } else {
                None
            }
        })
        .collect();
    let current_stage = &state.stages[state.current_stage_index as usize];
    let relevant_split = current_stage.splits.iter().find(|s| {
        let op_addr = match &s.operation { Operation::AmmSwap(op) => op.pool_address.as_str(), Operation::OrderbookSwap(op) => op.swap_contract.as_str() };
        replying_addresses.contains(&op_addr)
    }).ok_or_else(|| StdError::generic_err(format!("Could not find a split matching any replying contract. Contracts that replied: {:?}", replying_addresses)))?;

    let asset_info = get_operation_output(&relevant_split.operation)?;
    let amount = parse_amount_from_swap_reply(&msg)?;
    state.accumulated_assets.push(external::Asset {
        info: asset_info,
        amount,
    });
    state.replies_expected -= 1;

    if state.replies_expected > 0 {
        REPLY_STATES.save(deps.storage, master_reply_id, state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_swap_outputs"));
    }

    if state.current_stage_index as usize >= state.stages.len() - 1 {
        // This is the final stage, hand off to the dedicated final stage handler.
        return handle_final_stage(deps, env, msg.id, state);
    }

    // This is an intermediate stage, check for normalization.
    let config = CONFIG.load(deps.storage)?;
    let target_input_asset = get_next_stage_input_asset(&state.stages, state.current_stage_index)?;
    let mut conversion_submsgs = vec![];

    for asset in &state.accumulated_assets {
        if asset.info == target_input_asset {
            state.ready_for_next_stage_amount += asset.amount;
        } else {
            conversion_submsgs.push(SubMsg::reply_on_success(
                create_conversion_msg(asset, &config, &env)?,
                master_reply_id,
            ));
        }
    }
    state.accumulated_assets.clear();

    if conversion_submsgs.is_empty() {
        state.current_stage_index += 1;
        execute_next_swap_stage(deps, env, state, master_reply_id, target_input_asset)
    } else {
        state.awaiting = Awaiting::Conversions;
        state.replies_expected = conversion_submsgs.len() as u64;
        REPLY_STATES.save(deps.storage, master_reply_id, state)?;
        Ok(Response::new()
            .add_submessages(conversion_submsgs)
            .add_attribute("action", "normalizing_assets"))
    }
}

fn query_balance(
    deps: cosmwasm_std::Deps<InjectiveQueryWrapper>,
    env: &Env,
    asset_info: &external::AssetInfo,
) -> Result<Uint128, ContractError> {
    match asset_info {
        external::AssetInfo::NativeToken { denom } => {
            let balance = deps.querier.query_balance(&env.contract.address, denom)?;
            Ok(balance.amount)
        }
        external::AssetInfo::Token { contract_addr } => {
            let balance: Cw20BalanceResponse = deps.querier.query_wasm_smart(
                contract_addr,
                &Cw20QueryMsg::Balance {
                    address: env.contract.address.to_string(),
                },
            )?;
            Ok(balance.balance)
        }
    }
}

// A helper to create the final transfer message.
fn create_send_msg(
    recipient: &Addr,
    asset_info: &external::AssetInfo,
    amount: Uint128,
) -> Result<CosmosMsg<InjectiveMsgWrapper>, ContractError> {
    match asset_info {
        external::AssetInfo::NativeToken { denom } => {
            Ok(CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
                to_address: recipient.to_string(),
                amount: vec![Coin {
                    denom: denom.clone(),
                    amount,
                }],
            }))
        }
        external::AssetInfo::Token { contract_addr } => Ok(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: recipient.to_string(),
                amount,
            })?,
            funds: vec![],
        })),
    }
}

fn handle_final_stage(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    reply_id: u64,
    state: &mut ReplyState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let target_asset_info = state
        .accumulated_assets
        .iter()
        .find(|a| matches!(a.info, external::AssetInfo::Token { .. }))
        .map(|a| a.info.clone())
        .unwrap_or_else(|| state.accumulated_assets.first().unwrap().info.clone());

    let mut conversion_submsgs = vec![];
    let mut ready_amount = Uint128::zero();
    let config = CONFIG.load(deps.storage)?;
    for asset in &state.accumulated_assets {
        if asset.info == target_asset_info {
            ready_amount += asset.amount;
        } else {
            conversion_submsgs.push(SubMsg::reply_on_success(
                create_conversion_msg(asset, &config, &env)?,
                reply_id,
            ));
        }
    }

    if conversion_submsgs.is_empty() {
        // SCENARIO A: No conversion needed.
        let total_from_events: Uint128 = state.accumulated_assets.iter().map(|a| a.amount).sum();
        if total_from_events < state.minimum_receive {
            return Err(ContractError::MinimumReceiveNotMet {});
        }

        let forwardable_balance = query_balance(deps.as_ref(), &env, &target_asset_info)?;
        let mut response = Response::new();
        if !forwardable_balance.is_zero() {
            let send_msg = create_send_msg(&state.sender, &target_asset_info, forwardable_balance)?;
            response = response.add_message(send_msg);
        }

        REPLY_STATES.remove(deps.storage, reply_id);
        Ok(response
            .add_attribute("action", "aggregate_swap_complete")
            .add_attribute("final_received", total_from_events.to_string())
            .add_attribute("final_asset_type", format!("{:?}", target_asset_info)))
    } else {
        // SCENARIO B: Conversion is needed.
        state.awaiting = Awaiting::FinalConversions;
        state.replies_expected = conversion_submsgs.len() as u64;
        state.ready_for_next_stage_amount = ready_amount;
        state.accumulated_assets = vec![external::Asset {
            info: target_asset_info,
            amount: Uint128::zero(),
        }];
        REPLY_STATES.save(deps.storage, reply_id, state)?;
        Ok(Response::new()
            .add_submessages(conversion_submsgs)
            .add_attribute("action", "final_asset_normalization_started"))
    }
}

fn handle_final_conversion_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    state: &mut ReplyState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let reply_id = msg.id;
    state.ready_for_next_stage_amount += parse_amount_from_conversion_reply(&msg, &env)?;
    state.replies_expected -= 1;

    if state.replies_expected > 0 {
        REPLY_STATES.save(deps.storage, reply_id, state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_final_conversions"));
    }

    let total_final_amount = state.ready_for_next_stage_amount;
    let final_asset_info = state.accumulated_assets.first().unwrap().info.clone();

    if total_final_amount < state.minimum_receive {
        return Err(ContractError::MinimumReceiveNotMet {});
    }

    let forwardable_balance = query_balance(deps.as_ref(), &env, &final_asset_info)?;
    let mut response = Response::new();
    if !forwardable_balance.is_zero() {
        let send_msg = create_send_msg(&state.sender, &final_asset_info, forwardable_balance)?;
        response = response.add_message(send_msg);
    }

    REPLY_STATES.remove(deps.storage, reply_id);
    Ok(response
        .add_attribute("action", "aggregate_swap_complete")
        .add_attribute("final_received", total_final_amount.to_string())
        .add_attribute("final_asset_type", format!("{:?}", final_asset_info)))
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
    let offer_asset_info =
        get_next_stage_input_asset(&state.stages, state.current_stage_index - 1)?;
    execute_next_swap_stage(deps, env, state, master_reply_id, offer_asset_info)
}

fn execute_next_swap_stage(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    state: &mut ReplyState,
    reply_id: u64,
    offer_asset_info: external::AssetInfo,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let next_stage_idx = state.current_stage_index as usize;
    let next_stage = state
        .stages
        .get(next_stage_idx)
        .ok_or(ContractError::EmptyRoute {})?;
    let mut submessages = vec![];
    for split in &next_stage.splits {
        let amount_for_split = state
            .ready_for_next_stage_amount
            .multiply_ratio(split.percent as u128, 100u128);
        let msg = create_swap_cosmos_msg(
            &split.operation,
            &offer_asset_info,
            amount_for_split,
            &state.sender,
            &env,
            &state.stages,
            next_stage_idx,
        )?;
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
                msg: to_json_binary(&cw20_adapter::ReceiveSubmsg {
                    recipient: env.contract.address.to_string(),
                })?,
            };
            Ok(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&send_msg)?,
                funds: vec![],
            }))
        }
        // Convert Native -> CW20
        external::AssetInfo::NativeToken { denom } => Ok(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: config.cw20_adapter_address.to_string(),
            msg: to_json_binary(&cw20_adapter::ExecuteMsg::RedeemAndTransfer {
                recipient: Some(env.contract.address.to_string()),
            })?,
            funds: vec![Coin {
                denom: denom.clone(),
                amount: from.amount,
            }],
        })),
    }
}

fn get_operation_output(op: &Operation) -> Result<external::AssetInfo, ContractError> {
    Ok(match op {
        Operation::AmmSwap(o) => o.ask_asset_info.clone(),
        Operation::OrderbookSwap(o) => o.ask_asset_info.clone(),
    })
}

fn get_next_stage_input_asset(
    stages: &[Stage],
    current_stage_index: u64,
) -> Result<external::AssetInfo, ContractError> {
    let next_stage = stages
        .get((current_stage_index + 1) as usize)
        .ok_or(ContractError::EmptyRoute {})?;
    let op = &next_stage
        .splits
        .first()
        .ok_or(ContractError::EmptyRoute {})?
        .operation;
    Ok(match op {
        Operation::AmmSwap(o) => o.offer_asset_info.clone(),
        Operation::OrderbookSwap(o) => o.offer_asset_info.clone(),
    })
}

fn parse_amount_from_swap_reply(msg: &Reply) -> Result<Uint128, ContractError> {
    let events = msg
        .result
        .clone()
        .into_result()
        .map_err(|e| ContractError::SubmessageResultError { error: e })?
        .events;
    let amount_str = events
        .iter()
        .find_map(|event| {
            if !event.ty.starts_with("wasm") {
                return None;
            }
            let key = if event.ty == "wasm-atomic_swap_execution" {
                "swap_final_amount"
            } else {
                "return_amount"
            };
            event
                .attributes
                .iter()
                .find(|attr| attr.key == key)
                .map(|attr| attr.value.clone())
        })
        .ok_or(ContractError::NoAmountInReply {})?;
    amount_str
        .parse::<Uint128>()
        .map_err(|_| ContractError::MalformedAmountInReply { value: amount_str })
}

fn parse_amount_from_conversion_reply(msg: &Reply, env: &Env) -> Result<Uint128, ContractError> {
    let events = &msg
        .result
        .clone()
        .into_result()
        .map_err(|e| ContractError::SubmessageResultError { error: e })?
        .events;

    if let Some(transfer_event) = events.iter().find(|e| {
        e.ty == "transfer"
            && e.attributes
                .iter()
                .any(|a| a.key == "recipient" && a.value == env.contract.address.to_string())
    }) {
        let amount_attr = transfer_event
            .attributes
            .iter()
            .find(|a| a.key == "amount")
            .ok_or(ContractError::NoAmountInReply {})?;

        let amount_val = amount_attr
            .value
            .trim_end_matches(|c: char| !c.is_ascii_digit());
        return amount_val
            .parse::<Uint128>()
            .map_err(|_| ContractError::MalformedAmountInReply {
                value: amount_attr.value.clone(),
            });
    }

    if let Some(wasm_event) = events.iter().find(|e| {
        e.ty.starts_with("wasm")
            && e.attributes
                .iter()
                .any(|a| a.key == "action" && a.value == "transfer")
    }) {
        let amount_attr = wasm_event
            .attributes
            .iter()
            .find(|a| a.key == "amount")
            .ok_or(ContractError::NoAmountInReply {})?;

        return amount_attr.value.parse::<Uint128>().map_err(|_| {
            ContractError::MalformedAmountInReply {
                value: amount_attr.value.clone(),
            }
        });
    }

    Err(ContractError::NoConversionEventInReply {})
}
