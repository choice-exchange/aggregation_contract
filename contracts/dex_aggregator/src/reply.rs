use crate::error::ContractError;
use crate::execute::create_swap_cosmos_msg;
use crate::msg::Operation;
use crate::state::REPLY_STATES;
use cosmwasm_std::{DepsMut, Env, Reply, Response, SubMsg, Uint128};
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};

pub fn handle_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let event = match msg.result.clone().into_result() {
        Ok(result) => result.events.into_iter().find(|e| e.ty.starts_with("wasm")),
        Err(_) => None,
    };

    if event.is_none() {
        return Err(ContractError::Std(cosmwasm_std::StdError::generic_err(
            format!(
                "DEBUG: Failed to find wasm event in reply. Full Reply dump: {:?}",
                msg
            ),
        )));
    }
    let master_reply_id = msg.id;
    // Load the state for this reply ID
    let mut state = REPLY_STATES.load(deps.storage, msg.id)?;

    let event = msg
        .result
        .into_result()
        .map_err(|_| ContractError::ReplyParseError {})?
        .events
        .into_iter()
        .find(|e| e.ty.starts_with("wasm"))
        .ok_or(ContractError::ReplyParseError {})?;

    // Check event type and find the correct attribute for the return amount
    let amount_str = if event.ty == "wasm-atomic_swap_execution" {
        // This is an Orderbook swap
        event
            .attributes
            .into_iter()
            .find(|attr| attr.key == "swap_final_amount")
            .map(|attr| attr.value)
            .ok_or(ContractError::ReplyParseError {})?
    } else {
        // This is an AMM swap (event.ty == "wasm")
        event
            .attributes
            .into_iter()
            .find(|attr| attr.key == "return_amount")
            .map(|attr| attr.value)
            .ok_or(ContractError::ReplyParseError {})?
    };

    let amount_returned: Uint128 = amount_str.parse()?;

    // --- Accumulate amount for the CURRENT stage ---
    state.accumulated_amount_for_current_stage += amount_returned;
    state.replies_expected_for_current_stage -= 1;

    // --- DECISION POINT: Is the current stage finished? ---
    if state.replies_expected_for_current_stage > 0 {
        // NO: Stage is not finished. Save state and wait for more replies.
        REPLY_STATES.save(deps.storage, msg.id, &state)?;
        return Ok(Response::new().add_attribute("action", "stage_hop_pending"));
    }

    // YES: The current stage is complete.

    // --- DECISION POINT: Is this the LAST stage? ---
    let is_last_stage = state.current_stage_index as usize == state.stages.len() - 1;

    if is_last_stage {
        let final_amount = state.accumulated_amount_for_current_stage;

        if final_amount < state.minimum_receive {
            return Err(ContractError::MinimumReceiveNotMet {});
        }

        REPLY_STATES.remove(deps.storage, master_reply_id);
        Ok(Response::new()
            .add_attribute("action", "aggregate_swap_complete")
            .add_attribute("sender", state.sender)
            .add_attribute("final_received", final_amount.to_string()))
    } else {
        // NO: This was an intermediate stage. We must trigger the NEXT stage.

        // 1. Prepare for the next stage.
        let input_for_next_stage = state.accumulated_amount_for_current_stage;
        state.current_stage_index += 1;

        // We need to know the asset type of the intermediate amount.
        // KEY ASSUMPTION: All splits in a stage produce the SAME output asset.
        let current_stage = &state.stages[state.current_stage_index as usize - 1];
        let intermediate_asset_info = match &current_stage.splits[0].operation {
            Operation::AmmSwap(op) => op.ask_asset_info.clone(),
            Operation::OrderbookSwap(op) => op.ask_asset_info.clone(),
        };

        // 2. Get details for the next stage.
        let next_stage = &state.stages[state.current_stage_index as usize];
        state.replies_expected_for_current_stage = next_stage.splits.len() as u64;
        state.accumulated_amount_for_current_stage = Uint128::zero(); // Reset accumulator

        // 3. Dispatch submessages for the next stage.
        let mut submessages: Vec<SubMsg<InjectiveMsgWrapper>> = vec![];
        for split in &next_stage.splits {
            let split_amount = input_for_next_stage.multiply_ratio(split.percent as u128, 100u128);
            let msg = create_swap_cosmos_msg(
                &split.operation,
                &intermediate_asset_info,
                split_amount,
                &state.sender,
                &env,
                &state.stages,
                state.current_stage_index as usize,
            )?;
            submessages.push(SubMsg::reply_on_success(msg, master_reply_id));
        }

        // 4. Save the updated state and dispatch the new messages.
        REPLY_STATES.save(deps.storage, msg.id, &state)?;
        Ok(Response::new()
            .add_submessages(submessages)
            .add_attribute("action", "executing_next_stage")
            .add_attribute("stage_index", state.current_stage_index.to_string()))
    }
}
