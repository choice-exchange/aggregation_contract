use cosmwasm_std::{
    to_json_binary, Addr, Coin, CosmosMsg, Deps, DepsMut, Env, Reply, Response, StdError, SubMsg,
    Uint128, WasmMsg,
};
use crate::cw20::Cw20ExecuteMsg;
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};

use crate::error::ContractError;
use crate::execute::create_swap_cosmos_msg;
use crate::msg::{amm, cw20_adapter, Operation, PlannedSwap, Stage, StagePlan};
use crate::orderbook_exec;
use crate::state::{
    Awaiting, Config, ExecutionState, PendingPathOp, SubmsgReplyState, ACTIVE_ROUTES, CONFIG,
    FEE_MAP, REPLY_ID_COUNTER, SUBMSG_REPLY_STATES, TAX_TOKEN_REGISTRY,
};

const DECIMAL_FRACTIONAL: u128 = 1_000_000_000_000_000_000;

pub fn handle_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if let Ok(submsg_state) = SUBMSG_REPLY_STATES.load(deps.storage, msg.id) {
        let master_reply_id = submsg_state.master_reply_id;
        let mut exec_state = ACTIVE_ROUTES.load(deps.storage, master_reply_id)?;
        SUBMSG_REPLY_STATES.remove(deps.storage, msg.id);

        handle_swap_reply(deps, env, msg, &mut exec_state, submsg_state)
    } else {
        let master_reply_id = msg.id;
        let mut exec_state = ACTIVE_ROUTES.load(deps.storage, master_reply_id)?;

        match exec_state.awaiting {
            Awaiting::Conversions => handle_conversion_reply(deps, env, msg, &mut exec_state),
            Awaiting::FinalConversions => {
                handle_final_conversion_reply(deps, env, msg, &mut exec_state)
            }
            Awaiting::PathConversion => {
                handle_path_conversion_reply(deps, env, msg, &mut exec_state)
            }
            Awaiting::Swaps => Err(ContractError::Std(StdError::msg(format!(
                "Unregistered swap reply ID received: {}",
                msg.id
            )))),
        }
    }
}

pub(crate) fn proceed_to_next_step(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: Env,
    exec_state: &mut ExecutionState,
    master_reply_id: u64,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if exec_state.current_stage_index as usize >= exec_state.plan.stages.len() {
        return handle_final_stage(deps, env, master_reply_id, exec_state);
    }
    let next_stage_to_execute = exec_state
        .plan
        .stages
        .get(exec_state.current_stage_index as usize)
        .unwrap();

    let stage_plan = plan_next_stage(
        deps.as_ref(),
        &exec_state.accumulated_assets,
        next_stage_to_execute,
    )?;
    exec_state.accumulated_assets.clear();

    if stage_plan.conversions_needed.is_empty() {
        execute_planned_swaps(
            deps,
            env,
            exec_state,
            master_reply_id,
            &stage_plan.swaps_to_execute,
        )
    } else {
        let config = CONFIG.load(deps.storage)?;
        let mut conversion_submsgs = Vec::with_capacity(stage_plan.conversions_needed.len());
        for (asset_to_convert, _target_info) in &stage_plan.conversions_needed {
            let msg = create_conversion_msg(asset_to_convert, &config, &env)?;
            conversion_submsgs.push(SubMsg::reply_on_success(msg, master_reply_id));
        }

        exec_state.awaiting = Awaiting::Conversions;
        exec_state.replies_expected = conversion_submsgs.len() as u64;
        exec_state.pending_swaps = stage_plan.swaps_to_execute;

        ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;

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
    submsg_state: SubmsgReplyState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let master_reply_id = submsg_state.master_reply_id;
    let split_index = submsg_state.split_index;
    let op_index = submsg_state.op_index;

    let current_stage = exec_state
        .plan
        .stages
        .get(exec_state.current_stage_index as usize)
        .ok_or(ContractError::EmptyRoute {})?;

    let replied_op = &current_stage.splits[split_index].path[op_index];

    let result = match msg.result.into_result() {
        Ok(response) => response,
        Err(e) => {
            return Err(ContractError::SubmessageFailed {
                split_index,
                op_index,
                contract_addr: get_operation_address(replied_op),
                error: e,
            });
        }
    };

    // The produced amount comes from the typed spot-order response for orderbook
    // hops, and from wasm event attributes for AMM/CLMM hops.
    let received_amount = match replied_op {
        Operation::OrderbookSwap(ob) => {
            let market = orderbook_exec::load_market(deps.as_ref(), &ob.market_id)?;
            orderbook_exec::parse_order_output(&market, &ob.target_denom, &result)
                .map_err(|e| ContractError::OrderResponseDecode { err: e.to_string() })?
        }
        _ => parse_amount_from_swap_reply(&result.events, &env)?,
    };

    // A zero fill (no liquidity, IOC no-fill, or a zero-value path) ends this path
    // without contributing an asset.
    if received_amount.is_zero() {
        exec_state.replies_expected -= 1;
        if exec_state.replies_expected == 0 {
            exec_state.current_stage_index += 1;
            return proceed_to_next_step(&mut deps, env, exec_state, master_reply_id);
        } else {
            ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;
            return Ok(Response::new()
                .add_attribute("action", "accumulating_path_outputs")
                .add_attribute("info", "zero_value_path_completed"));
        }
    }

    let received_asset_info = get_operation_output(deps.api, replied_op, &result.events)?;

    let replied_path = &current_stage.splits[split_index].path;

    if let Some(next_op) = replied_path.get(op_index + 1) {
        // This is a multi-hop path, proceed to the next operation.
        let offer_asset_for_next_op = amm::Asset {
            info: received_asset_info,
            amount: received_amount,
        };

        // Before dispatching the next message, check for asset mismatch.
        let required_input_info = get_operation_input(deps.as_ref(), next_op)?;
        if offer_asset_for_next_op.info != required_input_info {
            // A mid-path conversion is needed.
            exec_state.awaiting = Awaiting::PathConversion;
            exec_state.pending_path_op = Some(PendingPathOp {
                operation: next_op.clone(),
                amount: received_amount,
            });

            let config = CONFIG.load(deps.storage)?;
            let conversion_msg = create_conversion_msg(&offer_asset_for_next_op, &config, &env)?;

            let sub_msg = SubMsg::reply_on_success(conversion_msg, master_reply_id);
            ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;

            return Ok(Response::new()
                .add_submessage(sub_msg)
                .add_attribute("action", "performing_path_conversion"));
        }

        // Create the message for the next step.
        let next_msg = create_swap_cosmos_msg(
            &mut deps,
            next_op,
            &offer_asset_for_next_op.info,
            offer_asset_for_next_op.amount,
            &env,
        )?;

        let mut reply_id_counter = REPLY_ID_COUNTER.load(deps.storage)?;
        reply_id_counter += 1;
        REPLY_ID_COUNTER.save(deps.storage, &reply_id_counter)?;
        let next_submsg_id = reply_id_counter;

        SUBMSG_REPLY_STATES.save(
            deps.storage,
            next_submsg_id,
            &SubmsgReplyState {
                master_reply_id,
                split_index,
                op_index: op_index + 1,
            },
        )?;

        let sub_msg = SubMsg::reply_on_success(next_msg, next_submsg_id);

        ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;

        Ok(Response::new()
            .add_submessage(sub_msg)
            .add_attribute("action", "proceeding_to_next_op_in_path")
            .add_attribute("split_index", split_index.to_string())
            .add_attribute("op_index", (op_index + 1).to_string()))
    } else {
        // The aggregator's per-pool fee (FEE_MAP) is keyed by a pool/contract
        // address. Orderbook hops have no such address (the order is placed
        // natively, and the exchange already takes its own trading fee), so they
        // carry no aggregator fee.
        let (amount_after_fee, fee, fee_pool_label) = match replied_op {
            Operation::OrderbookSwap(_) => (received_amount, Uint128::zero(), None),
            _ => {
                let pool_addr =
                    deps.api.addr_validate(&get_operation_address(replied_op))?;
                let (after_fee, fee) = apply_fee(&deps, &pool_addr, received_amount)?;
                (after_fee, fee, Some(pool_addr.to_string()))
            }
        };

        exec_state.accumulated_assets.push(amm::Asset {
            info: received_asset_info.clone(),
            amount: amount_after_fee,
        });
        exec_state.replies_expected -= 1;

        let mut response;
        if exec_state.replies_expected > 0 {
            ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;
            response = Response::new().add_attribute("action", "accumulating_path_outputs");
        } else {
            exec_state.current_stage_index += 1;
            response = proceed_to_next_step(&mut deps, env, exec_state, master_reply_id)?;
        }

        if !fee.is_zero() {
            let config = CONFIG.load(deps.storage)?;
            let fee_send_msg =
                create_send_msg(&deps, &config.fee_collector, &received_asset_info, fee)?;
            response = response
                .add_message(fee_send_msg)
                .add_attribute("fee_collected", fee.to_string())
                .add_attribute("fee_pool", fee_pool_label.unwrap_or_default());
        }
        Ok(response)
    }
}

fn apply_fee(
    deps: &DepsMut<InjectiveQueryWrapper>,
    pool_addr: &Addr,
    amount: Uint128,
) -> Result<(Uint128, Uint128), StdError> {
    let fee = match FEE_MAP.may_load(deps.storage, pool_addr)? {
        Some(fee_percent) => amount.multiply_ratio(fee_percent.atomics(), DECIMAL_FRACTIONAL),
        None => Uint128::zero(),
    };

    let amount_after_fee = amount.checked_sub(fee)?;
    Ok((amount_after_fee, fee))
}

// A helper to create the final transfer message.
fn create_send_msg(
    deps: &DepsMut<InjectiveQueryWrapper>,
    recipient: &Addr,
    asset_info: &amm::AssetInfo,
    amount: Uint128,
) -> Result<CosmosMsg<InjectiveMsgWrapper>, ContractError> {
    match asset_info {
        amm::AssetInfo::NativeToken { denom } => Ok(CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address: recipient.to_string(),
            amount: vec![Coin {
                denom: denom.clone(),
                amount: amount.into(),
            }],
        })),
        amm::AssetInfo::Token { contract_addr } => {
            let token_addr = deps.api.addr_validate(contract_addr)?;
            // Check if we are dealing with a registered tax token.
            if TAX_TOKEN_REGISTRY.has(deps.storage, &token_addr) {
                // Use the new tax-exempt message.
                Ok(CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: contract_addr.clone(),
                    msg: to_json_binary(&crate::msg::reflection::ExecuteMsg::TaxExemptTransfer {
                        recipient: recipient.to_string(),
                        amount,
                    })?,
                    funds: vec![],
                }))
            } else {
                // Use a standard CW20 Transfer for all other tokens.
                Ok(CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: contract_addr.clone(),
                    msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                        recipient: recipient.to_string(),
                        amount,
                    })?,
                    funds: vec![],
                }))
            }
        }
    }
}

/// Terminal disposition of a completed route's output. For an ordinary swap the
/// whole `total_amount` goes to the route's sender (gated by `minimum_receive`).
/// For a flash-arb cycle it repays `principal + fee` to the flash pool by direct
/// transfer and forwards the surplus to the initiator (gated by `min_profit`).
fn finalize_route(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    reply_id: u64,
    exec_state: &ExecutionState,
    total_amount: Uint128,
    asset_info: &amm::AssetInfo,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if let Some(flash) = &exec_state.plan.flash_repayment {
        let required = flash
            .repay_amount
            .checked_add(flash.min_profit)
            .map_err(StdError::from)?;
        if total_amount < required {
            return Err(ContractError::FlashProfitNotMet {
                required,
                actual: total_amount,
            });
        }
        let surplus = total_amount
            .checked_sub(flash.repay_amount)
            .map_err(StdError::from)?;

        // Repay the pool by direct transfer. `create_send_msg` uses Bank `Send` /
        // CW20 `Transfer` (never CW20 `Send`), which is exactly what the pool's
        // reentrancy-locked flash requires.
        let mut response = Response::new().add_message(create_send_msg(
            deps,
            &flash.pool,
            asset_info,
            flash.repay_amount,
        )?);
        if !surplus.is_zero() {
            response = response.add_message(create_send_msg(
                deps,
                &exec_state.plan.sender,
                asset_info,
                surplus,
            )?);
        }
        ACTIVE_ROUTES.remove(deps.storage, reply_id);
        Ok(response
            .add_attribute("action", "flash_route_complete")
            .add_attribute("repaid", flash.repay_amount.to_string())
            .add_attribute("profit", surplus.to_string()))
    } else {
        if total_amount < exec_state.plan.minimum_receive {
            return Err(ContractError::MinimumReceiveNotMet {
                minimum_receive: exec_state.plan.minimum_receive,
                actual_receive: total_amount,
            });
        }
        let mut response = Response::new();
        if !total_amount.is_zero() {
            response = response.add_message(create_send_msg(
                deps,
                &exec_state.plan.sender,
                asset_info,
                total_amount,
            )?);
        }
        ACTIVE_ROUTES.remove(deps.storage, reply_id);
        Ok(response
            .add_attribute("action", "aggregate_swap_complete")
            .add_attribute("final_received", total_amount.to_string()))
    }
}

fn handle_final_stage(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: Env,
    reply_id: u64,
    exec_state: &mut ExecutionState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if exec_state.accumulated_assets.is_empty() {
        if let Some(flash) = &exec_state.plan.flash_repayment {
            // The cycle produced nothing, so the loan can't be repaid. (The pool's
            // `reply_flash` would revert the tx anyway; surface a precise error.)
            let required = flash
                .repay_amount
                .checked_add(flash.min_profit)
                .map_err(StdError::from)?;
            return Err(ContractError::FlashProfitNotMet {
                required,
                actual: Uint128::zero(),
            });
        }
        if !exec_state.plan.minimum_receive.is_zero() {
            return Err(ContractError::MinimumReceiveNotMet {
                minimum_receive: exec_state.plan.minimum_receive,
                actual_receive: Uint128::zero(),
            });
        }
        ACTIVE_ROUTES.remove(deps.storage, reply_id);
        return Ok(Response::new().add_attribute("action", "aggregate_swap_complete_empty"));
    }

    // Normalization target: the borrowed asset for a flash cycle (so the route
    // closes in the token it must repay), otherwise the first accumulated asset.
    let target_asset_info = exec_state
        .plan
        .flash_repayment
        .as_ref()
        .map(|f| f.asset.clone())
        .unwrap_or_else(|| exec_state.accumulated_assets[0].info.clone());

    let mut conversion_submsgs = Vec::with_capacity(exec_state.accumulated_assets.len());
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
        // SCENARIO A: All assets were already the target type. We are done.
        finalize_route(deps, reply_id, exec_state, ready_amount, &target_asset_info)
    } else {
        // SCENARIO B: Conversions are needed. Set up the exec_state for the final reply.
        exec_state.awaiting = Awaiting::FinalConversions;
        exec_state.replies_expected = conversion_submsgs.len() as u64;
        exec_state.accumulated_assets = vec![amm::Asset {
            info: target_asset_info,
            amount: ready_amount,
        }];

        ACTIVE_ROUTES.save(deps.storage, reply_id, exec_state)?;

        Ok(Response::new()
            .add_submessages(conversion_submsgs)
            .add_attribute("action", "final_asset_normalization_started"))
    }
}

fn handle_final_conversion_reply(
    mut deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    exec_state: &mut ExecutionState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if msg.result.is_err() {
        return Err(ContractError::ConversionFailed {
            awaiting_state: "FinalConversions".to_string(),
            error: msg.result.unwrap_err(),
        });
    }

    let reply_id = msg.id;
    let events = &msg.result.into_result().unwrap().events;
    let converted_amount = parse_amount_from_conversion_reply(events, &env)?;

    let running_total_asset = exec_state.accumulated_assets.get_mut(0).ok_or_else(|| {
        StdError::msg("Final conversion state is invalid: no accumulated asset found")
    })?;

    running_total_asset.amount += converted_amount;
    exec_state.replies_expected -= 1;

    if exec_state.replies_expected > 0 {
        // Still waiting for more conversions to finish.
        ACTIVE_ROUTES.save(deps.storage, reply_id, exec_state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_final_conversions"));
    }

    // All final conversions are complete.
    let total_final_amount = running_total_asset.amount;
    let final_asset_info = running_total_asset.info.clone();

    finalize_route(
        &mut deps,
        reply_id,
        exec_state,
        total_final_amount,
        &final_asset_info,
    )
}

fn handle_conversion_reply(
    mut deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    exec_state: &mut ExecutionState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if msg.result.is_err() {
        return Err(ContractError::ConversionFailed {
            awaiting_state: "Conversions".to_string(),
            error: msg.result.unwrap_err(),
        });
    }

    let master_reply_id = msg.id;
    exec_state.replies_expected -= 1;

    if exec_state.replies_expected > 0 {
        ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_conversion_outputs"));
    }

    let swaps_to_execute = std::mem::take(&mut exec_state.pending_swaps);

    execute_planned_swaps(
        &mut deps,
        env,
        exec_state,
        master_reply_id,
        &swaps_to_execute,
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
                amount: from.amount.into(),
            }],
        })),
    }
}

/// The asset a completed hop produced. AMM/CLMM ops no longer carry an explicit
/// `ask_asset_info`; instead both pools emit an `ask_asset` attribute on their
/// swap event, which we read back here at zero query cost. Orderbook output is
/// always the native `target_denom`.
fn get_operation_output(
    api: &dyn cosmwasm_std::Api,
    op: &Operation,
    events: &[cosmwasm_std::Event],
) -> Result<amm::AssetInfo, ContractError> {
    Ok(match op {
        Operation::AmmSwap(_) | Operation::ClmmSwap(_) => parse_ask_asset_from_events(api, events)?,
        Operation::OrderbookSwap(o) => amm::AssetInfo::NativeToken {
            denom: o.target_denom.clone(),
        },
    })
}

/// Reconstruct the output `AssetInfo` from a pool swap reply. Both the legacy AMM
/// pair and the CLMM pool emit an `ask_asset` attribute equal to the output
/// asset's key — a bank denom or a CW20 contract address. The variant is recovered
/// by bech32 validation: a value that validates as an address is a CW20
/// (`Token`), anything else is a native bank denom (`NativeToken`). This is
/// unambiguous on Injective — native denoms (`inj`, `peggy0x..`, `factory/..`,
/// `ibc/..`, ...) are never bare bech32 addresses, so they can't be mistaken for a
/// CW20 contract.
fn parse_ask_asset_from_events(
    api: &dyn cosmwasm_std::Api,
    events: &[cosmwasm_std::Event],
) -> Result<amm::AssetInfo, ContractError> {
    let key = events
        .iter()
        .filter(|e| e.ty.starts_with("wasm"))
        .find_map(|e| {
            e.attributes
                .iter()
                .find(|a| a.key == "ask_asset")
                .map(|a| a.value.clone())
        })
        .ok_or(ContractError::NoAskAssetInReply {})?;

    Ok(if api.addr_validate(&key).is_ok() {
        amm::AssetInfo::Token { contract_addr: key }
    } else {
        amm::AssetInfo::NativeToken { denom: key }
    })
}

fn parse_amount_from_swap_reply(
    events: &[cosmwasm_std::Event],
    env: &Env,
) -> Result<Uint128, ContractError> {
    // Check for `post_tax_amount` from a tax token's transfer event.
    for event in events.iter().rev() {
        if event.ty != "wasm" {
            continue;
        }
        let is_recipient_self = event
            .attributes
            .iter()
            .any(|attr| attr.key == "to" && attr.value == env.contract.address.to_string());

        if is_recipient_self {
            if let Some(amount_attr) = event
                .attributes
                .iter()
                .find(|attr| attr.key == "post_tax_amount")
            {
                return amount_attr.value.parse::<Uint128>().map_err(|_| {
                    ContractError::MalformedAmountInReply {
                        value: amount_attr.value.clone(),
                    }
                });
            }
        }
    }

    // 2. Fallback to original logic for standard, non-taxable tokens.
    //    AMM emits "return_amount", CLMM emits "amount_out". (Orderbook hops are
    //    decoded from the typed spot-order response, not from events.)
    let amount_str_opt = events.iter().find_map(|event| {
        if !event.ty.starts_with("wasm") {
            return None;
        }
        event
            .attributes
            .iter()
            .find(|attr| attr.key == "return_amount" || attr.key == "amount_out")
            .map(|attr| attr.value.clone())
    });

    match amount_str_opt {
        Some(amount_str) => {
            let integer_part_str = if let Some(period_pos) = amount_str.find('.') {
                &amount_str[..period_pos]
            } else {
                &amount_str
            };
            integer_part_str
                .parse::<Uint128>()
                .map_err(|_| ContractError::MalformedAmountInReply { value: amount_str })
        }
        None => Ok(Uint128::zero()), // Return zero if no relevant amount is found.
    }
}

fn parse_amount_from_conversion_reply(
    events: &[cosmwasm_std::Event],
    env: &Env,
) -> Result<Uint128, ContractError> {
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
    deps: Deps<InjectiveQueryWrapper>,
    accumulated_assets: &[amm::Asset],
    next_stage: &Stage,
) -> Result<StagePlan, ContractError> {
    let mut native_info: Option<amm::AssetInfo> = None;
    let mut cw20_info: Option<amm::AssetInfo> = None;

    for split in &next_stage.splits {
        let first_op = split.path.first().ok_or(ContractError::EmptyRoute {})?;
        let offer_info = get_operation_input(deps, first_op)?;
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
        let offer_info = get_operation_input(deps, first_op)?;
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
                    StdError::msg(
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
                    StdError::msg(
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
        let offer_info = get_operation_input(deps, first_op)?;
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
            split_index: i,
            op_index: 0,
        });
    }

    Ok(StagePlan {
        swaps_to_execute,
        conversions_needed,
    })
}

fn get_operation_input(
    deps: Deps<InjectiveQueryWrapper>,
    op: &Operation,
) -> Result<amm::AssetInfo, ContractError> {
    Ok(match op {
        Operation::AmmSwap(o) => o.offer_asset_info.clone(),
        // The orderbook offer denom is the market side opposite `target_denom`.
        Operation::OrderbookSwap(o) => {
            let market = orderbook_exec::load_market(deps, &o.market_id)?;
            let offer_denom =
                orderbook_exec::offer_denom_for(&market, &o.target_denom).map_err(|_| {
                    ContractError::InvalidOrderbookDenom {
                        denom: o.target_denom.clone(),
                        market_id: o.market_id.as_str().to_string(),
                    }
                })?;
            amm::AssetInfo::NativeToken { denom: offer_denom }
        }
        Operation::ClmmSwap(o) => o.offer_asset_info.clone(),
    })
}

fn execute_planned_swaps(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: Env,
    exec_state: &mut ExecutionState,
    master_reply_id: u64,
    swaps: &[PlannedSwap],
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut submessages = Vec::with_capacity(swaps.len());
    let mut reply_id_counter = REPLY_ID_COUNTER.load(deps.storage)?;

    for swap in swaps.iter().filter(|s| !s.amount.is_zero()) {
        reply_id_counter += 1;
        let submsg_id = reply_id_counter;

        SUBMSG_REPLY_STATES.save(
            deps.storage,
            submsg_id,
            &SubmsgReplyState {
                master_reply_id,
                split_index: swap.split_index,
                op_index: swap.op_index,
            },
        )?;

        let offer_asset_info = get_operation_input(deps.as_ref(), &swap.operation)?;
        let msg =
            create_swap_cosmos_msg(deps, &swap.operation, &offer_asset_info, swap.amount, &env)?;

        submessages.push(SubMsg::reply_on_success(msg, submsg_id));
    }

    REPLY_ID_COUNTER.save(deps.storage, &reply_id_counter)?;

    if submessages.is_empty() {
        exec_state.current_stage_index += 1;
        return proceed_to_next_step(deps, env, exec_state, master_reply_id);
    }

    exec_state.awaiting = Awaiting::Swaps;
    exec_state.replies_expected = submessages.len() as u64;

    ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;

    Ok(Response::new()
        .add_submessages(submessages)
        .add_attribute("action", "executing_planned_swaps")
        .add_attribute("stage_index", exec_state.current_stage_index.to_string()))
}

/// A label identifying the contract/market a swap op routes through. AMM/CLMM ops
/// return their pool address (also the `FEE_MAP` key); orderbook ops have no
/// contract, so the market id is returned for diagnostics only.
fn get_operation_address(op: &Operation) -> String {
    match op {
        Operation::AmmSwap(o) => o.pool_address.clone(),
        Operation::OrderbookSwap(o) => o.market_id.as_str().to_string(),
        Operation::ClmmSwap(o) => o.pool_address.clone(),
    }
}

fn handle_path_conversion_reply(
    mut deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
    exec_state: &mut ExecutionState,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if msg.result.is_err() {
        return Err(ContractError::ConversionFailed {
            awaiting_state: "PathConversion".to_string(),
            error: msg.result.unwrap_err(),
        });
    }

    let master_reply_id = msg.id;
    let events = &msg.result.into_result().unwrap().events;
    let converted_amount = parse_amount_from_conversion_reply(events, &env)?;

    let pending_op_details = exec_state.pending_path_op.take().ok_or_else(|| {
        StdError::msg("Path conversion state is invalid: no pending operation found")
    })?;

    let current_stage = exec_state
        .plan
        .stages
        .get(exec_state.current_stage_index as usize)
        .unwrap();
    let (split_index, op_index) = current_stage
        .splits
        .iter()
        .enumerate()
        .find_map(|(si, split)| {
            split
                .path
                .iter()
                .enumerate()
                .find(|(_, op)| **op == pending_op_details.operation)
                .map(|(oi, _)| (si, oi))
        })
        .ok_or_else(|| StdError::msg("Could not find pending op in route plan"))?;

    let converted_asset_info = get_operation_input(deps.as_ref(), &pending_op_details.operation)?;
    let swap_msg = create_swap_cosmos_msg(
        &mut deps,
        &pending_op_details.operation,
        &converted_asset_info,
        converted_amount,
        &env,
    )?;

    let mut reply_id_counter = REPLY_ID_COUNTER.load(deps.storage)?;
    reply_id_counter += 1;
    REPLY_ID_COUNTER.save(deps.storage, &reply_id_counter)?;
    let submsg_id = reply_id_counter;

    SUBMSG_REPLY_STATES.save(
        deps.storage,
        submsg_id,
        &SubmsgReplyState {
            master_reply_id,
            split_index,
            op_index,
        },
    )?;

    let sub_msg = SubMsg::reply_on_success(swap_msg, submsg_id);

    exec_state.awaiting = Awaiting::Swaps;
    ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;

    Ok(Response::new()
        .add_submessage(sub_msg)
        .add_attribute("action", "resuming_path_after_conversion"))
}
