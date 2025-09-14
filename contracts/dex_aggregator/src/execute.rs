use cosmwasm_std::{
    to_json_binary, Addr, Coin, CosmosMsg, Decimal, DepsMut, Env, MessageInfo, Response, StdError,
    Uint128, WasmMsg,
};
use cw20::Cw20ExecuteMsg;
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};
use injective_math::FPDecimal;
use std::str::FromStr;

use crate::error::ContractError;
use crate::msg::{self, external, AmmPairExecuteMsg, Operation, OrderbookExecuteMsg, Stage};
use crate::reply::proceed_to_next_step;
use crate::state::{
    Awaiting, ExecutionState, RoutePlan, CONFIG, FEE_MAP, REPLY_ID_COUNTER, ROUTE_PLANS,
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
    _info: MessageInfo,
    stages: Vec<Stage>,
    minimum_receive_str: Option<String>,
    offer_asset: external::Asset,
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

    let reply_id = REPLY_ID_COUNTER.may_load(deps.storage)?.unwrap_or(0) + 1;
    REPLY_ID_COUNTER.save(deps.storage, &reply_id)?;

    let minimum_receive = match minimum_receive_str {
        Some(s) => Uint128::from_str(&s)?,
        None => Uint128::zero(),
    };

    let plan = RoutePlan {
        sender: initiator.clone(),
        minimum_receive,
        stages,
    };
    ROUTE_PLANS.save(deps.storage, reply_id, &plan)?;

    let mut initial_exec_state = ExecutionState {
        awaiting: Awaiting::Swaps,
        current_stage_index: 0,
        replies_expected: 0,
        accumulated_assets: vec![offer_asset],
        pending_swaps: vec![],
        pending_path_op: None,
    };

    proceed_to_next_step(&mut deps, env, &mut initial_exec_state, &plan, reply_id)
}

pub fn create_swap_cosmos_msg(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    operation: &Operation,
    offer_asset_info: &external::AssetInfo,
    amount: Uint128,
    env: &Env,
) -> Result<CosmosMsg<InjectiveMsgWrapper>, ContractError> {
    let recipient = env.contract.address.to_string();

    let cosmos_msg = match operation {
        Operation::AmmSwap(amm_op) => {
            let amm_swap_msg = AmmPairExecuteMsg::Swap {
                offer_asset: external::Asset {
                    info: offer_asset_info.clone(),
                    amount,
                },
                belief_price: None,
                max_spread: None,
                to: Some(recipient),
                deadline: None,
            };

            match offer_asset_info {
                external::AssetInfo::NativeToken { denom } => CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: amm_op.pool_address.clone(),
                    msg: to_json_binary(&amm_swap_msg)?,
                    funds: vec![Coin {
                        denom: denom.clone(),
                        amount,
                    }],
                }),
                external::AssetInfo::Token { contract_addr } => {
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
        Operation::OrderbookSwap(ob_op) => {
            let offer_denom =
                match &ob_op.offer_asset_info {
                    external::AssetInfo::NativeToken { denom } => denom.clone(),
                    _ => return Err(ContractError::Std(StdError::generic_err(
                        "This OrderbookSwapOp implementation only supports native token inputs.",
                    ))),
                };

            let target_denom = match &ob_op.ask_asset_info {
                external::AssetInfo::NativeToken { denom } => denom.clone(),
                _ => {
                    return Err(ContractError::Std(StdError::generic_err(
                        "Orderbook swaps only support native token (bank) outputs.",
                    )))
                }
            };

            let simulate_msg = msg::orderbook::QueryMsg::GetOutputQuantity {
                from_quantity: amount.into(),
                source_denom: offer_denom,
                target_denom: target_denom.clone(),
            };
            let simulation_response: msg::orderbook::SwapEstimationResult = deps
                .querier
                .query_wasm_smart(&ob_op.swap_contract, &simulate_msg)?;
            let expected_output_fp = simulation_response.result_quantity;
            let slippage = FPDecimal::from_str("0.005")?;
            let min_output_fp = expected_output_fp * (FPDecimal::ONE - slippage);
            let swap_msg = OrderbookExecuteMsg::SwapMinOutput {
                target_denom,
                min_output_quantity: min_output_fp,
            };

            let funds = vec![Coin {
                denom: match &ob_op.offer_asset_info {
                    external::AssetInfo::NativeToken { denom } => denom.clone(),
                    _ => unreachable!(),
                },
                amount,
            }];

            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: ob_op.swap_contract.clone(),
                msg: to_json_binary(&swap_msg)?,
                funds,
            })
        }
    };

    Ok(cosmos_msg)
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
        return Err(ContractError::Std(StdError::generic_err(
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
