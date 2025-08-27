use crate::error::ContractError;
use cosmwasm_std::{DepsMut, Env, Reply, Response, Uint128};
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};
use crate::{state::{REPLY_STATES}};

pub fn handle_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    msg: Reply,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {

    let event = match msg.result.clone().into_result() {
        Ok(result) => result.events.into_iter().find(|e| e.ty.starts_with("wasm")),
        Err(_) => None,
    };

    if event.is_none() {
        return Err(ContractError::Std(cosmwasm_std::StdError::generic_err(
            format!("DEBUG: Failed to find wasm event in reply. Full Reply dump: {:?}", msg)
        )));
    }

    // Load the state for this reply ID
    let mut state = REPLY_STATES.load(deps.storage, msg.id)?;

    // --- Parse the amount returned from the swap ---
    // This part is critical and depends on the swap contract's event logs.
    // We assume the swap contract emits a "wasm" event with a "return_amount" attribute.
    // You MUST verify this for the contracts you integrate with.
    let event = msg
        .result
        .into_result()
        .map_err(|_| ContractError::ReplyParseError {})?
        .events
        .into_iter()
        .find(|e| e.ty.starts_with("wasm")) // Use starts_with for robustness
        .ok_or(ContractError::ReplyParseError {})?;

    let amount_str = event
        .attributes
        .into_iter()
        .find(|attr| attr.key == "return_amount")
        .map(|attr| attr.value)
        .ok_or(ContractError::ReplyParseError {})?;

    let amount_returned: Uint128 = amount_str.parse()?;

    // Update the state
    state.accumulated_amount += amount_returned;
    state.expected_replies -= 1;

    // If this is the last reply, perform the final check
    if state.expected_replies == 0 {
        // Check if the minimum receive amount was met
        if state.accumulated_amount < state.minimum_receive {
            return Err(ContractError::MinimumReceiveNotMet {});
        }

        // Cleanup: remove the temporary state
        REPLY_STATES.remove(deps.storage, msg.id);

        // All good, return a success response
        Ok(Response::new()
            .add_attribute("action", "aggregate_swap_reply_success")
            .add_attribute("sender", state.sender)
            .add_attribute("total_received", state.accumulated_amount))
    } else {
        // Not the last reply, just save the updated state and wait for more
        REPLY_STATES.save(deps.storage, msg.id, &state)?;
        Ok(Response::new())
    }
}

