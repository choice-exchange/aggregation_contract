use cosmwasm_std::{
    to_json_binary, Addr, BankMsg, Coin, CosmosMsg, Decimal, DepsMut, Env, MessageInfo, Response,
    StdError, StdResult, Uint128, WasmMsg,
};
use crate::cw20::{BalanceResponse, Cw20ExecuteMsg, Cw20QueryMsg};
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};
use injective_math::FPDecimal;

use crate::error::ContractError;
use crate::msg::{self, amm, clmm, orderbook, Operation, Stage};
use crate::reply::proceed_to_next_step;
use crate::state::{
    Awaiting, ExecutionState, RoutePlan, CONFIG, FEE_MAP, REPLY_ID_COUNTER, TAX_TOKEN_REGISTRY,
};

/// Default per-op slippage tolerance (basis points) when an op doesn't supply
/// `max_slippage_bps` — 50 bps = 0.5%, matching the previous hardcoded floor.
pub const DEFAULT_SLIPPAGE_BPS: u16 = 50;
const BPS_DENOM: u128 = 10_000;

/// Apply a basis-point slippage tolerance to derive a minimum-output floor:
/// `amount * (10_000 - bps) / 10_000`. Errors if `bps > 10_000`.
fn min_out_after_slippage(amount: Uint128, bps: u16) -> Result<Uint128, ContractError> {
    if bps as u128 > BPS_DENOM {
        return Err(ContractError::InvalidSlippage { bps });
    }
    Ok(amount.multiply_ratio(BPS_DENOM - bps as u128, BPS_DENOM))
}

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

/// Build the swap message(s) for one planned operation.
///
/// Returns `(primary, extra)`: `primary` is the swap dispatched as a
/// reply-tracked submessage; `extra` are fire-and-forget messages added to the
/// same response without a reply (currently only the unspent-budget refund of a
/// `ClmmSwapExactOutput` leg, which is sent to `initiator`).
pub fn create_swap_cosmos_msg(
    deps: &mut DepsMut<InjectiveQueryWrapper>,
    operation: &Operation,
    offer_asset_info: &amm::AssetInfo,
    amount: Uint128,
    env: &Env,
    initiator: &Addr,
) -> Result<(CosmosMsg<InjectiveMsgWrapper>, Vec<CosmosMsg<InjectiveMsgWrapper>>), ContractError> {
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
            let tick_size_atomic = ob_op.min_quantity_tick_size;

            if tick_size_atomic.is_zero() {
                return Err(ContractError::Std(StdError::msg(
                    "min_quantity_tick_size cannot be zero",
                )));
            }

            let ratio = amount / tick_size_atomic;
            let rounded_atomic_amount = ratio * tick_size_atomic;

            if rounded_atomic_amount.is_zero() {
                return Ok((
                    CosmosMsg::Wasm(WasmMsg::Execute {
                        contract_addr: env.contract.address.to_string(),
                        msg: to_json_binary(&{})?,
                        funds: vec![],
                    }),
                    vec![],
                ));
            }

            let quantity_for_query_fp = FPDecimal::from(rounded_atomic_amount);

            let offer_denom =
                match &ob_op.offer_asset_info {
                    amm::AssetInfo::NativeToken { denom } => denom.clone(),
                    _ => return Err(ContractError::Std(StdError::msg(
                        "This OrderbookSwapOp implementation only supports native token inputs.",
                    ))),
                };

            let target_denom = match &ob_op.ask_asset_info {
                amm::AssetInfo::NativeToken { denom } => denom.clone(),
                _ => {
                    return Err(ContractError::Std(StdError::msg(
                        "Orderbook swaps only support native token (bank) outputs.",
                    )))
                }
            };

            let simulate_msg = msg::orderbook::QueryMsg::GetOutputQuantity {
                from_quantity: quantity_for_query_fp,
                source_denom: offer_denom,
                target_denom: target_denom.clone(),
            };
            let simulation_response: msg::orderbook::SwapEstimationResult = deps
                .querier
                .query_wasm_smart(&ob_op.swap_contract, &simulate_msg)?;
            let expected_output_fp = simulation_response.result_quantity;
            let bps = ob_op.max_slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS);
            if bps as u128 > BPS_DENOM {
                return Err(ContractError::InvalidSlippage { bps });
            }
            // factor = (10_000 - bps) / 10_000
            let slippage_factor =
                FPDecimal::from(BPS_DENOM - bps as u128) / FPDecimal::from(BPS_DENOM);

            let min_output_with_slippage_fp = expected_output_fp * slippage_factor;
            let floored_min_output_fp = min_output_with_slippage_fp.int();

            let swap_msg = orderbook::OrderbookExecuteMsg::SwapMinOutput {
                target_denom,
                min_output_quantity: floored_min_output_fp,
            };

            let funds = vec![Coin {
                denom: match &ob_op.offer_asset_info {
                    amm::AssetInfo::NativeToken { denom } => denom.clone(),
                    _ => unreachable!(),
                },
                amount: rounded_atomic_amount.into(),
            }];

            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: ob_op.swap_contract.clone(),
                msg: to_json_binary(&swap_msg)?,
                funds,
            })
        }
        Operation::ClmmSwap(clmm_op) => {
            // Query the pool for expected output
            let quote_query = clmm::ClmmPoolQueryMsg::Quote {
                token_in: offer_asset_info.clone(),
                amount_in: amount,
            };
            let quote_response: clmm::QuoteResponse = deps
                .querier
                .query_wasm_smart(&clmm_op.pool_address, &quote_query)?;

            if quote_response.amount_out.is_zero() {
                return Ok((
                    CosmosMsg::Wasm(WasmMsg::Execute {
                        contract_addr: env.contract.address.to_string(),
                        msg: to_json_binary(&{})?,
                        funds: vec![],
                    }),
                    vec![],
                ));
            }

            // Caller-supplied per-op slippage (defaults to 0.5%).
            let bps = clmm_op.max_slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS);
            let minimum_amount_out = min_out_after_slippage(quote_response.amount_out, bps)?;

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
        Operation::ClmmSwapExactOutput(eo_op) => {
            // Native input only for now. The op struct is CW20-capable so an
            // allowance-based CW20 path can be added later without a migration.
            let denom = match offer_asset_info {
                amm::AssetInfo::NativeToken { denom } => denom.clone(),
                amm::AssetInfo::Token { .. } => {
                    return Err(ContractError::ExactOutputNativeInputOnly {})
                }
            };

            if eo_op.amount_out.is_zero() {
                return Err(ContractError::ZeroAmount {});
            }

            // Resolve swap direction: zero_for_one means paying token0. We only
            // model token0/token1 of the pool config (serde ignores the rest).
            let config: clmm::ConfigResponse = deps
                .querier
                .query_wasm_smart(&eo_op.pool_address, &clmm::ClmmPoolQueryMsg::GetConfig {})?;
            let zero_for_one = if *offer_asset_info == config.token0 {
                true
            } else if *offer_asset_info == config.token1 {
                false
            } else {
                return Err(ContractError::OperationAssetMismatch {});
            };

            // Quote the exact-output cost at the current pool state. In the
            // common single-leg case there is no state change between this query
            // and execution, so the actual cost equals the quote and the pool
            // refunds nothing. (A same-pool, same-stage second split could move
            // the price and make the pool revert `ExcessiveInput` — safe, rare.)
            let quote: clmm::QuoteResponse = deps.querier.query_wasm_smart(
                &eo_op.pool_address,
                &clmm::ClmmPoolQueryMsg::QuoteExactOutput {
                    token_out: eo_op.ask_asset_info.clone(),
                    amount_out: eo_op.amount_out,
                },
            )?;

            if quote.amount_out < eo_op.amount_out {
                return Err(ContractError::ExactOutputNotFillable {
                    requested: eo_op.amount_out,
                    deliverable: quote.amount_out,
                });
            }
            let cost = quote.amount_in_consumed;
            if cost > amount {
                return Err(ContractError::InsufficientInputBudget {
                    cost,
                    budget: amount,
                });
            }

            let swap_msg = CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: eo_op.pool_address.clone(),
                msg: to_json_binary(&clmm::ClmmPoolExecuteMsg::SwapExactOutput {
                    zero_for_one,
                    amount_out: eo_op.amount_out,
                    maximum_amount_in: cost,
                    recipient: Some(recipient),
                    deadline: None,
                })?,
                funds: vec![Coin {
                    denom: denom.clone(),
                    amount: cost.into(),
                }],
            });

            // Return the unspent budget (input the exact-out leg didn't consume)
            // to the route initiator — it's the user's money, not route output,
            // so it bypasses fees and minimum_receive.
            let mut extra: Vec<CosmosMsg<InjectiveMsgWrapper>> = vec![];
            let refund = amount.checked_sub(cost).map_err(StdError::from)?;
            if !refund.is_zero() {
                extra.push(CosmosMsg::Bank(BankMsg::Send {
                    to_address: initiator.to_string(),
                    amount: vec![Coin {
                        denom,
                        amount: refund.into(),
                    }],
                }));
            }

            return Ok((swap_msg, extra));
        }
    };

    Ok((cosmos_msg, vec![]))
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

    let mut response = Response::new()
        .add_attribute("action", "emergency_withdraw")
        .add_attribute("recipient", info.sender.to_string())
        .add_attribute("asset", format!("{:?}", asset_info));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::{ClmmSwapExactOutputOp, ClmmSwapOp};
    use cosmwasm_std::testing::mock_env;
    use cosmwasm_std::{from_json, ContractResult, QuerierResult, SystemResult};
    use injective_cosmwasm::{mock_dependencies, HandlesSmartQuery};

    const POOL: &str = "pool_addr";

    fn native(d: &str) -> amm::AssetInfo {
        amm::AssetInfo::NativeToken { denom: d.to_string() }
    }

    // --- slippage helper ---------------------------------------------------

    #[test]
    fn slippage_default_is_50_bps() {
        // 0.5% off 1_000_000 → 995_000
        let out = min_out_after_slippage(Uint128::new(1_000_000), DEFAULT_SLIPPAGE_BPS).unwrap();
        assert_eq!(out, Uint128::new(995_000));
    }

    #[test]
    fn slippage_custom_bps() {
        // 1% (100 bps) off 1_000_000 → 990_000
        assert_eq!(
            min_out_after_slippage(Uint128::new(1_000_000), 100).unwrap(),
            Uint128::new(990_000)
        );
        // 100% (10_000 bps) → floor of 0 (accept any output)
        assert_eq!(
            min_out_after_slippage(Uint128::new(1_000_000), 10_000).unwrap(),
            Uint128::zero()
        );
    }

    #[test]
    fn slippage_over_100_pct_rejected() {
        let err = min_out_after_slippage(Uint128::new(1), 10_001).unwrap_err();
        assert!(matches!(err, ContractError::InvalidSlippage { bps: 10_001 }));
    }

    // --- exact-output message builder -------------------------------------

    /// Mock pool: answers GetConfig with the configured token0/token1 and
    /// QuoteExactOutput with the configured cost/deliverable.
    struct ClmmEoQuerier {
        token0: amm::AssetInfo,
        token1: amm::AssetInfo,
        cost: Uint128,
        deliverable: Uint128,
    }
    impl HandlesSmartQuery for ClmmEoQuerier {
        fn handle(&self, _addr: &str, msg: &cosmwasm_std::Binary) -> QuerierResult {
            let bin = match from_json::<clmm::ClmmPoolQueryMsg>(msg).unwrap() {
                clmm::ClmmPoolQueryMsg::GetConfig {} => to_json_binary(&clmm::ConfigResponse {
                    token0: self.token0.clone(),
                    token1: self.token1.clone(),
                })
                .unwrap(),
                clmm::ClmmPoolQueryMsg::QuoteExactOutput { .. } => {
                    to_json_binary(&clmm::QuoteResponse {
                        amount_out: self.deliverable,
                        amount_in_consumed: self.cost,
                        fee_amount: Uint128::zero(),
                    })
                    .unwrap()
                }
                clmm::ClmmPoolQueryMsg::Quote { .. } => unreachable!(),
            };
            SystemResult::Ok(ContractResult::Ok(bin))
        }
    }

    fn eo_op(offer: amm::AssetInfo, ask: amm::AssetInfo, amount_out: u128) -> Operation {
        Operation::ClmmSwapExactOutput(ClmmSwapExactOutputOp {
            pool_address: POOL.to_string(),
            offer_asset_info: offer,
            ask_asset_info: ask,
            amount_out: Uint128::new(amount_out),
        })
    }

    #[test]
    fn exact_output_native_attaches_cost_and_refunds_unspent_budget() {
        let mut deps = mock_dependencies();
        deps.querier.smart_query_handler = Some(Box::new(ClmmEoQuerier {
            token0: native("inj"),
            token1: native("usdt"),
            cost: Uint128::new(100),
            deliverable: Uint128::new(50),
        }));
        let env = mock_env();
        let initiator = Addr::unchecked("initiator");
        let op = eo_op(native("inj"), native("usdt"), 50);

        // Budget 150, cost 100 → attach 100, refund 50 to initiator.
        let (msg, extra) =
            create_swap_cosmos_msg(&mut deps.as_mut(), &op, &native("inj"), Uint128::new(150), &env, &initiator)
                .unwrap();

        match msg {
            CosmosMsg::Wasm(WasmMsg::Execute { contract_addr, msg, funds }) => {
                assert_eq!(contract_addr, POOL);
                assert_eq!(funds, vec![Coin { denom: "inj".to_string(), amount: 100u128.into() }]);
                match from_json::<clmm::ClmmPoolExecuteMsg>(&msg).unwrap() {
                    clmm::ClmmPoolExecuteMsg::SwapExactOutput {
                        zero_for_one,
                        amount_out,
                        maximum_amount_in,
                        ..
                    } => {
                        assert!(zero_for_one, "paying token0 (inj)");
                        assert_eq!(amount_out, Uint128::new(50));
                        assert_eq!(maximum_amount_in, Uint128::new(100));
                    }
                    _ => panic!("expected SwapExactOutput"),
                }
            }
            _ => panic!("expected wasm execute"),
        }

        assert_eq!(extra.len(), 1);
        match &extra[0] {
            CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
                assert_eq!(to_address, "initiator");
                assert_eq!(amount, &vec![Coin { denom: "inj".to_string(), amount: 50u128.into() }]);
            }
            _ => panic!("expected bank refund"),
        }
    }

    #[test]
    fn exact_output_no_refund_when_budget_equals_cost() {
        let mut deps = mock_dependencies();
        deps.querier.smart_query_handler = Some(Box::new(ClmmEoQuerier {
            token0: native("inj"),
            token1: native("usdt"),
            cost: Uint128::new(100),
            deliverable: Uint128::new(50),
        }));
        let env = mock_env();
        let op = eo_op(native("inj"), native("usdt"), 50);
        let (_, extra) = create_swap_cosmos_msg(
            &mut deps.as_mut(),
            &op,
            &native("inj"),
            Uint128::new(100),
            &env,
            &Addr::unchecked("initiator"),
        )
        .unwrap();
        assert!(extra.is_empty(), "no surplus → no refund message");
    }

    #[test]
    fn exact_output_zero_for_one_false_when_paying_token1() {
        let mut deps = mock_dependencies();
        // Pay usdt (token1) to receive inj (token0).
        deps.querier.smart_query_handler = Some(Box::new(ClmmEoQuerier {
            token0: native("inj"),
            token1: native("usdt"),
            cost: Uint128::new(100),
            deliverable: Uint128::new(50),
        }));
        let env = mock_env();
        let op = eo_op(native("usdt"), native("inj"), 50);
        let (msg, _) = create_swap_cosmos_msg(
            &mut deps.as_mut(),
            &op,
            &native("usdt"),
            Uint128::new(100),
            &env,
            &Addr::unchecked("initiator"),
        )
        .unwrap();
        if let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = msg {
            if let clmm::ClmmPoolExecuteMsg::SwapExactOutput { zero_for_one, .. } =
                from_json(&msg).unwrap()
            {
                assert!(!zero_for_one, "paying token1 (usdt) → zero_for_one false");
            } else {
                panic!("expected SwapExactOutput");
            }
        } else {
            panic!("expected wasm execute");
        }
    }

    #[test]
    fn exact_output_rejects_cw20_input() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        let op = eo_op(
            amm::AssetInfo::Token { contract_addr: "cw20".to_string() },
            native("usdt"),
            50,
        );
        let err = create_swap_cosmos_msg(
            &mut deps.as_mut(),
            &op,
            &amm::AssetInfo::Token { contract_addr: "cw20".to_string() },
            Uint128::new(100),
            &env,
            &Addr::unchecked("initiator"),
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::ExactOutputNativeInputOnly {}));
    }

    #[test]
    fn exact_output_rejects_budget_below_cost() {
        let mut deps = mock_dependencies();
        deps.querier.smart_query_handler = Some(Box::new(ClmmEoQuerier {
            token0: native("inj"),
            token1: native("usdt"),
            cost: Uint128::new(200),
            deliverable: Uint128::new(50),
        }));
        let env = mock_env();
        let op = eo_op(native("inj"), native("usdt"), 50);
        let err = create_swap_cosmos_msg(
            &mut deps.as_mut(),
            &op,
            &native("inj"),
            Uint128::new(150), // budget < cost 200
            &env,
            &Addr::unchecked("initiator"),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ContractError::InsufficientInputBudget {
                cost,
                budget
            } if cost == Uint128::new(200) && budget == Uint128::new(150)
        ));
    }

    #[test]
    fn exact_output_rejects_unfillable() {
        let mut deps = mock_dependencies();
        deps.querier.smart_query_handler = Some(Box::new(ClmmEoQuerier {
            token0: native("inj"),
            token1: native("usdt"),
            cost: Uint128::new(100),
            deliverable: Uint128::new(40), // pool can only deliver 40 of requested 50
        }));
        let env = mock_env();
        let op = eo_op(native("inj"), native("usdt"), 50);
        let err = create_swap_cosmos_msg(
            &mut deps.as_mut(),
            &op,
            &native("inj"),
            Uint128::new(150),
            &env,
            &Addr::unchecked("initiator"),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ContractError::ExactOutputNotFillable { requested, deliverable }
            if requested == Uint128::new(50) && deliverable == Uint128::new(40)
        ));
    }

    // --- caller-supplied slippage threads into the exact-input min-out -----

    struct ClmmQuoteQuerier {
        amount_out: Uint128,
    }
    impl HandlesSmartQuery for ClmmQuoteQuerier {
        fn handle(&self, _addr: &str, _msg: &cosmwasm_std::Binary) -> QuerierResult {
            SystemResult::Ok(ContractResult::Ok(
                to_json_binary(&clmm::QuoteResponse {
                    amount_out: self.amount_out,
                    amount_in_consumed: Uint128::new(1_000),
                    fee_amount: Uint128::zero(),
                })
                .unwrap(),
            ))
        }
    }

    #[test]
    fn clmm_exact_input_uses_caller_slippage() {
        let mut deps = mock_dependencies();
        deps.querier.smart_query_handler =
            Some(Box::new(ClmmQuoteQuerier { amount_out: Uint128::new(1_000_000) }));
        let env = mock_env();
        // 200 bps slippage → min_out = 1_000_000 * 9800/10000 = 980_000.
        let op = Operation::ClmmSwap(ClmmSwapOp {
            pool_address: POOL.to_string(),
            offer_asset_info: native("inj"),
            ask_asset_info: native("usdt"),
            max_slippage_bps: Some(200),
        });
        let (msg, _) = create_swap_cosmos_msg(
            &mut deps.as_mut(),
            &op,
            &native("inj"),
            Uint128::new(1_000),
            &env,
            &Addr::unchecked("initiator"),
        )
        .unwrap();
        if let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = msg {
            if let clmm::ClmmPoolExecuteMsg::SwapExactInput { minimum_amount_out, .. } =
                from_json(&msg).unwrap()
            {
                assert_eq!(minimum_amount_out, Uint128::new(980_000));
            } else {
                panic!("expected SwapExactInput");
            }
        } else {
            panic!("expected wasm execute");
        }
    }
}

