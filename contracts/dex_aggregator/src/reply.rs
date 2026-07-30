use crate::cw20::Cw20ExecuteMsg;
use cosmwasm_std::{
    to_json_binary, Addr, Coin, CosmosMsg, Deps, DepsMut, Env, Event, Reply, Response, StdError,
    SubMsg, Uint128, WasmMsg,
};
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};

use crate::error::ContractError;
use crate::execute::{create_swap_cosmos_msg, query_asset_balance};
use crate::msg::{amm, clmm, cw20_adapter, Operation, PlannedSwap, Stage, StagePlan};
use injective_math::FPDecimal;
use crate::orderbook_exec;
use crate::state::{
    apply_fee, Awaiting, Config, ExecutionState, PendingPathOp, SubmsgReplyState, SwapLeg,
    ACTIVE_ROUTES, CONFIG, REPLY_ID_COUNTER, SUBMSG_REPLY_STATES, TAX_TOKEN_REGISTRY,
};

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

/// Finish an in-flight split that produced nothing (a zero fill, an IOC no-fill,
/// or a hop whose dispatch resolved to "no message"). Drops the path's pending
/// reply: if it was the last one in the stage, advance to the next step; otherwise
/// keep accumulating the remaining splits.
fn complete_zero_value_path(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: Env,
    exec_state: &mut ExecutionState,
    master_reply_id: u64,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    exec_state.replies_expected -= 1;
    if exec_state.replies_expected == 0 {
        exec_state.current_stage_index += 1;
        proceed_to_next_step(deps, env, exec_state, master_reply_id)
    } else {
        ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;
        Ok(Response::new()
            .add_attribute("action", "accumulating_path_outputs")
            .add_attribute("info", "zero_value_path_completed"))
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
            // A conversion hands real funds to the adapter, so it is bounded like
            // any other outgoing amount.
            let amount = clamp_to_route_funds(
                deps.as_ref(),
                &env,
                exec_state,
                &asset_to_convert.info,
                asset_to_convert.amount,
            )?;
            if amount.is_zero() {
                continue;
            }
            let msg = create_conversion_msg(
                &amm::Asset {
                    info: asset_to_convert.info.clone(),
                    amount,
                },
                &config,
                &env,
            )?;
            conversion_submsgs.push(SubMsg::reply_on_success(msg, master_reply_id));
        }

        if conversion_submsgs.is_empty() {
            return execute_planned_swaps(
                deps,
                env,
                exec_state,
                master_reply_id,
                &stage_plan.swaps_to_execute,
            );
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

    // Clone the replied split's path so the op references below don't hold an
    // immutable borrow of `exec_state` (we mutate `exec_state.pending_fees` while
    // capturing the orderbook surplus).
    let path: Vec<Operation> = {
        let current_stage = exec_state
            .plan
            .stages
            .get(exec_state.current_stage_index as usize)
            .ok_or(ContractError::EmptyRoute {})?;
        current_stage.splits[split_index].path.clone()
    };

    let replied_op = &path[op_index];

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
        Operation::OrderbookSwap(_) => {
            // Cached at dispatch (`DispatchedSwap::ob_is_buy`), where the market was
            // already loaded — this used to re-run `load_market` purely to recover
            // the direction. Structurally always set for an orderbook hop, so a
            // missing value is a bug, not a default to guess at: getting it wrong
            // would invert the fill accounting.
            let is_buy = submsg_state.ob_is_buy.ok_or_else(|| {
                ContractError::Std(StdError::msg(
                    "orderbook reply is missing its cached order direction",
                ))
            })?;
            match orderbook_exec::decode_order_fill(&result)
                .map_err(|e| ContractError::OrderResponseDecode { err: e.to_string() })?
            {
                None => Uint128::zero(),
                Some(fill) => {
                    // Buy hops are sized at `worst_price` so margin == input; the
                    // order fills at cheaper levels and the chain refunds the
                    // difference. Capture that price-improvement surplus as protocol
                    // revenue (it lingers in the offer denom; paid to the fee
                    // collector at finalize). Any *unfilled* remainder is NOT a fee —
                    // it's left in the contract for the user sweep in finalize_route.
                    if is_buy {
                        if let (Some(order_qty), Some(order_price)) =
                            (submsg_state.ob_order_qty, submsg_state.ob_order_price)
                        {
                            let surplus = ob_buy_surplus(
                                submsg_state.in_amount,
                                order_qty,
                                order_price,
                                &fill,
                            );
                            if !surplus.is_zero() {
                                accumulate_fee(
                                    exec_state,
                                    &amm::AssetInfo::NativeToken {
                                        denom: submsg_state.in_denom.clone(),
                                    },
                                    surplus,
                                );
                            }
                        }
                    }
                    let out = if is_buy {
                        fill.quantity
                    } else {
                        fill.quantity * fill.price - fill.fee
                    };
                    if out.is_negative() || out.is_zero() {
                        Uint128::zero()
                    } else {
                        Uint128::from(out)
                    }
                }
            }
        }
        _ => parse_amount_from_swap_reply(&result.events, &env)?,
    };

    // A zero fill (no liquidity, IOC no-fill, or a zero-value path) ends this path
    // without contributing an asset.
    if received_amount.is_zero() {
        return complete_zero_value_path(&mut deps, env, exec_state, master_reply_id);
    }

    // Resolved from the op / the pool's own pair config — never from what the venue
    // claimed in its reply events.
    let received_asset_info = get_operation_output(deps.as_ref(), replied_op)?;

    // Record this venue trade for the terminal `aggregator_swap` event. A
    // fee-bearing terminal hop patches `fee_amount` in below; intermediate and
    // orderbook hops keep zero.
    exec_state.legs.push(SwapLeg {
        kind: operation_kind(replied_op).to_string(),
        venue: get_operation_address(replied_op),
        offer_denom: submsg_state.in_denom.clone(),
        offer_amount: submsg_state.in_amount,
        ask_denom: asset_key(&received_asset_info),
        ask_amount: received_amount,
        fee_amount: Uint128::zero(),
    });

    let replied_path = &path;

    if let Some(next_op) = replied_path.get(op_index + 1) {
        // This is a multi-hop path, proceed to the next operation. The hop's
        // reported output is bounded by what the route can back before any of it is
        // handed to the next venue.
        let carried = clamp_to_route_funds(
            deps.as_ref(),
            &env,
            exec_state,
            &received_asset_info,
            received_amount,
        )?;
        if carried.is_zero() {
            return complete_zero_value_path(&mut deps, env, exec_state, master_reply_id);
        }
        let offer_asset_for_next_op = amm::Asset {
            info: received_asset_info,
            amount: carried,
        };

        // Before dispatching the next message, check for asset mismatch.
        let required_input_info = get_operation_input(deps.as_ref(), next_op)?;
        if offer_asset_for_next_op.info != required_input_info {
            // A mid-path conversion is needed.
            exec_state.awaiting = Awaiting::PathConversion;
            exec_state.pending_path_op = Some(PendingPathOp {
                operation: next_op.clone(),
                amount: carried,
                split_index,
                op_index: op_index + 1,
            });

            let config = CONFIG.load(deps.storage)?;
            let conversion_msg = create_conversion_msg(&offer_asset_for_next_op, &config, &env)?;

            let sub_msg = SubMsg::reply_on_success(conversion_msg, master_reply_id);
            ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;

            return Ok(Response::new()
                .add_submessage(sub_msg)
                .add_attribute("action", "performing_path_conversion"));
        }

        // Create the message for the next step. `None` => the next hop provably
        // yields nothing (e.g. a CLMM quote of zero), so this path ends here as a
        // zero-value path rather than reverting.
        let dispatched = match create_swap_cosmos_msg(
            &mut deps,
            next_op,
            &offer_asset_for_next_op.info,
            offer_asset_for_next_op.amount,
            &env,
        )? {
            Some(d) => d,
            // `None` => the next hop yields nothing; the intermediate already in hand
            // (this op's input denom is snapshotted) is returned to the user by the
            // residue sweep in finalize_route.
            None => return complete_zero_value_path(&mut deps, env, exec_state, master_reply_id),
        };

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
                in_denom: asset_key(&offer_asset_for_next_op.info),
                in_amount: offer_asset_for_next_op.amount,
                ob_order_qty: dispatched.ob_order_qty,
                ob_order_price: dispatched.ob_order_price,
                ob_is_buy: dispatched.ob_is_buy,
            },
        )?;

        let sub_msg = SubMsg::reply_on_success(dispatched.msg, next_submsg_id);

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
                let pool_addr = deps.api.addr_validate(&get_operation_address(replied_op))?;
                let (after_fee, fee) = apply_fee(deps.storage, &pool_addr, received_amount)?;
                (after_fee, fee, Some(pool_addr.to_string()))
            }
        };

        // Patch the fee onto this hop's leg record *before* `proceed_to_next_step`
        // can finalize and emit the `aggregator_swap` event.
        if !fee.is_zero() {
            if let Some(last_leg) = exec_state.legs.last_mut() {
                last_leg.fee_amount = fee;
            }
        }

        exec_state.accumulated_assets.push(amm::Asset {
            info: received_asset_info.clone(),
            amount: amount_after_fee,
        });
        exec_state.replies_expected -= 1;

        // Accrue rather than send. The transfer used to be appended to this
        // response AFTER `proceed_to_next_step`, and submessages recurse
        // depth-first, so the whole remainder of the route — including
        // `finalize_route`'s residue sweep — executed first. The sweep read the
        // still-unsent fee as residue and paid it to the USER; this transfer then
        // found an empty balance and reverted everything. `build_residue_sweep`
        // now pays it out of the same residue it is part of.
        if !fee.is_zero() {
            accumulate_pool_fee(exec_state, &received_asset_info, fee);
        }

        let mut response;
        if exec_state.replies_expected > 0 {
            ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;
            response = Response::new().add_attribute("action", "accumulating_path_outputs");
        } else {
            exec_state.current_stage_index += 1;
            response = proceed_to_next_step(&mut deps, env, exec_state, master_reply_id)?;
        }

        if !fee.is_zero() {
            response = response
                .add_attribute("fee_collected", fee.to_string())
                .add_attribute("fee_pool", fee_pool_label.unwrap_or_default());
        }
        Ok(response)
    }
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
/// The protocol's share of an orderbook BUY's leftover quote: **price improvement
/// on the filled quantity, and nothing else.**
///
/// The order is placed at `order_price` (the `worst_price` bound) and fills at
/// `fill.price`; the difference on the quantity that actually filled is the value
/// the aggregator's routing produced, and is the only "orderbook fee" it takes. It
/// is capped at what the chain actually handed back, so it can never claim more
/// than exists — `ob_buy_surplus` derives the refund in `FPDecimal` and can land a
/// few atomic units above the real figure.
///
/// Everything else in `leftover` belongs to the user and is returned by the residue
/// sweep: the quantity-tick flooring loss (input never committed as margin at all),
/// the `(1 + fee)` sizing headroom, the relayer-fee rebate, and the margin held
/// against any unfilled base.
///
/// ⚠️ The user's share used to be computed as `reserved · (order_qty − filled) /
/// order_qty`, which is exactly ZERO whenever the order fills completely — so on
/// every fully-filled buy the whole leftover went to the fee collector and nothing
/// came back, regardless of how little of the input the order had actually spent.
/// On a single-level fill (where price improvement is provably zero) that measured
/// ~10 bps of the input on a 1000 USDT buy.
///
/// Sells have no such surplus — their price improvement comes out as extra output,
/// which already flows onward to the user.
fn ob_buy_surplus(
    reserved_in: Uint128,
    order_qty: FPDecimal,
    order_price: FPDecimal,
    fill: &orderbook_exec::OrderFill,
) -> Uint128 {
    if order_qty.is_zero() || fill.quantity.is_zero() {
        return Uint128::zero();
    }
    let reserved = FPDecimal::from(reserved_in);
    // What the fill actually consumed of the reserved quote.
    let consumed = fill.quantity * fill.price + fill.fee;
    let leftover = reserved - consumed; // total refund the chain returned
    if leftover.is_negative() || leftover.is_zero() {
        return Uint128::zero();
    }
    let filled = if fill.quantity > order_qty {
        order_qty
    } else {
        fill.quantity
    };
    let improvement = (order_price - fill.price) * filled;
    if improvement.is_negative() || improvement.is_zero() {
        return Uint128::zero();
    }
    // Never claim more than the chain actually refunded.
    let surplus = if improvement > leftover {
        leftover
    } else {
        improvement
    };
    Uint128::from(surplus)
}

/// Accumulate protocol revenue owed in `info`, to be paid to the fee collector at
/// finalize and netted out of that denom's residue sweep.
fn accumulate_fee(exec_state: &mut ExecutionState, info: &amm::AssetInfo, amount: Uint128) {
    if let Some(entry) = exec_state.pending_fees.iter_mut().find(|(i, _)| i == info) {
        entry.1 += amount;
    } else {
        exec_state.pending_fees.push((info.clone(), amount));
    }
}

/// As [`accumulate_fee`], but for the per-pool `FEE_MAP` carve, which a flash route
/// does NOT suppress.
fn accumulate_pool_fee(exec_state: &mut ExecutionState, info: &amm::AssetInfo, amount: Uint128) {
    if let Some(entry) = exec_state
        .pending_pool_fees
        .iter_mut()
        .find(|(i, _)| i == info)
    {
        entry.1 += amount;
    } else {
        exec_state.pending_pool_fees.push((info.clone(), amount));
    }
}

/// Sweep every touched denom out of the contract so a successful route never
/// leaves funds behind: each denom's residue (`current - entry_baseline`) is split
/// into the protocol surplus accrued in it (`pending_fees`, capped at the residue)
/// -> fee collector, and the remainder (unfilled orderbook remainders, dropped
/// intermediates, un-spent split inputs, no-fills) -> the route's sender.
///
/// `final_payout` is the tracked amount of `final_asset` that `finalize_route`
/// disburses separately (the user payment, or flash repay + surplus). It is netted
/// out of that denom's residue instead of skipping the denom wholesale.
///
/// ⚠️ That netting is load-bearing. Skipping `final_asset` outright stranded the
/// orderbook BUY price-improvement refund on every CLOSED cycle: the surplus is
/// accrued in the buy hop's OFFER denom, which for a cycle that returns to its
/// start IS the final asset — so `pending_fees` in it was never paid, and it isn't
/// part of `final_payout` either (a buy's tracked output is base, the refund is
/// quote). It simply accumulated in the contract until an `EmergencyWithdraw`,
/// under-reporting arb profit and tripping `FlashProfitNotMet` on cycles that were
/// genuinely profitable.
///
/// `is_flash` suppresses the protocol carve entirely: a flash-arb caller is an
/// allowlisted signer running their own cycle, so the orderbook buy surplus is
/// their arb profit, not aggregator revenue. (The per-pool `FEE_MAP` carve in
/// `handle_swap_reply` is unaffected and still applies.)
fn build_residue_sweep(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: &Env,
    exec_state: &ExecutionState,
    final_asset: &amm::AssetInfo,
    final_payout: Uint128,
    is_flash: bool,
) -> Result<Vec<CosmosMsg<InjectiveMsgWrapper>>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    let mut msgs: Vec<CosmosMsg<InjectiveMsgWrapper>> = Vec::new();
    for (info, baseline) in &exec_state.entry_balances {
        let current = query_asset_balance(deps.as_ref(), &env.contract.address, info)?;
        let mut residue = current.saturating_sub(*baseline);
        if info == final_asset {
            // Everything ABOVE what we are about to disburse. Saturating: if the
            // tracked amount ever exceeded the real balance the payout itself
            // fails, and we must not sweep against it here.
            residue = residue.saturating_sub(final_payout);
        }
        if residue.is_zero() {
            continue;
        }
        // Per-pool FEE_MAP carve: always owed, flash or not. Orderbook buy-hop
        // price improvement: protocol revenue on a user swap, but the caller's own
        // arb profit on a flash cycle, so suppressed there.
        let mut fee = exec_state
            .pending_pool_fees
            .iter()
            .find(|(i, _)| i == info)
            .map(|(_, a)| *a)
            .unwrap_or_default();
        if !is_flash {
            fee += exec_state
                .pending_fees
                .iter()
                .find(|(i, _)| i == info)
                .map(|(_, a)| *a)
                .unwrap_or_default();
        }
        let fee = fee.min(residue);
        let to_user = residue - fee;
        if !fee.is_zero() {
            msgs.push(create_send_msg(deps, &config.fee_collector, info, fee)?);
        }
        if !to_user.is_zero() {
            msgs.push(create_send_msg(deps, &exec_state.plan.sender, info, to_user)?);
        }
    }
    Ok(msgs)
}

fn finalize_route(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    env: &Env,
    reply_id: u64,
    exec_state: &ExecutionState,
    total_amount: Uint128,
    asset_info: &amm::AssetInfo,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let is_flash = exec_state.plan.flash_repayment.is_some();

    // Orderbook buy-hop price improvement accrued in the OUTPUT denom. On a user
    // swap that is protocol revenue and stays out of the payout. On a flash-arb
    // cycle it is the caller's own arb profit, so credit it into the amount the
    // `min_profit` gate measures — otherwise a cycle whose edge came mostly from
    // buy-side price improvement reverts with `FlashProfitNotMet` despite being
    // genuinely profitable. The refund is already sitting in the contract on top of
    // the tracked output, so it is real, spendable balance.
    //
    // ⚠️ CLAMPED to the balance actually on hand. `ob_buy_surplus` derives the refund
    // in `FPDecimal` from the decoded fill, so it can land a few atomic units ABOVE
    // what the exchange really returned. `build_residue_sweep` was always immune
    // (`.min(residue)`); crediting the raw figure here is not, and over-crediting
    // makes `finalize_route` promise more than it holds — the payout then dies with
    // "insufficient funds" and takes the whole cycle with it. Bound it by the same
    // residue the sweep would have paid out: balance, less the entry baseline, less
    // the tracked output.
    let credited_surplus = if is_flash {
        let accrued = exec_state
            .pending_fees
            .iter()
            .find(|(i, _)| i == asset_info)
            .map(|(_, a)| *a)
            .unwrap_or_default();
        let baseline = exec_state
            .entry_balances
            .iter()
            .find(|(i, _)| i == asset_info)
            .map(|(_, b)| *b)
            .unwrap_or_default();
        let current = query_asset_balance(deps.as_ref(), &env.contract.address, asset_info)?;
        let headroom = current
            .saturating_sub(baseline)
            .saturating_sub(total_amount);
        accrued.min(headroom)
    } else {
        Uint128::zero()
    };
    let total_amount = total_amount
        .checked_add(credited_surplus)
        .map_err(StdError::from)?;

    // The tracked output is a sum of numbers the venues *reported*. Bound it by what
    // the route actually brought in before disbursing any of it, so a hop that
    // over-reported cannot be paid out of balances that predate the route. A no-op
    // on an honest route (the contract necessarily holds at least the tracked
    // output); on a forged one it collapses the payout to what really arrived, and
    // the `minimum_receive` / `min_profit` gate below then rejects the route.
    //
    // `credited_surplus` is added first and is itself bounded by the same headroom,
    // so this cannot claw back a flash cycle's legitimate buy-hop refund.
    let backed = route_spendable(deps.as_ref(), env, exec_state, asset_info)?;
    if total_amount > backed {
        return Err(ContractError::OutputNotBacked {
            asset: asset_key(asset_info),
            wanted: total_amount,
            available: backed,
        });
    }

    // Return everything the route touched but does not deliver as output. Both
    // branches below disburse exactly `total_amount` of `asset_info` (user payment,
    // or flash repay + surplus), so that is what the sweep nets out — including the
    // credited surplus, so it can never be paid twice.
    let sweep_msgs = build_residue_sweep(deps, env, exec_state, asset_info, total_amount, is_flash)?;

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
            .add_messages(sweep_msgs)
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
            .add_messages(sweep_msgs)
            .add_event(build_swap_event(exec_state, asset_info, total_amount))
            .add_attribute("action", "aggregate_swap_complete")
            .add_attribute("final_received", total_amount.to_string()))
    }
}

/// The single consolidated event emitted once per completed user swap. Lets an
/// indexer record one row per route — who swapped what for what — instead of
/// stitching together the underlying pool/market events. `swap_results` carries the
/// per-venue leg breakdown (JSON array of [`SwapLeg`]) for per-pool attribution.
///
/// Field names for the top-line (`sender`, `swap_input_*`, `swap_final_*`,
/// `swap_results`) mirror the legacy `inj-orderbook-swap-contract`
/// `atomic_swap_execution` event so existing indexer plumbing maps over directly.
/// On-chain the event type is `wasm-aggregator_swap`.
fn build_swap_event(
    exec_state: &ExecutionState,
    final_asset: &amm::AssetInfo,
    final_amount: Uint128,
) -> Event {
    let route_json = serde_json_wasm::to_string(&exec_state.legs).unwrap_or_default();
    Event::new("aggregator_swap")
        .add_attribute("sender", exec_state.plan.sender.to_string())
        .add_attribute("recipient", exec_state.plan.sender.to_string())
        .add_attribute("swap_input_denom", asset_key(&exec_state.plan.offer.info))
        .add_attribute(
            "swap_input_amount",
            exec_state.plan.offer.amount.to_string(),
        )
        .add_attribute("swap_final_denom", asset_key(final_asset))
        .add_attribute("swap_final_amount", final_amount.to_string())
        .add_attribute(
            "minimum_receive",
            exec_state.plan.minimum_receive.to_string(),
        )
        .add_attribute("stage_count", exec_state.plan.stages.len().to_string())
        .add_attribute("leg_count", exec_state.legs.len().to_string())
        .add_attribute("swap_results", route_json)
}

/// String key for an asset: the bank denom for natives, the contract address for
/// CW20s. Used in leg records and the consolidated swap event.
fn asset_key(info: &amm::AssetInfo) -> String {
    match info {
        amm::AssetInfo::NativeToken { denom } => denom.clone(),
        amm::AssetInfo::Token { contract_addr } => contract_addr.clone(),
    }
}

/// Venue-kind tag for a leg record: `"amm"`, `"clmm"`, or `"orderbook"`.
fn operation_kind(op: &Operation) -> &'static str {
    match op {
        Operation::AmmSwap(_) => "amm",
        Operation::OrderbookSwap(_) => "orderbook",
        Operation::ClmmSwap(_) => "clmm",
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
            let amount =
                clamp_to_route_funds(deps.as_ref(), &env, exec_state, &asset.info, asset.amount)?;
            if amount.is_zero() {
                continue;
            }
            let msg = create_conversion_msg(
                &amm::Asset {
                    info: asset.info.clone(),
                    amount,
                },
                &config,
                &env,
            )?;
            conversion_submsgs.push(SubMsg::reply_on_success(msg, reply_id));
        }
    }
    // The target side is disbursed from the tracked amount, which `finalize_route`
    // bounds against the same baselines.
    ready_amount = ready_amount.min(route_spendable(
        deps.as_ref(),
        &env,
        exec_state,
        &target_asset_info,
    )?);

    if conversion_submsgs.is_empty() {
        // SCENARIO A: All assets were already the target type. We are done.
        finalize_route(deps, &env, reply_id, exec_state, ready_amount, &target_asset_info)
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
        &env,
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

/// The asset a hop produces, resolved WITHOUT executing it and without trusting
/// anything the venue says at reply time.
///
/// Orderbook output is the native `target_denom`. AMM/CLMM ops take it from the
/// op's own `ask_asset_info` when supplied (free, and what the arb bot should do),
/// otherwise from the pool's `Pair {}` / `GetConfig {}` — the side that isn't the
/// offer, exactly as `SimulateRoute` has always derived it.
///
/// ⚠️ This used to be read back from the reply's `ask_asset` event attribute. That
/// made the *venue* the authority on what it had produced, and `pool_address` is
/// unconstrained: a contract the caller wrote could emit
/// `ask_asset=<any denom> return_amount=<the aggregator's balance>` while
/// transferring nothing, and be paid it. Resolving up-front instead lets
/// `snapshot_entry_balances` record a baseline for the output denom, which is what
/// bounds every later disbursement (see `route_spendable`).
pub(crate) fn get_operation_output(
    deps: Deps<InjectiveQueryWrapper>,
    op: &Operation,
) -> Result<amm::AssetInfo, ContractError> {
    match op {
        Operation::OrderbookSwap(o) => Ok(amm::AssetInfo::NativeToken {
            denom: o.target_denom.clone(),
        }),
        Operation::AmmSwap(o) => match &o.ask_asset_info {
            Some(info) => Ok(info.clone()),
            None => {
                let pair: amm::PairInfo = deps
                    .querier
                    .query_wasm_smart(&o.pool_address, &amm::QueryMsg::Pair {})
                    .map_err(|e| ContractError::UnresolvableAskAsset {
                        venue: o.pool_address.clone(),
                        reason: e.to_string(),
                    })?;
                counter_asset(&pair.asset_infos, &o.offer_asset_info, &o.pool_address)
            }
        },
        Operation::ClmmSwap(o) => match &o.ask_asset_info {
            Some(info) => Ok(info.clone()),
            None => {
                let cfg: clmm::ConfigResponse = deps
                    .querier
                    .query_wasm_smart(&o.pool_address, &clmm::ClmmPoolQueryMsg::GetConfig {})
                    .map_err(|e| ContractError::UnresolvableAskAsset {
                        venue: o.pool_address.clone(),
                        reason: e.to_string(),
                    })?;
                counter_asset(
                    &[cfg.token0, cfg.token1],
                    &o.offer_asset_info,
                    &o.pool_address,
                )
            }
        },
    }
}

/// The side of a pool's asset pair that isn't the offer.
fn counter_asset(
    pair: &[amm::AssetInfo; 2],
    offer: &amm::AssetInfo,
    venue: &str,
) -> Result<amm::AssetInfo, ContractError> {
    if offer == &pair[0] {
        Ok(pair[1].clone())
    } else if offer == &pair[1] {
        Ok(pair[0].clone())
    } else {
        Err(ContractError::UnresolvableAskAsset {
            venue: venue.to_string(),
            reason: format!(
                "offer asset {} is not one of the pool's two assets",
                asset_key(offer)
            ),
        })
    }
}

/// How much of `info` this route may still move: what the contract holds now, less
/// the baseline snapshotted before the route started.
///
/// This is the single invariant that keeps a route inside its own funds. The engine
/// tracks amounts *virtually* — a hop's output is a number reported by the venue —
/// so without this a hop that reports an output it never delivered would have the
/// aggregator spend or pay out the difference from whatever else it happens to
/// hold: protocol fees, dust, another route's in-flight funds under reentrancy.
///
/// Fails closed: an asset with no baseline cannot be bounded, so it cannot be
/// moved. `snapshot_entry_balances` covers every op's input and output plus the
/// offer, which is every asset a route can legitimately touch.
fn route_spendable(
    deps: Deps<InjectiveQueryWrapper>,
    env: &Env,
    exec_state: &ExecutionState,
    info: &amm::AssetInfo,
) -> Result<Uint128, ContractError> {
    let baseline = exec_state
        .entry_balances
        .iter()
        .find(|(i, _)| i == info)
        .map(|(_, b)| *b)
        .ok_or_else(|| ContractError::UnsnapshottedAsset {
            asset: asset_key(info),
        })?;
    let current = query_asset_balance(deps, &env.contract.address, info)?;
    Ok(current.saturating_sub(baseline))
}

/// `amount`, reduced to what the route can actually back. Used on every outgoing
/// amount: swap inputs, conversions, and the final payout.
fn clamp_to_route_funds(
    deps: Deps<InjectiveQueryWrapper>,
    env: &Env,
    exec_state: &ExecutionState,
    info: &amm::AssetInfo,
    amount: Uint128,
) -> Result<Uint128, ContractError> {
    Ok(amount.min(route_spendable(deps, env, exec_state, info)?))
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
    // Resolve each split's offer asset once. For orderbook ops this loads the spot
    // market, so caching it here avoids re-querying the same market in the needs and
    // allocation passes below.
    let offer_infos: Vec<amm::AssetInfo> = next_stage
        .splits
        .iter()
        .map(|split| {
            let first_op = split.path.first().ok_or(ContractError::EmptyRoute {})?;
            get_operation_input(deps, first_op)
        })
        .collect::<Result<_, _>>()?;

    let mut native_info: Option<amm::AssetInfo> = None;
    let mut cw20_info: Option<amm::AssetInfo> = None;

    for offer_info in &offer_infos {
        match offer_info {
            amm::AssetInfo::NativeToken { .. } => {
                if native_info.is_none() {
                    native_info = Some(offer_info.clone());
                }
            }
            amm::AssetInfo::Token { .. } => {
                if cw20_info.is_none() {
                    cw20_info = Some(offer_info.clone());
                }
            }
        }
    }

    // The two piles below are summed as if each side were a single fungible token.
    // That holds only under the adapter identity — a bank denom and its CW20
    // wrapper are the same asset. Two genuinely different natives (or two different
    // CW20s) arriving in one stage would be added together and then allocated to
    // splits as though interchangeable, silently mis-routing. Reject instead.
    let mut native_have = Uint128::zero();
    let mut cw20_have = Uint128::zero();
    let mut seen_native: Option<&str> = None;
    let mut seen_cw20: Option<&str> = None;
    for asset in accumulated_assets {
        match &asset.info {
            amm::AssetInfo::NativeToken { denom } => {
                match seen_native {
                    Some(seen) if seen != denom => {
                        return Err(ContractError::MixedAssetsInStage {
                            kind: "native".to_string(),
                            first: seen.to_string(),
                            second: denom.clone(),
                        })
                    }
                    _ => seen_native = Some(denom),
                }
                native_have += asset.amount;
            }
            amm::AssetInfo::Token { contract_addr } => {
                match seen_cw20 {
                    Some(seen) if seen != contract_addr => {
                        return Err(ContractError::MixedAssetsInStage {
                            kind: "CW20".to_string(),
                            first: seen.to_string(),
                            second: contract_addr.clone(),
                        })
                    }
                    _ => seen_cw20 = Some(contract_addr),
                }
                cw20_have += asset.amount;
            }
        }
    }
    let total_logical_amount = native_have + cw20_have;

    // Allocate FIRST, then derive the conversion needs from the same numbers.
    //
    // ⚠️ These used to be two independent passes: the needs pass gave every split
    // `floor(pct · total / 100)`, while the allocation pass gave the LAST split the
    // remainder `total − Σ(earlier splits)`. Those differ by the rounding remainder
    // R = total − Σ floor(pct_i · total / 100) ∈ [0, splits−1]. When the last split
    // sat on the side being converted AWAY from, exactly R atomic units of its input
    // had been converted into the other asset — so the hop was short and either
    // reverted for insufficient funds or silently ate the contract's own dust.
    let mut per_split: Vec<Uint128> = Vec::with_capacity(next_stage.splits.len());
    let mut allocated = Uint128::zero();
    for (i, split) in next_stage.splits.iter().enumerate() {
        let amount_for_split = if i < next_stage.splits.len() - 1 {
            total_logical_amount.multiply_ratio(split.percent as u128, 100u128)
        } else {
            total_logical_amount
                .checked_sub(allocated)
                .map_err(StdError::from)?
        };
        allocated += amount_for_split;
        per_split.push(amount_for_split);
    }

    let mut total_native_needs = Uint128::zero();
    let mut total_cw20_needs = Uint128::zero();
    for (i, amount_for_split) in per_split.iter().enumerate() {
        match offer_infos[i] {
            amm::AssetInfo::NativeToken { .. } => total_native_needs += *amount_for_split,
            amm::AssetInfo::Token { .. } => total_cw20_needs += *amount_for_split,
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
    for (i, split) in next_stage.splits.iter().enumerate() {
        let first_op = split.path.first().ok_or(ContractError::EmptyRoute {})?;
        swaps_to_execute.push(PlannedSwap {
            operation: first_op.clone(),
            // Same vector the conversion sizing above was derived from.
            amount: per_split[i],
            split_index: i,
            op_index: 0,
            // Already resolved above (loads the spot market once for orderbook ops).
            offer_info: offer_infos[i].clone(),
        });
    }

    Ok(StagePlan {
        swaps_to_execute,
        conversions_needed,
    })
}

pub(crate) fn get_operation_input(
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

    // Per-denom budget for this stage. Every split's input is drawn from what the
    // route can actually back, and the draw is decremented as splits are planned —
    // so a stage whose allocation was inflated by an over-reporting hop cannot
    // dispatch more than once against the same real funds.
    let mut budgets: Vec<(amm::AssetInfo, Uint128)> = Vec::new();

    for swap in swaps.iter().filter(|s| !s.amount.is_zero()) {
        // Resolved at plan time (see `PlannedSwap.offer_info`) — avoids re-running
        // `get_operation_input`, which for an orderbook op is a `load_market` query.
        let offer_asset_info = swap.offer_info.clone();

        if !budgets.iter().any(|(i, _)| *i == offer_asset_info) {
            let available = route_spendable(deps.as_ref(), &env, exec_state, &offer_asset_info)?;
            budgets.push((offer_asset_info.clone(), available));
        }
        let budget = budgets
            .iter_mut()
            .find(|(i, _)| *i == offer_asset_info)
            .expect("budget entry just inserted");
        let amount = swap.amount.min(budget.1);
        if amount.is_zero() {
            continue;
        }
        budget.1 -= amount;

        // `None` => this hop provably yields nothing; skip the split entirely
        // (don't burn a reply id or persist submsg state for a message we never send).
        let dispatched =
            match create_swap_cosmos_msg(deps, &swap.operation, &offer_asset_info, amount, &env)? {
                Some(d) => d,
                // `None` => the hop provably yields nothing and is not dispatched. The
                // allocated input stays in the contract (offer denom is snapshotted) and
                // is returned to the user by the residue sweep in finalize_route.
                None => continue,
            };

        reply_id_counter += 1;
        let submsg_id = reply_id_counter;

        SUBMSG_REPLY_STATES.save(
            deps.storage,
            submsg_id,
            &SubmsgReplyState {
                master_reply_id,
                split_index: swap.split_index,
                op_index: swap.op_index,
                in_denom: asset_key(&offer_asset_info),
                in_amount: amount,
                ob_order_qty: dispatched.ob_order_qty,
                ob_order_price: dispatched.ob_order_price,
                ob_is_buy: dispatched.ob_is_buy,
            },
        )?;

        submessages.push(SubMsg::reply_on_success(dispatched.msg, submsg_id));
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

    // Recorded when the conversion was dispatched. Previously this searched the plan
    // for the first `Operation` equal to the pending one, which mis-attributed the
    // resumed hop whenever the same op appeared in two splits.
    let (split_index, op_index) = (pending_op_details.split_index, pending_op_details.op_index);

    let converted_asset_info = get_operation_input(deps.as_ref(), &pending_op_details.operation)?;
    let converted_amount = clamp_to_route_funds(
        deps.as_ref(),
        &env,
        exec_state,
        &converted_asset_info,
        converted_amount,
    )?;
    if converted_amount.is_zero() {
        return complete_zero_value_path(&mut deps, env, exec_state, master_reply_id);
    }
    // `None` => the resumed hop provably yields nothing; end the path gracefully.
    let dispatched = match create_swap_cosmos_msg(
        &mut deps,
        &pending_op_details.operation,
        &converted_asset_info,
        converted_amount,
        &env,
    )? {
        Some(d) => d,
        None => return complete_zero_value_path(&mut deps, env, exec_state, master_reply_id),
    };

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
            in_denom: asset_key(&converted_asset_info),
            in_amount: converted_amount,
            ob_order_qty: dispatched.ob_order_qty,
            ob_order_price: dispatched.ob_order_price,
            ob_is_buy: dispatched.ob_is_buy,
        },
    )?;

    let sub_msg = SubMsg::reply_on_success(dispatched.msg, submsg_id);

    exec_state.awaiting = Awaiting::Swaps;
    ACTIVE_ROUTES.save(deps.storage, master_reply_id, exec_state)?;

    Ok(Response::new()
        .add_submessage(sub_msg)
        .add_attribute("action", "resuming_path_after_conversion"))
}
