use crate::error::ContractError;
use crate::execute::create_swap_cosmos_msg;
use crate::msg::{amm, cw20_adapter, Operation, PlannedSwap, Stage, StagePlan};
use crate::state::{
    Awaiting, Config, ExecutionState, PendingPathOp, RoutePlan, CONFIG, EXECUTION_STATES, FEE_MAP,
    ROUTE_PLANS,
};
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
    let reply_id = msg.id;
    let mut exec_state = EXECUTION_STATES.load(deps.storage, reply_id)?;
    let plan = ROUTE_PLANS.load(deps.storage, reply_id)?;

    match exec_state.awaiting {
        Awaiting::Swaps => handle_swap_reply(deps, env, msg, &mut exec_state, &plan),
        Awaiting::Conversions => handle_conversion_reply(deps, env, msg, &mut exec_state, &plan),
        Awaiting::FinalConversions => {
            handle_final_conversion_reply(deps, env, msg, &mut exec_state, &plan)
        }
        Awaiting::PathConversion => {
            handle_path_conversion_reply(deps, env, msg, &mut exec_state, &plan)
        }
    }
}

pub(crate) fn proceed_to_next_step(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: Env,
    exec_state: &mut ExecutionState,
    plan: &RoutePlan,
    master_reply_id: u64,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if exec_state.current_stage_index as usize >= plan.stages.len() {
        return handle_final_stage(deps, env, master_reply_id, exec_state, plan);
    }

    let next_stage_to_execute = plan
        .stages
        .get(exec_state.current_stage_index as usize)
        .unwrap();

    let stage_plan = plan_next_stage(&exec_state.accumulated_assets, next_stage_to_execute)?;
    exec_state.accumulated_assets.clear();

    if stage_plan.conversions_needed.is_empty() {
        execute_planned_swaps(
            deps,
            env,
            exec_state,
            plan,
            master_reply_id,
            stage_plan.swaps_to_execute,
        )
    } else {
        let config = CONFIG.load(deps.storage)?;
        let mut conversion_submsgs = vec![];
        for (asset_to_convert, _target_info) in &stage_plan.conversions_needed {
            let msg = create_conversion_msg(asset_to_convert, &config, &env)?;
            conversion_submsgs.push(SubMsg::reply_on_success(msg, master_reply_id));
        }

        exec_state.awaiting = Awaiting::Conversions;
        exec_state.replies_expected = conversion_submsgs.len() as u64;
        exec_state.pending_swaps = stage_plan.swaps_to_execute;

        EXECUTION_STATES.save(deps.storage, master_reply_id, exec_state)?;

        Ok(Response::new()
            .add_submessages(conversion_submsgs)
            .add_attribute("action", "performing_minimal_conversions"))
    }
}

fn handle_swap_reply(
    mut deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    exec_state: &mut ExecutionState,
    plan: &RoutePlan,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let master_reply_id = msg.id;

    let events = &msg
        .result
        .clone()
        .into_result()
        .map_err(|e| ContractError::SubmessageResultError { error: e })?
        .events;

    // Find the specific DEX event. This is our source of truth for the amount.
    let swap_event_opt = events.iter().rev().find(|e| {
        e.ty.starts_with("wasm")
            && (e.attributes.iter().any(|a| a.key == "return_amount")
                || e.attributes.iter().any(|a| a.key == "swap_final_amount"))
    });

    // If there is no swap event, we assume the output was zero.
    // In this case, the path cannot continue, so we treat it as "complete" with a zero value.
    if swap_event_opt.is_none() {
        exec_state.replies_expected -= 1; // Mutate exec_state

        let response = if exec_state.replies_expected > 0 {
            EXECUTION_STATES.save(deps.storage, master_reply_id, exec_state)?; // Save exec_state
            Response::new()
                .add_attribute("action", "accumulating_path_outputs")
                .add_attribute("info", "zero_value_path_completed")
        } else {
            exec_state.current_stage_index += 1; // Mutate exec_state
                                                 // Call proceeds with exec_state and plan
            proceed_to_next_step(&mut deps, env, exec_state, plan, master_reply_id)?
        };
        return Ok(response);
    }

    let swap_event = swap_event_opt.unwrap();

    // Get the address of the contract that emitted this specific event.
    let replying_pool_addr_str = swap_event
        .attributes
        .iter()
        .find(|a| a.key == "_contract_address")
        .map(|a| a.value.clone())
        .ok_or_else(|| StdError::generic_err("Swap result event is missing '_contract_address'"))?;

    let replying_pool_addr = deps.api.addr_validate(&replying_pool_addr_str)?;

    let current_stage = plan
        .stages
        .get(exec_state.current_stage_index as usize)
        .ok_or(ContractError::EmptyRoute {})?;

    // Now, find the operation that matches this validated address.
    let mut replied_path_info = None;
    'outer: for (split_idx, split) in current_stage.splits.iter().enumerate() {
        for (op_idx, op) in split.path.iter().enumerate() {
            if get_operation_address(op) == replying_pool_addr.as_str() {
                replied_path_info = Some(((split_idx, op_idx), op));
                break 'outer;
            }
        }
    }

    let ((split_index, op_index), replied_op) = replied_path_info.ok_or_else(|| {
        StdError::generic_err(format!(
            "Could not find a split/operation matching the replying contract: {}",
            replying_pool_addr
        ))
    })?;

    // Since we know the event exists, we can now safely parse the amount from the original message.
    let received_amount = parse_amount_from_swap_reply(&msg)?;
    let received_asset_info = get_operation_output(replied_op)?;

    let replied_path = &current_stage.splits[split_index].path;

    if let Some(next_op) = replied_path.get(op_index + 1) {
        let required_input_info = get_operation_input(next_op)?;
        let offer_asset_for_next_op = amm::Asset {
            info: received_asset_info,
            amount: received_amount,
        };
        if offer_asset_for_next_op.info != required_input_info {
            exec_state.awaiting = Awaiting::PathConversion; // Mutate exec_state
            exec_state.pending_path_op = Some(PendingPathOp {
                // Mutate exec_state
                operation: next_op.clone(),
                amount: received_amount,
            });
            let config = CONFIG.load(deps.storage)?;
            let conversion_msg = create_conversion_msg(&offer_asset_for_next_op, &config, &env)?;
            let sub_msg = SubMsg::reply_on_success(conversion_msg, master_reply_id);
            EXECUTION_STATES.save(deps.storage, master_reply_id, exec_state)?; // Save exec_state
            return Ok(Response::new()
                .add_submessage(sub_msg)
                .add_attribute("action", "performing_path_conversion"));
        }
        let next_msg = create_swap_cosmos_msg(
            &mut deps,
            next_op,
            &offer_asset_for_next_op.info,
            offer_asset_for_next_op.amount,
            &env,
        )?;
        let sub_msg = SubMsg::reply_on_success(next_msg, master_reply_id);
        EXECUTION_STATES.save(deps.storage, master_reply_id, exec_state)?; // Save exec_state
        Ok(Response::new()
            .add_submessage(sub_msg)
            .add_attribute("action", "proceeding_to_next_op_in_path")
            .add_attribute("split_index", split_index.to_string())
            .add_attribute("op_index", (op_index + 1).to_string()))
    } else {
        let fee = match FEE_MAP.may_load(deps.storage, &replying_pool_addr)? {
            Some(fee_percent) => {
                let numerator = fee_percent.atomics();
                let denominator = Uint128::new(1_000_000_000_000_000_000u128);
                received_amount.multiply_ratio(numerator, denominator)
            }
            None => Uint128::zero(),
        };
        let amount_after_fee = received_amount.checked_sub(fee).map_err(StdError::from)?;
        exec_state.accumulated_assets.push(amm::Asset {
            // Mutate exec_state
            info: received_asset_info.clone(),
            amount: amount_after_fee,
        });
        exec_state.replies_expected -= 1;
        let mut response;
        if exec_state.replies_expected > 0 {
            EXECUTION_STATES.save(deps.storage, master_reply_id, exec_state)?; // Save exec_state
            response = Response::new().add_attribute("action", "accumulating_path_outputs");
        } else {
            exec_state.current_stage_index += 1; // Mutate exec_state
            response = proceed_to_next_step(&mut deps, env, exec_state, plan, master_reply_id)?;
        }
        if !fee.is_zero() {
            let config = CONFIG.load(deps.storage)?;
            let fee_send_msg = create_send_msg(&config.fee_collector, &received_asset_info, fee)?;
            response = response
                .add_message(fee_send_msg)
                .add_attribute("fee_collected", fee.to_string())
                .add_attribute("fee_pool", replying_pool_addr.to_string());
        }
        Ok(response)
    }
}

// A helper to create the final transfer message.
fn create_send_msg(
    recipient: &Addr,
    asset_info: &amm::AssetInfo,
    amount: Uint128,
) -> Result<CosmosMsg<InjectiveMsgWrapper>, ContractError> {
    match asset_info {
        amm::AssetInfo::NativeToken { denom } => Ok(CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address: recipient.to_string(),
            amount: vec![Coin {
                denom: denom.clone(),
                amount,
            }],
        })),
        amm::AssetInfo::Token { contract_addr } => Ok(CosmosMsg::Wasm(WasmMsg::Execute {
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
    exec_state: &mut ExecutionState,
    plan: &RoutePlan,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if exec_state.accumulated_assets.is_empty() {
        if !plan.minimum_receive.is_zero() {
            return Err(ContractError::MinimumReceiveNotMet {});
        }
        // CLEANUP HERE
        EXECUTION_STATES.remove(deps.storage, reply_id);
        ROUTE_PLANS.remove(deps.storage, reply_id);
        return Ok(Response::new().add_attribute("action", "aggregate_swap_complete_empty"));
    }

    // The target asset for normalization is the type of the first asset in the final list.
    let target_asset_info = exec_state.accumulated_assets[0].info.clone();

    let mut conversion_submsgs = vec![];
    let mut ready_amount = Uint128::zero();
    let config = CONFIG.load(deps.storage)?;

    for asset in &exec_state.accumulated_assets {
        if asset.info == target_asset_info {
            ready_amount += asset.amount;
        } else {
            let msg = create_conversion_msg(asset, &config, &env)?;
            conversion_submsgs.push(SubMsg::reply_on_success(msg, reply_id));
        }
    }

    if conversion_submsgs.is_empty() {
        // SCENARIO A: All assets were already the same type. We are done.
        let total_final_amount = ready_amount;
        // Check against minimum_receive from the immutable plan
        if total_final_amount < plan.minimum_receive {
            return Err(ContractError::MinimumReceiveNotMet {});
        }

        let mut response = Response::new();
        if !total_final_amount.is_zero() {
            // Use the sender address from the immutable plan
            let send_msg = create_send_msg(&plan.sender, &target_asset_info, total_final_amount)?;
            response = response.add_message(send_msg);
        }

        EXECUTION_STATES.remove(deps.storage, reply_id);
        ROUTE_PLANS.remove(deps.storage, reply_id);

        // State cleanup is now handled in the main `handle_reply` function
        Ok(response
            .add_attribute("action", "aggregate_swap_complete")
            .add_attribute("final_received", total_final_amount.to_string()))
    } else {
        // SCENARIO B: Conversions are needed. Set up the exec_state for the final reply.
        exec_state.awaiting = Awaiting::FinalConversions;
        exec_state.replies_expected = conversion_submsgs.len() as u64;
        exec_state.accumulated_assets = vec![amm::Asset {
            info: target_asset_info,
            amount: ready_amount,
        }];

        // Save the small, mutated exec_state
        EXECUTION_STATES.save(deps.storage, reply_id, exec_state)?;

        Ok(Response::new()
            .add_submessages(conversion_submsgs)
            .add_attribute("action", "final_asset_normalization_started"))
    }
}

fn handle_final_conversion_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    exec_state: &mut ExecutionState,
    plan: &RoutePlan,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let reply_id = msg.id;
    let converted_amount = parse_amount_from_conversion_reply(&msg, &env)?;

    let running_total_asset = exec_state.accumulated_assets.get_mut(0).ok_or_else(|| {
        StdError::generic_err("Final conversion state is invalid: no accumulated asset found")
    })?;

    running_total_asset.amount += converted_amount;
    exec_state.replies_expected -= 1;

    if exec_state.replies_expected > 0 {
        // Still waiting for more conversions to finish. Save the updated exec_state.
        EXECUTION_STATES.save(deps.storage, reply_id, exec_state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_final_conversions"));
    }

    // All final conversions are complete.
    let total_final_amount = running_total_asset.amount;
    let final_asset_info = running_total_asset.info.clone();

    if total_final_amount < plan.minimum_receive {
        return Err(ContractError::MinimumReceiveNotMet {});
    }

    let mut response = Response::new();
    if !total_final_amount.is_zero() {
        // Get the sender address from the immutable plan
        let send_msg = create_send_msg(&plan.sender, &final_asset_info, total_final_amount)?;
        response = response.add_message(send_msg);
    }

    EXECUTION_STATES.remove(deps.storage, reply_id);
    ROUTE_PLANS.remove(deps.storage, reply_id);

    // State cleanup is now handled in the main `handle_reply` function
    Ok(response
        .add_attribute("action", "aggregate_swap_complete")
        .add_attribute("final_received", total_final_amount.to_string()))
}

fn handle_conversion_reply(
    mut deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    exec_state: &mut ExecutionState,
    plan: &RoutePlan,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let master_reply_id = msg.id;
    exec_state.replies_expected -= 1; // Mutate exec_state

    if exec_state.replies_expected > 0 {
        // Save the small, mutated exec_state
        EXECUTION_STATES.save(deps.storage, master_reply_id, exec_state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_conversion_outputs"));
    }

    // Take pending_swaps from the mutated exec_state
    let swaps_to_execute = std::mem::take(&mut exec_state.pending_swaps);

    // Call the updated execute_planned_swaps with both state objects
    execute_planned_swaps(
        &mut deps,
        env,
        exec_state,
        plan,
        master_reply_id,
        swaps_to_execute,
    )
}

fn create_conversion_msg(
    from: &amm::Asset,
    config: &Config,
    env: &Env,
) -> Result<CosmosMsg<InjectiveMsgWrapper>, ContractError> {
    match &from.info {
        // Convert CW20 -> Native
        amm::AssetInfo::Token { contract_addr } => {
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
        amm::AssetInfo::NativeToken { denom } => Ok(CosmosMsg::Wasm(WasmMsg::Execute {
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

fn get_operation_output(op: &Operation) -> Result<amm::AssetInfo, ContractError> {
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

    match amount_str_opt {
        Some(amount_str) => amount_str
            .parse::<Uint128>()
            .map_err(|_| ContractError::MalformedAmountInReply { value: amount_str }),
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

fn plan_next_stage(
    accumulated_assets: &[amm::Asset],
    next_stage: &Stage,
) -> Result<StagePlan, ContractError> {
    let mut native_info: Option<amm::AssetInfo> = None;
    let mut cw20_info: Option<amm::AssetInfo> = None;

    for split in &next_stage.splits {
        let first_op = split.path.first().ok_or(ContractError::EmptyRoute {})?;
        let offer_info = get_operation_input(first_op)?;
        match offer_info {
            amm::AssetInfo::NativeToken { .. } => {
                if native_info.is_none() {
                    native_info = Some(offer_info);
                }
            }
            amm::AssetInfo::Token { .. } => {
                if cw20_info.is_none() {
                    cw20_info = Some(offer_info);
                }
            }
        }
    }

    let mut native_have = Uint128::zero();
    let mut cw20_have = Uint128::zero();
    for asset in accumulated_assets {
        match &asset.info {
            amm::AssetInfo::NativeToken { .. } => {
                native_have += asset.amount;
            }
            amm::AssetInfo::Token { .. } => {
                cw20_have += asset.amount;
            }
        }
    }
    let total_logical_amount = native_have + cw20_have;

    let mut total_native_needs = Uint128::zero();
    let mut total_cw20_needs = Uint128::zero();
    for split in &next_stage.splits {
        let amount_for_split = total_logical_amount.multiply_ratio(split.percent as u128, 100u128);
        let first_op = split.path.first().ok_or(ContractError::EmptyRoute {})?;
        let offer_info = get_operation_input(first_op)?;
        match offer_info {
            amm::AssetInfo::NativeToken { .. } => total_native_needs += amount_for_split,
            amm::AssetInfo::Token { .. } => total_cw20_needs += amount_for_split,
        }
    }

    let mut conversions_needed: Vec<(amm::Asset, amm::AssetInfo)> = vec![];
    if native_have > total_native_needs {
        if let Some(target_info) = &cw20_info {
            let native_asset_to_convert_info = accumulated_assets
                .iter()
                .find(|a| matches!(a.info, amm::AssetInfo::NativeToken { .. }))
                .map(|a| a.info.clone())
                .ok_or_else(|| {
                    StdError::generic_err(
                        "State inconsistency: have native amount but no native asset info found",
                    )
                })?;

            conversions_needed.push((
                amm::Asset {
                    info: native_asset_to_convert_info,
                    amount: native_have - total_native_needs,
                },
                target_info.clone(),
            ));
        }
    }
    if cw20_have > total_cw20_needs {
        if let Some(target_info) = &native_info {
            let cw20_asset_to_convert_info = accumulated_assets
                .iter()
                .find(|a| matches!(a.info, amm::AssetInfo::Token { .. }))
                .map(|a| a.info.clone())
                .ok_or_else(|| {
                    StdError::generic_err(
                        "State inconsistency: have cw20 amount but no cw20 asset info found",
                    )
                })?;

            conversions_needed.push((
                amm::Asset {
                    info: cw20_asset_to_convert_info,
                    amount: cw20_have - total_cw20_needs,
                },
                target_info.clone(),
            ));
        }
    }

    let mut swaps_to_execute: Vec<PlannedSwap> = vec![];
    let mut native_allocated = Uint128::zero();
    let mut cw20_allocated = Uint128::zero();
    for (i, split) in next_stage.splits.iter().enumerate() {
        let first_op = split.path.first().ok_or(ContractError::EmptyRoute {})?;
        let offer_info = get_operation_input(first_op)?;
        let amount_for_split = if i < next_stage.splits.len() - 1 {
            total_logical_amount.multiply_ratio(split.percent as u128, 100u128)
        } else {
            let already_allocated = native_allocated + cw20_allocated;
            total_logical_amount
                .checked_sub(already_allocated)
                .map_err(StdError::from)?
        };
        match offer_info {
            amm::AssetInfo::NativeToken { .. } => native_allocated += amount_for_split,
            amm::AssetInfo::Token { .. } => cw20_allocated += amount_for_split,
        }
        swaps_to_execute.push(PlannedSwap {
            operation: first_op.clone(),
            amount: amount_for_split,
        });
    }

    Ok(StagePlan {
        swaps_to_execute,
        conversions_needed,
    })
}

fn get_operation_input(op: &Operation) -> Result<amm::AssetInfo, ContractError> {
    Ok(match op {
        Operation::AmmSwap(o) => o.offer_asset_info.clone(),
        Operation::OrderbookSwap(o) => o.offer_asset_info.clone(),
    })
}

fn execute_planned_swaps(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: Env,
    exec_state: &mut ExecutionState,
    plan: &RoutePlan,
    reply_id: u64,
    swaps: Vec<PlannedSwap>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut submessages = vec![];

    for swap in swaps.into_iter().filter(|s| !s.amount.is_zero()) {
        let offer_asset_info = get_operation_input(&swap.operation)?;
        let msg =
            create_swap_cosmos_msg(deps, &swap.operation, &offer_asset_info, swap.amount, &env)?;
        submessages.push(SubMsg::reply_on_success(msg, reply_id));
    }

    if submessages.is_empty() {
        exec_state.current_stage_index += 1;
        return proceed_to_next_step(deps, env, exec_state, plan, reply_id);
    }

    exec_state.awaiting = Awaiting::Swaps;
    exec_state.replies_expected = submessages.len() as u64;

    EXECUTION_STATES.save(deps.storage, reply_id, exec_state)?;

    Ok(Response::new()
        .add_submessages(submessages)
        .add_attribute("action", "executing_planned_swaps")
        .add_attribute("stage_index", exec_state.current_stage_index.to_string()))
}

fn get_operation_address(op: &Operation) -> &String {
    match op {
        Operation::AmmSwap(o) => &o.pool_address,
        Operation::OrderbookSwap(o) => &o.swap_contract,
    }
}

fn handle_path_conversion_reply(
    mut deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    exec_state: &mut ExecutionState,
    _plan: &RoutePlan,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let master_reply_id = msg.id;

    let converted_amount = parse_amount_from_conversion_reply(&msg, &env)?;

    let pending_op_details = exec_state.pending_path_op.take().ok_or_else(|| {
        StdError::generic_err("Path conversion state is invalid: no pending operation found")
    })?;

    let converted_asset_info = get_operation_input(&pending_op_details.operation)?;

    let swap_msg = create_swap_cosmos_msg(
        &mut deps,
        &pending_op_details.operation,
        &converted_asset_info,
        converted_amount,
        &env,
    )?;
    let sub_msg = SubMsg::reply_on_success(swap_msg, master_reply_id);

    exec_state.awaiting = Awaiting::Swaps;

    EXECUTION_STATES.save(deps.storage, master_reply_id, exec_state)?;

    Ok(Response::new()
        .add_submessage(sub_msg)
        .add_attribute("action", "resuming_path_after_conversion"))
}
