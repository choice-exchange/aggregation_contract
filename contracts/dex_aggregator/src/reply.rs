use crate::error::ContractError;
use crate::execute::create_swap_cosmos_msg;
use crate::msg::{cw20_adapter, external, Operation, PlannedSwap, Stage, StagePlan};
use crate::state::{Awaiting, Config, ReplyState, CONFIG, FEE_MAP, REPLY_STATES};
use cosmwasm_std::{
    to_json_binary, Addr, Coin, CosmosMsg, DepsMut, Env, Reply, Response, StdError, SubMsg,
    Uint128, WasmMsg,
};
use cw20::Cw20ExecuteMsg;
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

pub(crate) fn proceed_to_next_step(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: Env,
    state: &mut ReplyState,
    master_reply_id: u64,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if state.current_stage_index as usize >= state.stages.len() {
        return handle_final_stage(deps, env, master_reply_id, state);
    }

    let next_stage_to_execute = state
        .stages
        .get(state.current_stage_index as usize)
        .unwrap();
    let plan = plan_next_stage(&state.accumulated_assets, next_stage_to_execute)?;
    state.accumulated_assets.clear();

    if plan.conversions_needed.is_empty() {
        execute_planned_swaps(deps, env, state, master_reply_id, plan.swaps_to_execute)
    } else {
        let config = CONFIG.load(deps.storage)?;
        let mut conversion_submsgs = vec![];
        for (asset_to_convert, _target_info) in &plan.conversions_needed {
            let msg = create_conversion_msg(asset_to_convert, &config, &env)?;
            conversion_submsgs.push(SubMsg::reply_on_success(msg, master_reply_id));
        }
        state.awaiting = Awaiting::Conversions;
        state.replies_expected = conversion_submsgs.len() as u64;
        state.pending_swaps = plan.swaps_to_execute; // Save the plan for after conversion
        REPLY_STATES.save(deps.storage, master_reply_id, state)?;
        Ok(Response::new()
            .add_submessages(conversion_submsgs)
            .add_attribute("action", "performing_minimal_conversions"))
    }
}

fn handle_swap_reply(
    mut deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    state: &mut ReplyState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let master_reply_id = msg.id;

    let amount = parse_amount_from_swap_reply(&msg)?;
    let asset_info = find_asset_info_from_swap_reply(&msg, state)?;
    let pool_addr = find_replying_pool_addr(&msg)?; // Get the address of the pool that replied

    // --- FEE LOGIC ---
    let fee = match FEE_MAP.may_load(deps.storage, &pool_addr)? {
        Some(fee_percent) => {
            let numerator = fee_percent.atomics();
            let denominator = Uint128::new(1_000_000_000_000_000_000u128);
            amount.multiply_ratio(numerator, denominator)
        }
        None => Uint128::zero(),
    };

    let amount_after_fee = amount.checked_sub(fee).map_err(StdError::from)?;
    // --- END FEE LOGIC ---

    state.accumulated_assets.push(external::Asset {
        info: asset_info.clone(),
        amount: amount_after_fee,
    });
    state.replies_expected -= 1;

    // Prepare the response. We will either hand off to proceed_to_next_step
    // or return a simple accumulating response.
    let mut response;

    if state.replies_expected > 0 {
        REPLY_STATES.save(deps.storage, master_reply_id, state)?;
        response = Response::new().add_attribute("action", "accumulating_swap_outputs");
    } else {
        state.current_stage_index += 1;
        response = proceed_to_next_step(&mut deps, env, state, master_reply_id)?;
    }

    // If a fee was calculated, create and add the message to send it.
    if !fee.is_zero() {
        let config = CONFIG.load(deps.storage)?;
        let fee_send_msg = create_send_msg(&config.fee_collector, &asset_info, fee)?;
        response = response
            .add_message(fee_send_msg)
            .add_attribute("fee_collected", fee.to_string())
            .add_attribute("fee_pool", pool_addr.to_string());
    }

    Ok(response)
}

fn find_replying_pool_addr(msg: &Reply) -> Result<Addr, ContractError> {
    let events = &msg
        .result
        .clone()
        .into_result()
        .map_err(|e| ContractError::SubmessageResultError { error: e })?
        .events;

    // Find all potential contract addresses from the events.
    let mut potential_addrs = vec![];
    for e in events.iter().filter(|e| e.ty.starts_with("wasm")) {
        if let Some(addr) = e.attributes.iter().find(|a| a.key == "_contract_address") {
            potential_addrs.push(addr.value.clone());
        }
        if let Some(addr) = e.attributes.iter().find(|a| a.key == "sender") {
            potential_addrs.push(addr.value.clone());
        }
    }

    // Injective queries often use `_contract_address`, which is the most reliable.
    // We assume the first valid address we find is the one we want.
    // This could be made more robust if needed.
    let addr_str = potential_addrs.first().ok_or_else(|| {
        StdError::generic_err("Could not find any replying contract address in wasm events")
    })?;

    // We don't have access to `deps` here to validate, so we return the string to the caller.
    // Correction: Let's pass deps in to validate immediately.
    // The caller `handle_swap_reply` has deps. Let's do it there.
    Ok(Addr::unchecked(addr_str)) // Use unchecked for now, validate in caller.
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
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: Env,
    reply_id: u64,
    state: &mut ReplyState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    // If there are no assets, we're done.
    if state.accumulated_assets.is_empty() {
        if !state.minimum_receive.is_zero() {
            return Err(ContractError::MinimumReceiveNotMet {});
        }
        REPLY_STATES.remove(deps.storage, reply_id);
        return Ok(Response::new().add_attribute("action", "aggregate_swap_complete_empty"));
    }

    // The target asset for normalization is the type of the first asset in the final list.
    let target_asset_info = state.accumulated_assets[0].info.clone();

    let mut conversion_submsgs = vec![];
    let mut ready_amount = Uint128::zero();
    let config = CONFIG.load(deps.storage)?;

    for asset in &state.accumulated_assets {
        if asset.info == target_asset_info {
            ready_amount += asset.amount;
        } else {
            // This asset needs to be converted to the target type.
            let msg = create_conversion_msg(asset, &config, &env)?;
            conversion_submsgs.push(SubMsg::reply_on_success(msg, reply_id));
        }
    }

    if conversion_submsgs.is_empty() {
        // SCENARIO A: All assets were already the same type. We are done.
        let total_final_amount = ready_amount;
        if total_final_amount < state.minimum_receive {
            return Err(ContractError::MinimumReceiveNotMet {});
        }

        let mut response = Response::new();
        // Only create and add the send message if there is a non-zero amount to send.
        if !total_final_amount.is_zero() {
            let send_msg = create_send_msg(&state.sender, &target_asset_info, total_final_amount)?;
            response = response.add_message(send_msg);
        }

        REPLY_STATES.remove(deps.storage, reply_id);
        Ok(response
            .add_attribute("action", "aggregate_swap_complete")
            .add_attribute("final_received", total_final_amount.to_string()))
    } else {
        // SCENARIO B: Conversions are needed. Set up the state for the final reply.
        state.awaiting = Awaiting::FinalConversions;
        state.replies_expected = conversion_submsgs.len() as u64;

        // CRITICAL: The `accumulated_assets` now stores our in-progress total.
        // It contains a single entry representing the assets that are already the target type.
        state.accumulated_assets = vec![external::Asset {
            info: target_asset_info,
            amount: ready_amount,
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
    let converted_amount = parse_amount_from_conversion_reply(&msg, &env)?;

    let running_total_asset = state.accumulated_assets.get_mut(0).ok_or_else(|| {
        StdError::generic_err("Final conversion state is invalid: no accumulated asset found")
    })?;

    running_total_asset.amount += converted_amount;
    state.replies_expected -= 1;

    if state.replies_expected > 0 {
        // Still waiting for more conversions to finish.
        REPLY_STATES.save(deps.storage, reply_id, state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_final_conversions"));
    }

    // All final conversions are complete.
    let total_final_amount = running_total_asset.amount;
    let final_asset_info = running_total_asset.info.clone();

    if total_final_amount < state.minimum_receive {
        return Err(ContractError::MinimumReceiveNotMet {});
    }

    let mut response = Response::new();
    // Only create and add the send message if there is a non-zero amount to send.
    if !total_final_amount.is_zero() {
        let send_msg = create_send_msg(&state.sender, &final_asset_info, total_final_amount)?;
        response = response.add_message(send_msg);
    }

    REPLY_STATES.remove(deps.storage, reply_id);
    Ok(response
        .add_attribute("action", "aggregate_swap_complete")
        .add_attribute("final_received", total_final_amount.to_string()))
}

fn handle_conversion_reply(
    mut deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    state: &mut ReplyState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let master_reply_id = msg.id;
    // Note: for this new architecture, we don't need to accumulate converted assets.
    // The planner has already accounted for them. We just need to wait for all conversions to finish.
    state.replies_expected -= 1;

    if state.replies_expected > 0 {
        REPLY_STATES.save(deps.storage, master_reply_id, state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_conversion_outputs"));
    }

    // All conversions are done. Take the saved plan and execute it.
    let swaps_to_execute = std::mem::take(&mut state.pending_swaps);
    execute_planned_swaps(&mut deps, env, state, master_reply_id, swaps_to_execute)
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

fn parse_amount_from_swap_reply(msg: &Reply) -> Result<Uint128, ContractError> {
    let events = msg
        .result
        .clone()
        .into_result()
        .map_err(|e| ContractError::SubmessageResultError { error: e })?
        .events;

    let amount_str_opt = events // Changed to an Option
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
        });

    // --- THIS IS THE FIX ---
    match amount_str_opt {
        // If we found the attribute, parse it as before.
        Some(amount_str) => amount_str
            .parse::<Uint128>()
            .map_err(|_| ContractError::MalformedAmountInReply { value: amount_str }),
        // If we did NOT find the attribute, it means the return was 0. Return Ok(0).
        None => Ok(Uint128::zero()),
    }
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

        let numeric_part =
            if let Some(first_non_digit) = amount_attr.value.find(|c: char| !c.is_ascii_digit()) {
                &amount_attr.value[..first_non_digit]
            } else {
                &amount_attr.value
            };

        return numeric_part.parse::<Uint128>().map_err(|_| {
            ContractError::MalformedAmountInReply {
                value: amount_attr.value.clone(),
            }
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

fn find_asset_info_from_swap_reply(
    msg: &Reply,
    state: &ReplyState,
) -> Result<external::AssetInfo, ContractError> {
    let events = &msg
        .result
        .clone()
        .into_result()
        .map_err(|e| ContractError::SubmessageResultError { error: e })?
        .events;

    // Find all potential contract addresses from the events.
    let mut potential_addrs = vec![];
    for e in events.iter().filter(|e| e.ty.starts_with("wasm")) {
        // The address of the contract that was executed.
        if let Some(addr) = e.attributes.iter().find(|a| a.key == "_contract_address") {
            potential_addrs.push(&addr.value);
        }
        // For Cw20::Send, the `sender` of the hook message is the pool.
        if let Some(addr) = e.attributes.iter().find(|a| a.key == "sender") {
            potential_addrs.push(&addr.value);
        }
    }

    let current_stage = state
        .stages
        .get(state.current_stage_index as usize)
        .ok_or(ContractError::EmptyRoute {})?;

    // Find the split that matches any of the potential addresses.
    let relevant_split = current_stage
        .splits
        .iter()
        .find(|s| {
            let op_addr = match &s.operation {
                Operation::AmmSwap(op) => &op.pool_address,
                Operation::OrderbookSwap(op) => &op.swap_contract,
            };
            potential_addrs.contains(&op_addr)
        })
        .ok_or_else(|| {
            StdError::generic_err(format!(
                "Could not find a split matching any replying contract addresses: {:?}",
                potential_addrs
            ))
        })?;

    get_operation_output(&relevant_split.operation)
}

fn plan_next_stage(
    accumulated_assets: &[external::Asset],
    next_stage: &Stage,
) -> Result<StagePlan, ContractError> {
    let mut native_info: Option<external::AssetInfo> = None;
    let mut cw20_info: Option<external::AssetInfo> = None;

    for split in &next_stage.splits {
        let offer_info = get_operation_input(&split.operation)?;
        match offer_info {
            external::AssetInfo::NativeToken { .. } => {
                native_info = Some(offer_info);
            }
            external::AssetInfo::Token { .. } => {
                cw20_info = Some(offer_info);
            }
        }
    }

    let mut native_have = Uint128::zero();
    let mut cw20_have = Uint128::zero();
    for asset in accumulated_assets {
        match &asset.info {
            external::AssetInfo::NativeToken { .. } => {
                native_have += asset.amount;
                if native_info.is_none() {
                    native_info = Some(asset.info.clone());
                }
            }
            external::AssetInfo::Token { .. } => {
                cw20_have += asset.amount;
                if cw20_info.is_none() {
                    cw20_info = Some(asset.info.clone());
                }
            }
        }
    }
    let total_logical_amount = native_have + cw20_have;

    let mut swaps_to_execute: Vec<PlannedSwap> = vec![];
    let mut total_native_needs = Uint128::zero();
    let mut total_cw20_needs = Uint128::zero();

    // First pass: Calculate total needs for each asset form.
    for split in &next_stage.splits {
        let amount_for_split = total_logical_amount.multiply_ratio(split.percent as u128, 100u128);
        let offer_info = get_operation_input(&split.operation)?;
        match offer_info {
            external::AssetInfo::NativeToken { .. } => total_native_needs += amount_for_split,
            external::AssetInfo::Token { .. } => total_cw20_needs += amount_for_split,
        }
    }

    // Determine conversions needed.
    let mut conversions_needed: Vec<(external::Asset, external::AssetInfo)> = vec![];
    if native_have > total_native_needs {
        if let Some(target_info) = &cw20_info {
            conversions_needed.push((
                external::Asset {
                    info: native_info.clone().unwrap(),
                    amount: native_have - total_native_needs,
                },
                target_info.clone(),
            ));
        }
    }
    if cw20_have > total_cw20_needs {
        if let Some(target_info) = &native_info {
            conversions_needed.push((
                external::Asset {
                    info: cw20_info.clone().unwrap(),
                    amount: cw20_have - total_cw20_needs,
                },
                target_info.clone(),
            ));
        }
    }

    // Second pass: Create the concrete swap plan using the remainder method.
    let mut native_allocated = Uint128::zero();
    let mut cw20_allocated = Uint128::zero();
    for (i, split) in next_stage.splits.iter().enumerate() {
        let offer_info = get_operation_input(&split.operation)?;
        let amount_for_split = if i < next_stage.splits.len() - 1 {
            total_logical_amount.multiply_ratio(split.percent as u128, 100u128)
        } else {
            let already_allocated = native_allocated + cw20_allocated;
            total_logical_amount
                .checked_sub(already_allocated)
                .map_err(StdError::from)?
        };

        match offer_info {
            external::AssetInfo::NativeToken { .. } => native_allocated += amount_for_split,
            external::AssetInfo::Token { .. } => cw20_allocated += amount_for_split,
        }

        swaps_to_execute.push(PlannedSwap {
            operation: split.operation.clone(),
            amount: amount_for_split,
        });
    }

    Ok(StagePlan {
        swaps_to_execute,
        conversions_needed,
    })
}

fn get_operation_input(op: &Operation) -> Result<external::AssetInfo, ContractError> {
    Ok(match op {
        Operation::AmmSwap(o) => o.offer_asset_info.clone(),
        Operation::OrderbookSwap(o) => o.offer_asset_info.clone(),
    })
}

fn execute_planned_swaps(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: Env,
    state: &mut ReplyState,
    reply_id: u64,
    swaps: Vec<PlannedSwap>, // Takes the concrete plan
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut submessages = vec![];

    // Filter out zero-amount swaps before creating messages ---
    for swap in swaps.into_iter().filter(|s| !s.amount.is_zero()) {
        let offer_asset_info = get_operation_input(&swap.operation)?;
        let msg =
            create_swap_cosmos_msg(deps, &swap.operation, &offer_asset_info, swap.amount, &env)?;
        submessages.push(SubMsg::reply_on_success(msg, reply_id));
    }

    // If all swaps were zero-amount, we might not have any messages.
    // We need to handle this case by proceeding directly to the next step.
    if submessages.is_empty() {
        state.current_stage_index += 1;
        // We pass `deps` by mutable reference.
        return proceed_to_next_step(deps, env, state, reply_id);
    }

    state.awaiting = Awaiting::Swaps;
    state.replies_expected = submessages.len() as u64;
    REPLY_STATES.save(deps.storage, reply_id, state)?;

    Ok(Response::new()
        .add_submessages(submessages)
        .add_attribute("action", "executing_planned_swaps")
        .add_attribute("stage_index", state.current_stage_index.to_string()))
}
