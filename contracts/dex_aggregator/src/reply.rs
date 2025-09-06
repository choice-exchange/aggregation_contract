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
        return handle_final_stage(deps, env, msg.id, state);
    }

    let next_stage = state
        .stages
        .get((state.current_stage_index + 1) as usize)
        .unwrap();
    let config = CONFIG.load(deps.storage)?;

    // --- CALL THE RECONCILER ---
    let reconciliation = reconcile_assets(&state.accumulated_assets, next_stage)?;

    state.accumulated_assets.clear(); // We are done with the raw outputs.

    if reconciliation.conversions_needed.is_empty() {
        // SCENARIO A: No conversions needed. Proceed directly to the next stage.
        state.current_stage_index += 1;
        execute_next_swap_stage(
            deps,
            env,
            state,
            master_reply_id,
            reconciliation.assets_ready_to_use,
        )
    } else {
        // SCENARIO B: Conversions are required.
        let mut conversion_submsgs = vec![];
        for (asset_to_convert, _target_info) in &reconciliation.conversions_needed {
            let msg = create_conversion_msg(asset_to_convert, &config, &env)?;
            conversion_submsgs.push(SubMsg::reply_on_success(msg, master_reply_id));
        }

        state.awaiting = Awaiting::Conversions;
        state.replies_expected = conversion_submsgs.len() as u64;
        state.ready_assets_for_next_stage = reconciliation.assets_ready_to_use;
        REPLY_STATES.save(deps.storage, master_reply_id, state)?;

        Ok(Response::new()
            .add_submessages(conversion_submsgs)
            .add_attribute("action", "performing_minimal_conversions"))
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
    let converted_amount = parse_amount_from_conversion_reply(&msg, &env)?;
    let converted_asset_info = parse_asset_info_from_conversion_reply(&msg, &env)?;

    // Add the newly converted asset to our list of ready assets.
    if let Some((_info, amount)) = state
        .ready_assets_for_next_stage
        .iter_mut()
        .find(|(info, _)| *info == converted_asset_info)
    {
        *amount += converted_amount;
    } else {
        state
            .ready_assets_for_next_stage
            .push((converted_asset_info, converted_amount));
    }

    state.replies_expected -= 1;

    if state.replies_expected > 0 {
        REPLY_STATES.save(deps.storage, master_reply_id, state)?;
        return Ok(Response::new().add_attribute("action", "accumulating_conversion_outputs"));
    }

    // All conversions are complete.
    state.current_stage_index += 1;
    let final_ready_assets = std::mem::take(&mut state.ready_assets_for_next_stage);

    execute_next_swap_stage(deps, env, state, master_reply_id, final_ready_assets)
}

fn execute_next_swap_stage(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    state: &mut ReplyState,
    reply_id: u64,
    ready_assets: Vec<(external::AssetInfo, Uint128)>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let next_stage_idx = state.current_stage_index as usize;
    let next_stage = state
        .stages
        .get(next_stage_idx)
        .ok_or(ContractError::EmptyRoute {})?;

    let mut submessages = vec![];

    // --- NEW LOGIC ---
    // Keep track of how much of each asset pile we have allocated.
    let mut amounts_allocated: Vec<(external::AssetInfo, Uint128)> = vec![];

    for (i, split) in next_stage.splits.iter().enumerate() {
        let offer_asset_info_for_split = match &split.operation {
            Operation::AmmSwap(o) => &o.offer_asset_info,
            Operation::OrderbookSwap(o) => &o.offer_asset_info,
        };

        // Find the total available amount for this asset type from the reconciled assets.
        let total_amount_for_type = ready_assets
            .iter()
            .find(|(info, _amount)| info == offer_asset_info_for_split)
            .map(|(_info, amount)| *amount)
            .unwrap_or_else(Uint128::zero);

        // Determine how many other splits also require this same asset.
        // This is important for the remainder calculation.
        let num_splits_for_this_asset = next_stage
            .splits
            .iter()
            .filter(|s| {
                let offer_info = match &s.operation {
                    Operation::AmmSwap(o) => &o.offer_asset_info,
                    Operation::OrderbookSwap(o) => &o.offer_asset_info,
                };
                offer_info == offer_asset_info_for_split
            })
            .count();

        // Find the index of the current split among those that use the same asset.
        let current_split_index_for_asset = next_stage
            .splits
            .iter()
            .enumerate()
            .filter(|(_i, s)| {
                let offer_info = match &s.operation {
                    Operation::AmmSwap(o) => &o.offer_asset_info,
                    Operation::OrderbookSwap(o) => &o.offer_asset_info,
                };
                offer_info == offer_asset_info_for_split
            })
            .position(|(idx, _s)| idx == i)
            .unwrap_or(0);

        // Use the remainder method for splitting the pile.
        let amount_for_split = if current_split_index_for_asset < num_splits_for_this_asset - 1 {
            total_amount_for_type.multiply_ratio(split.percent as u128, 100u128)
        } else {
            // This is the last split for this asset type, it gets the remainder.
            let already_allocated = amounts_allocated
                .iter()
                .find(|(info, _)| info == offer_asset_info_for_split)
                .map(|(_, amount)| *amount)
                .unwrap_or_else(Uint128::zero);
            total_amount_for_type
                .checked_sub(already_allocated)
                .map_err(StdError::overflow)?
        };

        // Update our running total of allocated funds.
        if let Some((_, allocated)) = amounts_allocated
            .iter_mut()
            .find(|(info, _)| info == offer_asset_info_for_split)
        {
            *allocated += amount_for_split;
        } else {
            amounts_allocated.push((offer_asset_info_for_split.clone(), amount_for_split));
        }

        let msg = create_swap_cosmos_msg(
            &deps,
            &split.operation,
            offer_asset_info_for_split,
            amount_for_split,
            &env,
        )?;
        submessages.push(SubMsg::reply_on_success(msg, reply_id));
    }

    state.awaiting = Awaiting::Swaps;
    state.replies_expected = submessages.len() as u64;
    state.ready_assets_for_next_stage.clear();
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

fn parse_asset_info_from_conversion_reply(
    msg: &Reply,
    env: &Env,
) -> Result<external::AssetInfo, ContractError> {
    let events = &msg
        .result
        .clone()
        .into_result()
        .map_err(|e| ContractError::SubmessageResultError { error: e })?
        .events;

    // --- Case 1: Look for a native token transfer from the bank module ---
    // This event indicates we received a native (e.g., token-factory) asset.
    if let Some(transfer_event) = events.iter().find(|e| {
        e.ty == "transfer"
            && e.attributes
                .iter()
                .any(|a| a.key == "recipient" && a.value == env.contract.address.to_string())
    }) {
        // Find the amount attribute, which contains both the amount and the denom.
        let amount_attr = transfer_event
            .attributes
            .iter()
            .find(|a| a.key == "amount")
            .ok_or(ContractError::NoAmountInReply {})?;

        // The denom is the non-numeric part of the amount string.
        let denom_start_index = amount_attr
            .value
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| ContractError::MalformedAmountInReply {
                value: amount_attr.value.clone(),
            })?;

        let denom = &amount_attr.value[denom_start_index..];
        return Ok(external::AssetInfo::NativeToken {
            denom: denom.to_string(),
        });
    }

    // --- Case 2: Look for a CW20 token transfer from a wasm module ---
    // This event indicates we received a CW20 asset.
    if let Some(wasm_event) = events.iter().find(|e| {
        // Find the specific wasm event corresponding to a CW20 transfer TO our contract.
        e.ty == "wasm"
            && e.attributes
                .iter()
                .any(|a| a.key == "action" && a.value == "transfer")
            && e.attributes
                .iter()
                .any(|a| a.key == "to" && a.value == env.contract.address.to_string())
    }) {
        // The '_contract_address' of this event is the address of the CW20 token.
        let contract_addr_attr = wasm_event
            .attributes
            .iter()
            .find(|a| a.key == "_contract_address")
            .ok_or(ContractError::NoConversionEventInReply {})?; // Or a more specific error

        return Ok(external::AssetInfo::Token {
            contract_addr: contract_addr_attr.value.clone(),
        });
    }

    // If neither pattern was found, it's an error.
    Err(ContractError::NoConversionEventInReply {})
}

struct Reconciliation {
    // Assets that are already the correct type and amount for the next stage.
    pub assets_ready_to_use: Vec<(external::AssetInfo, Uint128)>,
    // The specific conversions required to meet the needs of the next stage.
    pub conversions_needed: Vec<(external::Asset, external::AssetInfo)>, // (Asset to Convert, Target AssetInfo)
}

/// Compares the assets produced by the last stage with the assets required by the next stage
/// and determines the minimum set of conversions needed.
fn reconcile_assets(
    // The outputs from the completed stage (our "haves").
    accumulated_assets: &[external::Asset],
    // The definition of the next stage (to calculate our "needs").
    next_stage: &Stage,
) -> Result<Reconciliation, ContractError> {
    let mut native_info: Option<external::AssetInfo> = None;
    let mut cw20_info: Option<external::AssetInfo> = None;

    // --- NEW STEP 1: Discover all AssetInfos required by the next stage ---
    // This ensures we know the AssetInfo for a token even if we don't have any of it yet.
    for split in &next_stage.splits {
        let offer_info = match &split.operation {
            Operation::AmmSwap(o) => &o.offer_asset_info,
            Operation::OrderbookSwap(o) => &o.offer_asset_info,
        };
        match offer_info {
            external::AssetInfo::NativeToken { .. } => {
                if native_info.is_none() {
                    native_info = Some(offer_info.clone());
                }
            }
            external::AssetInfo::Token { .. } => {
                if cw20_info.is_none() {
                    cw20_info = Some(offer_info.clone());
                }
            }
        }
    }

    // --- STEP 2: Tally the "Haves" and the total logical amount ---
    let mut native_have = Uint128::zero();
    let mut cw20_have = Uint128::zero();

    for asset in accumulated_assets {
        match &asset.info {
            external::AssetInfo::NativeToken { .. } => {
                native_have += asset.amount;
                // If we didn't learn about native_info from the next stage, learn it from the haves.
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

    // --- STEP 3: Calculate the "Needs" ---
    let mut native_needs = Uint128::zero();
    let mut cw20_needs = Uint128::zero();
    let mut amount_allocated = Uint128::zero();
    let splits_count = next_stage.splits.len();

    for (i, split) in next_stage.splits.iter().enumerate() {
        // Calculate the amount for all but the last split using ratios.
        // The last split gets the remainder to prevent dust loss.
        let amount_for_split = if i < splits_count - 1 {
            total_logical_amount.multiply_ratio(split.percent as u128, 100u128)
        } else {
            total_logical_amount
                .checked_sub(amount_allocated)
                .map_err(|e| ContractError::Std(StdError::overflow(e)))?
        };
        amount_allocated += amount_for_split;

        let offer_info = match &split.operation {
            Operation::AmmSwap(o) => &o.offer_asset_info,
            Operation::OrderbookSwap(o) => &o.offer_asset_info,
        };
        match offer_info {
            external::AssetInfo::NativeToken { .. } => native_needs += amount_for_split,
            external::AssetInfo::Token { .. } => cw20_needs += amount_for_split,
        }
    }

    // --- STEP 4: Determine the Delta and Plan Conversions ---
    let mut assets_ready_to_use: Vec<(external::AssetInfo, Uint128)> = vec![];
    let mut conversions_needed: Vec<(external::Asset, external::AssetInfo)> = vec![];

    // Reconcile Native Tokens
    if native_have > native_needs {
        let surplus = native_have - native_needs;
        assets_ready_to_use.push((native_info.as_ref().unwrap().clone(), native_needs));
        if let Some(target_info) = &cw20_info {
            conversions_needed.push((
                external::Asset {
                    info: native_info.as_ref().unwrap().clone(),
                    amount: surplus,
                },
                target_info.clone(),
            ));
        }
    } else if !native_have.is_zero() {
        assets_ready_to_use.push((native_info.as_ref().unwrap().clone(), native_have));
    }

    // Reconcile CW20 Tokens
    if cw20_have > cw20_needs {
        let surplus = cw20_have - cw20_needs;
        assets_ready_to_use.push((cw20_info.as_ref().unwrap().clone(), cw20_needs));
        // MODIFIED: This check will now succeed because we populated native_info in Step 1.
        if let Some(target_info) = &native_info {
            conversions_needed.push((
                external::Asset {
                    info: cw20_info.as_ref().unwrap().clone(),
                    amount: surplus,
                },
                target_info.clone(),
            ));
        }
    } else if !cw20_have.is_zero() {
        assets_ready_to_use.push((cw20_info.as_ref().unwrap().clone(), cw20_have));
    }

    Ok(Reconciliation {
        assets_ready_to_use,
        conversions_needed,
    })
}
