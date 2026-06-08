use crate::cw20::{BalanceResponse, Cw20ExecuteMsg, Cw20QueryMsg};
use cosmwasm_std::{
    to_json_binary, Addr, BankMsg, Binary, Coin, CosmosMsg, Decimal, DepsMut, Env, MessageInfo,
    Response, StdError, StdResult, Uint128, WasmMsg,
};
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};

use crate::error::ContractError;
use crate::msg::{amm, clmm, Operation, Stage};
use crate::orderbook_exec;
use crate::reply::proceed_to_next_step;
use crate::state::{
    Awaiting, ExecutionState, FlashRepayment, PendingFlashCtx, RoutePlan, CONFIG, FEE_MAP,
    PENDING_FLASH, REPLY_ID_COUNTER, TAX_TOKEN_REGISTRY,
};

pub fn update_admin(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_admin: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;

    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }

    let new_admin_addr = deps.api.addr_validate(&new_admin)?;

    config.admin = new_admin_addr.clone();

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "update_admin")
        .add_attribute("new_admin", new_admin_addr.to_string()))
}

pub fn execute_aggregate_swaps_internal(
    mut deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    stages: Vec<Stage>,
    minimum_receive: Option<Uint128>,
    offer_asset: amm::Asset,
    initiator: Addr,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if offer_asset.amount.is_zero() {
        return Err(ContractError::ZeroAmount {});
    }
    if stages.is_empty() {
        return Err(ContractError::NoStages {});
    }

    let first_stage = stages.first().unwrap();
    let total_percentage: u8 = first_stage.splits.iter().map(|s| s.percent).sum();
    if total_percentage != 100 {
        return Err(ContractError::InvalidPercentageSum {});
    }

    let reply_id = REPLY_ID_COUNTER.update(deps.storage, |id| -> StdResult<_> { Ok(id + 1) })?;

    let minimum_receive = minimum_receive.unwrap_or_default();

    let plan = RoutePlan {
        sender: initiator.clone(),
        minimum_receive,
        stages,
        flash_repayment: None,
    };

    let mut initial_exec_state = ExecutionState {
        plan,
        awaiting: Awaiting::Swaps,
        current_stage_index: 0,
        replies_expected: 0,
        accumulated_assets: vec![offer_asset],
        pending_swaps: vec![],
        pending_path_op: None,
    };

    proceed_to_next_step(&mut deps, env, &mut initial_exec_state, reply_id)
}

/// Entry point for a capital-free CLMM flash-arb. Borrows `flash_amount` of
/// `flash_asset` from `flash_pool` and fires the pool's `Flash {}`; the rest of
/// the cycle runs inside the pool's `FlashCallback` (see [`execute_flash_callback`]).
#[allow(clippy::too_many_arguments)]
pub fn execute_flash_route(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    flash_pool: String,
    flash_asset: amm::AssetInfo,
    flash_amount: Uint128,
    stages: Vec<Stage>,
    min_profit: Uint128,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if flash_amount.is_zero() {
        return Err(ContractError::ZeroAmount {});
    }
    if stages.is_empty() {
        return Err(ContractError::NoStages {});
    }
    let first_stage = stages.first().unwrap();
    let total_percentage: u8 = first_stage.splits.iter().map(|s| s.percent).sum();
    if total_percentage != 100 {
        return Err(ContractError::InvalidPercentageSum {});
    }

    let flash_pool_addr = deps.api.addr_validate(&flash_pool)?;

    // The pool holds its reentrancy lock for the whole callback, so any swap
    // against `flash_pool` inside the cycle would revert the entire transaction.
    // Reject it up-front. Only CLMM hops can hit the flash pool (AMM/orderbook
    // venues have distinct addresses).
    for stage in &stages {
        for split in &stage.splits {
            for op in &split.path {
                if let Operation::ClmmSwap(o) = op {
                    if deps.api.addr_validate(&o.pool_address)? == flash_pool_addr {
                        return Err(ContractError::FlashPoolInCycle {});
                    }
                }
            }
        }
    }

    // Map the borrowed asset onto the pool's token0/token1 to set the loan amounts.
    let config: clmm::ConfigResponse = deps
        .querier
        .query_wasm_smart(&flash_pool_addr, &clmm::ClmmPoolQueryMsg::GetConfig {})?;
    let flash_is_token0 = if flash_asset == config.token0 {
        true
    } else if flash_asset == config.token1 {
        false
    } else {
        return Err(ContractError::FlashAssetNotInPool {});
    };
    let (amount0, amount1) = if flash_is_token0 {
        (flash_amount, Uint128::zero())
    } else {
        (Uint128::zero(), flash_amount)
    };

    PENDING_FLASH.save(
        deps.storage,
        &PendingFlashCtx {
            flash_pool: flash_pool_addr.clone(),
            flash_asset,
            flash_is_token0,
            principal: flash_amount,
            stages,
            min_profit,
            initiator: info.sender,
        },
    )?;

    // `data` is unused — both ends of the callback read `PENDING_FLASH`.
    let flash_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: flash_pool_addr.to_string(),
        msg: to_json_binary(&clmm::ClmmPoolFlashMsg::Flash {
            recipient: env.contract.address.to_string(),
            amount0,
            amount1,
            data: Binary::default(),
        })?,
        funds: vec![],
    });

    Ok(Response::new()
        .add_message(flash_msg)
        .add_attribute("action", "flash_route")
        .add_attribute("flash_pool", flash_pool_addr)
        .add_attribute("flash_amount", flash_amount.to_string()))
}

/// Borrower callback the CLMM pool invokes mid-flash. The loan is already on our
/// balance; we run the arb cycle through the normal route engine, then the final
/// stage repays the pool and forwards the surplus (see `finalize_route`).
pub fn execute_flash_callback(
    mut deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    fee0: Uint128,
    fee1: Uint128,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    // No pending context ⇒ this is not a flash we initiated. Reject before any
    // route runs, so forged callbacks can never spend idle contract balances.
    let ctx = PENDING_FLASH
        .may_load(deps.storage)?
        .ok_or(ContractError::NoPendingFlash {})?;
    PENDING_FLASH.remove(deps.storage);

    if info.sender != ctx.flash_pool {
        return Err(ContractError::Unauthorized {});
    }

    let fee = if ctx.flash_is_token0 { fee0 } else { fee1 };
    let repay_amount = ctx.principal.checked_add(fee).map_err(StdError::from)?;

    let reply_id = REPLY_ID_COUNTER.update(deps.storage, |id| -> StdResult<_> { Ok(id + 1) })?;

    let plan = RoutePlan {
        sender: ctx.initiator,
        minimum_receive: Uint128::zero(),
        stages: ctx.stages,
        flash_repayment: Some(FlashRepayment {
            pool: ctx.flash_pool,
            asset: ctx.flash_asset.clone(),
            repay_amount,
            min_profit: ctx.min_profit,
        }),
    };

    let mut exec_state = ExecutionState {
        plan,
        awaiting: Awaiting::Swaps,
        current_stage_index: 0,
        replies_expected: 0,
        accumulated_assets: vec![amm::Asset {
            info: ctx.flash_asset,
            amount: ctx.principal,
        }],
        pending_swaps: vec![],
        pending_path_op: None,
    };

    proceed_to_next_step(&mut deps, env, &mut exec_state, reply_id)
}

/// Builds the dispatch message for a single hop. Returns `Ok(None)` when the hop
/// provably produces nothing (a CLMM estimation-mode `Quote` of zero), so the
/// caller can finish the split as a graceful zero-value path instead of emitting a
/// submessage. Every other outcome returns `Ok(Some(msg))`.
pub fn create_swap_cosmos_msg(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    operation: &Operation,
    offer_asset_info: &amm::AssetInfo,
    amount: Uint128,
    env: &Env,
) -> Result<Option<CosmosMsg<InjectiveMsgWrapper>>, ContractError> {
    let recipient = env.contract.address.to_string();

    let cosmos_msg = match operation {
        Operation::AmmSwap(amm_op) => {
            let amm_swap_msg = amm::AmmPairExecuteMsg::Swap {
                offer_asset: amm::Asset {
                    info: offer_asset_info.clone(),
                    amount,
                },
                belief_price: None,
                max_spread: None,
                to: Some(recipient),
            };

            match offer_asset_info {
                amm::AssetInfo::NativeToken { denom } => CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: amm_op.pool_address.clone(),
                    msg: to_json_binary(&amm_swap_msg)?,
                    funds: vec![Coin {
                        denom: denom.clone(),
                        amount: amount.into(),
                    }],
                }),
                amm::AssetInfo::Token { contract_addr } => {
                    let token_addr = deps.api.addr_validate(contract_addr)?;
                    if TAX_TOKEN_REGISTRY.has(deps.storage, &token_addr) {
                        // It's a tax token. We must use its tax-exempt send function.
                        CosmosMsg::Wasm(WasmMsg::Execute {
                            contract_addr: contract_addr.clone(),
                            msg: to_json_binary(
                                &crate::msg::reflection::ExecuteMsg::TaxExemptSend {
                                    contract: amm_op.pool_address.clone(),
                                    amount,
                                    msg: to_json_binary(&amm_swap_msg)?,
                                },
                            )?,
                            funds: vec![],
                        })
                    } else {
                        // It's a standard token. Use the normal Cw20::Send.
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
        }
        Operation::OrderbookSwap(ob_op) => {
            // Orderbook hops are placed natively: the contract submits an atomic
            // spot market order that debits its own default subaccount. The order's
            // direction, denoms and ticks are all derived from the market.
            let offer_denom = match offer_asset_info {
                amm::AssetInfo::NativeToken { denom } => denom.clone(),
                _ => {
                    return Err(ContractError::Std(StdError::msg(
                        "Orderbook swaps only support native token inputs.",
                    )))
                }
            };

            let market = orderbook_exec::load_market(deps.as_ref(), &ob_op.market_id)?;

            // The offer must be the side of the market opposite `target_denom`.
            let expected_offer = orderbook_exec::offer_denom_for(&market, &ob_op.target_denom)
                .map_err(|_| ContractError::InvalidOrderbookDenom {
                    denom: ob_op.target_denom.clone(),
                    market_id: ob_op.market_id.as_str().to_string(),
                })?;
            if offer_denom != expected_offer {
                return Err(ContractError::InvalidOrderbookDenom {
                    denom: offer_denom,
                    market_id: ob_op.market_id.as_str().to_string(),
                });
            }

            match orderbook_exec::build_swap_order_msg(
                deps.as_ref(),
                &env.contract.address,
                &market,
                &offer_denom,
                amount,
                ob_op.quantity,
                ob_op.worst_price,
            )? {
                Some(order_msg) => order_msg,
                None => return Err(ContractError::AmountTooSmall {}),
            }
        }
        Operation::ClmmSwap(clmm_op) => {
            // Direct mode: the caller supplied the floor, so skip the per-hop
            // `Quote` re-simulation entirely. Estimation mode: quote the pool and
            // apply 0.5% slippage (and bail to a no-op on a zero-output hop, so a
            // split that can't fill completes gracefully as a zero-value path).
            let minimum_amount_out = match clmm_op.minimum_amount_out {
                Some(min_out) => min_out,
                None => {
                    let quote_query = clmm::ClmmPoolQueryMsg::Quote {
                        token_in: offer_asset_info.clone(),
                        amount_in: amount,
                    };
                    let quote_response: clmm::QuoteResponse = deps
                        .querier
                        .query_wasm_smart(&clmm_op.pool_address, &quote_query)?;

                    // The pool can't fill this hop. Signal a zero-value path to the
                    // caller (no submessage); a self-call no-op would only revert.
                    if quote_response.amount_out.is_zero() {
                        return Ok(None);
                    }

                    quote_response.amount_out.multiply_ratio(995u128, 1000u128)
                }
            };

            let clmm_swap_msg = clmm::ClmmPoolExecuteMsg::SwapExactInput {
                minimum_amount_out,
                recipient: Some(recipient),
                deadline: None,
            };

            match offer_asset_info {
                amm::AssetInfo::NativeToken { denom } => CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: clmm_op.pool_address.clone(),
                    msg: to_json_binary(&clmm_swap_msg)?,
                    funds: vec![Coin {
                        denom: denom.clone(),
                        amount: amount.into(),
                    }],
                }),
                amm::AssetInfo::Token { contract_addr } => {
                    let token_addr = deps.api.addr_validate(contract_addr)?;
                    let hook_msg = clmm::Cw20HookMsg::SwapExactInput {
                        minimum_amount_out,
                        recipient: Some(env.contract.address.to_string()),
                        deadline: None,
                    };
                    if TAX_TOKEN_REGISTRY.has(deps.storage, &token_addr) {
                        CosmosMsg::Wasm(WasmMsg::Execute {
                            contract_addr: contract_addr.clone(),
                            msg: to_json_binary(
                                &crate::msg::reflection::ExecuteMsg::TaxExemptSend {
                                    contract: clmm_op.pool_address.clone(),
                                    amount,
                                    msg: to_json_binary(&hook_msg)?,
                                },
                            )?,
                            funds: vec![],
                        })
                    } else {
                        let cw20_send_msg = Cw20ExecuteMsg::Send {
                            contract: clmm_op.pool_address.clone(),
                            amount,
                            msg: to_json_binary(&hook_msg)?,
                        };
                        CosmosMsg::Wasm(WasmMsg::Execute {
                            contract_addr: contract_addr.clone(),
                            msg: to_json_binary(&cw20_send_msg)?,
                            funds: vec![],
                        })
                    }
                }
            }
        }
    };

    Ok(Some(cosmos_msg))
}

/// Admin-only. Sets or updates the fee for a given pool address.
pub fn set_fee(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    pool_address: String,
    fee_percent: Decimal,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }

    // Validate that the fee is reasonable (e.g., less than 100%)
    if fee_percent >= Decimal::one() {
        return Err(ContractError::Std(StdError::msg(
            "Fee percentage must be less than 100%",
        )));
    }

    let pool_addr = deps.api.addr_validate(&pool_address)?;
    FEE_MAP.save(deps.storage, &pool_addr, &fee_percent)?;

    Ok(Response::new()
        .add_attribute("action", "set_fee")
        .add_attribute("pool_address", pool_addr)
        .add_attribute("fee_percent", fee_percent.to_string()))
}

/// Admin-only. Removes the fee for a given pool address.
pub fn remove_fee(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    pool_address: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }

    let pool_addr = deps.api.addr_validate(&pool_address)?;
    FEE_MAP.remove(deps.storage, &pool_addr);

    Ok(Response::new()
        .add_attribute("action", "remove_fee")
        .add_attribute("pool_address", pool_addr))
}

/// Admin-only. Updates the fee collector address.
pub fn update_fee_collector(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_fee_collector: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }

    let new_collector_addr = deps.api.addr_validate(&new_fee_collector)?;
    config.fee_collector = new_collector_addr.clone();
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "update_fee_collector")
        .add_attribute("new_fee_collector", new_collector_addr))
}

pub fn emergency_withdraw(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    asset_info: amm::AssetInfo,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    // 1. Authorization Check
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }

    let asset_label = match &asset_info {
        amm::AssetInfo::NativeToken { denom } => denom.clone(),
        amm::AssetInfo::Token { contract_addr } => contract_addr.clone(),
    };
    let mut response = Response::new()
        .add_attribute("action", "emergency_withdraw")
        .add_attribute("recipient", info.sender.to_string())
        .add_attribute("asset", asset_label);

    let (amount_to_withdraw, send_msg) = match asset_info {
        amm::AssetInfo::NativeToken { denom } => {
            let balance = deps.querier.query_balance(&env.contract.address, denom)?;
            let msg = if !balance.amount.is_zero() {
                Some(CosmosMsg::Bank(BankMsg::Send {
                    to_address: info.sender.to_string(),
                    amount: vec![balance.clone()],
                }))
            } else {
                None
            };
            // cosmwasm-std 3.0: native balance is Uint256; unify with the cw20 arm's Uint128.
            (
                Uint128::try_from(balance.amount).map_err(StdError::from)?,
                msg,
            )
        }
        amm::AssetInfo::Token { contract_addr } => {
            let balance: BalanceResponse = deps.querier.query_wasm_smart(
                contract_addr.clone(),
                &Cw20QueryMsg::Balance {
                    address: env.contract.address.to_string(),
                },
            )?;
            let msg = if !balance.balance.is_zero() {
                Some(CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr,
                    msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                        recipient: info.sender.to_string(),
                        amount: balance.balance,
                    })?,
                    funds: vec![],
                }))
            } else {
                None
            };
            (balance.balance, msg)
        }
    };

    if let Some(msg) = send_msg {
        response = response.add_message(msg);
    }

    response = response.add_attribute("withdrawn_amount", amount_to_withdraw.to_string());

    Ok(response)
}

pub fn register_tax_token(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    contract_addr: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }
    let addr = deps.api.addr_validate(&contract_addr)?;
    TAX_TOKEN_REGISTRY.save(deps.storage, &addr, &true)?;
    Ok(Response::new()
        .add_attribute("action", "register_tax_token")
        .add_attribute("token_addr", addr))
}

pub fn deregister_tax_token(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    contract_addr: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }
    let addr = deps.api.addr_validate(&contract_addr)?;
    TAX_TOKEN_REGISTRY.remove(deps.storage, &addr);
    Ok(Response::new()
        .add_attribute("action", "deregister_tax_token")
        .add_attribute("token_addr", addr))
}
