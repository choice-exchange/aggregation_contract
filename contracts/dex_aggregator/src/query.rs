use crate::msg::{
    amm, clmm, AllFeesResponse, FeeInfo, FeeResponse, FlashSignersResponse, IsFlashSignerResponse,
    Operation, SimulateRouteResponse, Stage,
};
use crate::orderbook_exec::{self, FPCoin};
use crate::state::{apply_fee, Config, FEE_MAP, FLASH_SIGNERS, FLASH_UNRESTRICTED};
use cosmwasm_std::{
    to_json_binary, Addr, Binary, Coin, Deps, Env, Order, StdError, StdResult, Uint128, WasmQuery,
};
use cw_storage_plus::Bound;
use injective_cosmwasm::InjectiveQueryWrapper;

pub fn query_config(deps: Deps) -> StdResult<Binary> {
    let config: Config = crate::state::CONFIG.load(deps.storage)?;
    to_json_binary(&config)
}

/// Canonical identity of an asset under the CW20 adapter: a CW20 and the bank denom
/// the adapter wraps it into (`factory/<adapter>/<cw20>`) are the same asset, which
/// is the assumption the executor's stage allocator is built on.
fn canonical_key(info: &amm::AssetInfo, adapter: &Addr) -> String {
    match info {
        amm::AssetInfo::Token { contract_addr } => contract_addr.clone(),
        amm::AssetInfo::NativeToken { denom } => denom
            .strip_prefix(&format!("factory/{adapter}/"))
            .map(|s| s.to_string())
            .unwrap_or_else(|| denom.clone()),
    }
}

pub fn simulate_route(
    deps: Deps<InjectiveQueryWrapper>,
    env: Env,
    stages: Vec<Stage>,
    amount_in: Coin,
) -> StdResult<Binary> {
    if stages.is_empty() {
        return to_json_binary(&SimulateRouteResponse {
            output_amount: Uint128::zero(),
        });
    }

    // The gate must reject every shape execution rejects, or it clears routes that
    // are guaranteed to revert. Percentage sums were previously checked on the
    // executor's first stage only, and never here at all.
    crate::execute::validate_stages(&stages).map_err(|e| StdError::msg(e.to_string()))?;

    let adapter = crate::state::CONFIG.load(deps.storage)?.cw20_adapter_address;

    let mut current_assets: Vec<amm::Asset> = vec![amm::Asset {
        info: amm::AssetInfo::NativeToken {
            denom: amount_in.denom,
        },
        // cosmwasm-std 3.0: Coin.amount is Uint256; our assets are Uint128.
        amount: Uint128::try_from(amount_in.amount).map_err(StdError::from)?,
    }];

    for stage in stages {
        let mut next_stage_outputs: Vec<amm::Asset> = vec![];

        // Mirror `plan_next_stage` exactly: one native pile plus one CW20 pile,
        // summed into a single logical amount under adapter identity, and rejected
        // outright if two genuinely different natives (or CW20s) arrive together.
        //
        // ⚠️ This used to allocate each split from the pile matching that split's OWN
        // input asset. That both quoted multi-denom stages the executor now refuses
        // with `MixedAssetsInStage`, and quoted ~zero for any split whose input the
        // executor would have produced by an adapter conversion.
        let mut native_have = Uint128::zero();
        let mut cw20_have = Uint128::zero();
        let mut seen_native: Option<String> = None;
        let mut seen_cw20: Option<String> = None;
        for asset in &current_assets {
            match &asset.info {
                amm::AssetInfo::NativeToken { denom } => {
                    match &seen_native {
                        Some(seen) if seen != denom => {
                            return Err(StdError::msg(format!(
                                "stage received two different native assets ({seen} and {denom})"
                            )))
                        }
                        _ => seen_native = Some(denom.clone()),
                    }
                    native_have += asset.amount;
                }
                amm::AssetInfo::Token { contract_addr } => {
                    match &seen_cw20 {
                        Some(seen) if seen != contract_addr => {
                            return Err(StdError::msg(format!(
                                "stage received two different CW20 assets ({seen} and {contract_addr})"
                            )))
                        }
                        _ => seen_cw20 = Some(contract_addr.clone()),
                    }
                    cw20_have += asset.amount;
                }
            }
        }
        let total_logical_amount = native_have + cw20_have;

        let mut allocated = Uint128::zero();

        for (i, split) in stage.splits.iter().enumerate() {
            let path_input_info = get_path_start_info(deps, &split.path)?;

            // Identical to the executor: every split draws from the combined pile,
            // and the last one absorbs the rounding remainder.
            let amount_for_split = if i < stage.splits.len() - 1 {
                total_logical_amount.multiply_ratio(split.percent as u128, 100u128)
            } else {
                total_logical_amount
                    .checked_sub(allocated)
                    .map_err(StdError::from)?
            };
            allocated += amount_for_split;

            let mut current_path_asset = amm::Asset {
                info: path_input_info,
                amount: amount_for_split,
            };

            for operation in &split.path {
                let output_asset =
                    simulate_single_operation(deps, &env, operation, &current_path_asset)?;
                current_path_asset = output_asset;
            }

            // Mirror the executor: the aggregator's per-pool fee (FEE_MAP) is
            // deducted once at each split path's terminal hop (see
            // `handle_swap_reply` in reply.rs). Orderbook terminals carry no
            // aggregator fee. Without this the simulation over-reports output vs
            // the executed fill for any pool with a configured fee.
            let fee_pool: Option<String> = match split.path.last() {
                Some(Operation::AmmSwap(o)) => Some(o.pool_address.clone()),
                Some(Operation::ClmmSwap(o)) => Some(o.pool_address.clone()),
                Some(Operation::OrderbookSwap(_)) | None => None,
            };
            if let Some(addr) = fee_pool {
                // Read-only fee lookup: `Addr::unchecked` yields the same FEE_MAP
                // storage key as the validated address the executor uses (an
                // invalid address simply isn't in the map → zero fee), so we skip
                // bech32 validation here.
                let pool_addr = Addr::unchecked(addr);
                let (after_fee, _fee) =
                    apply_fee(deps.storage, &pool_addr, current_path_asset.amount)?;
                current_path_asset.amount = after_fee;
            }

            next_stage_outputs.push(current_path_asset);
        }

        current_assets = next_stage_outputs;
    }

    // `handle_final_stage` normalizes every accumulated asset onto the first one via
    // the adapter (1:1), so the outputs may only be summed when they are the same
    // asset under adapter identity. Summing unconditionally reported 100 USDT plus
    // 5 INJ as "105" — a number with no meaning, for a route the executor would have
    // reverted on when the adapter refused the conversion.
    let mut total_output = Uint128::zero();
    let target_key = current_assets
        .first()
        .map(|a| canonical_key(&a.info, &adapter));
    for asset in &current_assets {
        let key = canonical_key(&asset.info, &adapter);
        if Some(&key) != target_key.as_ref() {
            return Err(StdError::msg(format!(
                "route ends in two different assets ({} and {key}); they cannot be summed",
                target_key.unwrap_or_default()
            )));
        }
        total_output += asset.amount;
    }

    let response = SimulateRouteResponse {
        output_amount: total_output,
    };
    to_json_binary(&response)
}

/// Simulates a single swap operation.
fn simulate_single_operation(
    deps: Deps<InjectiveQueryWrapper>,
    env: &Env,
    operation: &Operation,
    offer_asset: &amm::Asset,
) -> StdResult<amm::Asset> {
    match operation {
        Operation::AmmSwap(op) => {
            let pair_query = amm::QueryMsg::Simulation {
                offer_asset: offer_asset.clone(),
            };
            let contract_addr = op.pool_address.to_string();

            let sim_response: amm::SimulationResponse = deps.querier.query(
                &WasmQuery::Smart {
                    contract_addr,
                    msg: to_json_binary(&pair_query)?,
                }
                .into(),
            )?;

            // The op no longer carries the output asset; derive it from the pair
            // (the side that isn't the offer). Read-only query, so the extra
            // `Pair {}` call is acceptable here (execution reads it from the event).
            let pair_info: amm::PairInfo = deps
                .querier
                .query_wasm_smart(&op.pool_address, &amm::QueryMsg::Pair {})?;
            let ask_info = counter_asset(&pair_info.asset_infos, &offer_asset.info)?;

            Ok(amm::Asset {
                info: ask_info,
                amount: sim_response.return_amount,
            })
        }
        Operation::OrderbookSwap(op) => {
            // Same estimator the execution path uses, so the quote can't diverge
            // from the fill. `result_quantity` is already in `target_denom`
            // (base for a buy, quote-minus-fee for a sell).
            let source_denom = match &offer_asset.info {
                amm::AssetInfo::NativeToken { denom } => denom.clone(),
                _ => {
                    return Err(StdError::msg(
                        "Orderbook simulation only supports native token inputs",
                    ))
                }
            };

            let market = orderbook_exec::load_market(deps, &op.market_id)?;
            let expected_offer = orderbook_exec::offer_denom_for(&market, &op.target_denom)?;
            if source_denom != expected_offer {
                return Err(StdError::msg(format!(
                    "offer denom {source_denom} is not valid for orderbook market {}",
                    op.market_id.as_str()
                )));
            }

            // Direct mode fixes the order outright, so quoting it by walking the
            // book describes a DIFFERENT order than the one that will be submitted.
            // Size it through the same helper the executor builds the order with.
            let est = match (op.quantity, op.worst_price) {
                (Some(q), Some(p)) => orderbook_exec::direct_mode_estimate(
                    &deps,
                    &market,
                    &source_denom,
                    offer_asset.amount,
                    q,
                    p,
                )?,
                _ => orderbook_exec::estimate_single_swap_execution(
                    &deps,
                    &env.contract.address,
                    &market,
                    FPCoin {
                        amount: offer_asset.amount.into(),
                        denom: source_denom,
                    },
                    true,
                )?,
            };

            Ok(amm::Asset {
                info: amm::AssetInfo::NativeToken {
                    denom: op.target_denom.clone(),
                },
                amount: est.result_quantity.into(),
            })
        }
        Operation::ClmmSwap(op) => {
            let quote_query = clmm::ClmmPoolQueryMsg::Quote {
                token_in: offer_asset.info.clone(),
                amount_in: offer_asset.amount,
            };
            let contract_addr = op.pool_address.to_string();

            let quote_response: clmm::QuoteResponse = deps.querier.query(
                &WasmQuery::Smart {
                    contract_addr,
                    msg: to_json_binary(&quote_query)?,
                }
                .into(),
            )?;

            // Derive the output asset from the pool config (the token that isn't
            // the offer); read-only, so the extra `GetConfig {}` query is fine.
            let config: clmm::ConfigResponse = deps
                .querier
                .query_wasm_smart(&op.pool_address, &clmm::ClmmPoolQueryMsg::GetConfig {})?;
            let ask_info = counter_asset(&[config.token0, config.token1], &offer_asset.info)?;

            Ok(amm::Asset {
                info: ask_info,
                amount: quote_response.amount_out,
            })
        }
    }
}

/// Given a pool's two assets and the offer side, return the *other* side (the
/// asset the hop produces). Errors if the offer isn't one of the pair.
fn counter_asset(pair: &[amm::AssetInfo; 2], offer: &amm::AssetInfo) -> StdResult<amm::AssetInfo> {
    if *offer == pair[0] {
        Ok(pair[1].clone())
    } else if *offer == pair[1] {
        Ok(pair[0].clone())
    } else {
        Err(StdError::msg(
            "offer asset is not part of the pool's asset pair",
        ))
    }
}

fn get_path_start_info(
    deps: Deps<InjectiveQueryWrapper>,
    path: &[Operation],
) -> StdResult<amm::AssetInfo> {
    let first_op = path
        .first()
        .ok_or_else(|| StdError::msg("Path cannot be empty"))?;
    Ok(match first_op {
        Operation::AmmSwap(op) => op.offer_asset_info.clone(),
        Operation::OrderbookSwap(op) => {
            let market = orderbook_exec::load_market(deps, &op.market_id)?;
            amm::AssetInfo::NativeToken {
                denom: orderbook_exec::offer_denom_for(&market, &op.target_denom)?,
            }
        }
        Operation::ClmmSwap(op) => op.offer_asset_info.clone(),
    })
}

/// Queries the fee percentage for a specific pool address.
pub fn query_fee_for_pool(deps: Deps, pool_address: String) -> StdResult<Binary> {
    let pool_addr = deps.api.addr_validate(&pool_address)?;
    let fee = FEE_MAP.may_load(deps.storage, &pool_addr)?;

    to_json_binary(&FeeResponse { fee })
}

// Pagination constants
const DEFAULT_LIMIT: u32 = 10;
const MAX_LIMIT: u32 = 30;

/// Queries all configured fees with pagination.
pub fn query_all_fees(
    deps: Deps,
    start_after: Option<String>,
    limit: Option<u32>,
) -> StdResult<Binary> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;

    // Validate the start_after address if provided
    let start = start_after
        .map(|addr| deps.api.addr_validate(&addr))
        .transpose()?;

    let fees: Vec<FeeInfo> = FEE_MAP
        .range(
            deps.storage,
            start.as_ref().map(Bound::exclusive), // Use exclusive bound for start_after
            None,
            Order::Ascending,
        )
        .take(limit)
        .map(|item| {
            let (pool_addr, fee_fraction) = item?;
            Ok(FeeInfo {
                pool_address: pool_addr.to_string(),
                fee_fraction,
            })
        })
        .collect::<StdResult<_>>()?;

    to_json_binary(&AllFeesResponse { fees })
}

/// Whether `signer` may call `FlashRoute` (explicitly allowlisted, or flash is
/// unrestricted).
pub fn query_is_flash_signer(deps: Deps, signer: String) -> StdResult<Binary> {
    let signer_addr = deps.api.addr_validate(&signer)?;
    let unrestricted = FLASH_UNRESTRICTED.may_load(deps.storage)?.unwrap_or(false);
    let authorized = unrestricted || FLASH_SIGNERS.has(deps.storage, &signer_addr);
    to_json_binary(&IsFlashSignerResponse { authorized })
}

/// Lists allowlisted flash signers (paginated) plus the unrestricted flag.
pub fn query_flash_signers(
    deps: Deps,
    start_after: Option<String>,
    limit: Option<u32>,
) -> StdResult<Binary> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let start = start_after
        .map(|addr| deps.api.addr_validate(&addr))
        .transpose()?;

    let signers: Vec<String> = FLASH_SIGNERS
        .range(
            deps.storage,
            start.as_ref().map(Bound::exclusive),
            None,
            Order::Ascending,
        )
        .take(limit)
        .map(|item| {
            let (signer_addr, _) = item?;
            Ok(signer_addr.to_string())
        })
        .collect::<StdResult<_>>()?;

    let unrestricted = FLASH_UNRESTRICTED.may_load(deps.storage)?.unwrap_or(false);
    to_json_binary(&FlashSignersResponse {
        signers,
        unrestricted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::query;
    use crate::msg::{AmmSwapOp, QueryMsg, Split, Stage};
    use amm::AssetInfo;
    use cosmwasm_std::testing::{mock_env, MockApi, MockQuerier, MockStorage};
    use cosmwasm_std::{from_json, ContractResult, Decimal, OwnedDeps, SystemResult};
    use std::marker::PhantomData;
    use std::str::FromStr;

    const POOL_A_ADDR: &str = "inj1hkhdaj2ts42k2x53h3w0f26g2xvy3a52e0u4gp";
    const POOL_B_ADDR: &str = "inj12sqy2n5qt52n5q2n5qt52n5q2n5qt52n5q2n5qt";

    /// Injective-typed mock deps (the query path now needs `Deps<InjectiveQueryWrapper>`).
    /// Orderbook isn't exercised here, so the default wasm-only `MockQuerier` suffices.
    fn inj_mock_deps(
    ) -> OwnedDeps<MockStorage, MockApi, MockQuerier<InjectiveQueryWrapper>, InjectiveQueryWrapper>
    {
        let mut deps = OwnedDeps {
            storage: MockStorage::default(),
            api: MockApi::default(),
            querier: MockQuerier::new(&[]),
            custom_query_type: PhantomData,
        };
        // `simulate_route` reads the adapter address to decide whether two assets are
        // the same asset under adapter identity.
        crate::state::CONFIG
            .save(
                &mut deps.storage,
                &Config {
                    admin: Addr::unchecked("inj1admin"),
                    cw20_adapter_address: Addr::unchecked("inj1adapter"),
                    fee_collector: Addr::unchecked("inj1collector"),
                },
            )
            .unwrap();
        deps
    }

    /// Build an `amm::PairInfo` binary for the given two assets.
    fn pair_binary(a0: AssetInfo, a1: AssetInfo) -> Binary {
        to_json_binary(&amm::PairInfo {
            asset_infos: [a0, a1],
        })
        .unwrap()
    }

    fn native(denom: &str) -> AssetInfo {
        AssetInfo::NativeToken {
            denom: denom.to_string(),
        }
    }

    #[test]
    fn test_simulate_simple_path() {
        let mut querier: MockQuerier<InjectiveQueryWrapper> = MockQuerier::new(&[]);
        let mock_response = amm::SimulationResponse {
            return_amount: Uint128::new(50000),
            spread_amount: Uint128::zero(),
            commission_amount: Uint128::zero(),
        };
        let mock_response_binary = to_json_binary(&mock_response).unwrap();

        querier.update_wasm(
            move |query: &WasmQuery| -> SystemResult<ContractResult<Binary>> {
                match query {
                    WasmQuery::Smart { contract_addr, msg } => {
                        if contract_addr != POOL_A_ADDR {
                            panic!("Unexpected contract call to {}", contract_addr);
                        }
                        match from_json::<amm::QueryMsg>(msg).unwrap() {
                            amm::QueryMsg::Simulation { .. } => {
                                SystemResult::Ok(ContractResult::Ok(mock_response_binary.clone()))
                            }
                            amm::QueryMsg::Pair {} => SystemResult::Ok(ContractResult::Ok(
                                pair_binary(native("inj"), native("usdt")),
                            )),
                        }
                    }
                    _ => panic!("Unsupported query type"),
                }
            },
        );
        let mut deps = inj_mock_deps();
        deps.querier = querier;

        let stages = vec![Stage {
            splits: vec![Split {
                percent: 100,
                path: vec![Operation::AmmSwap(AmmSwapOp { ask_asset_info: None, max_spread: None,
                    pool_address: POOL_A_ADDR.to_string(),
                    offer_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                })],
            }],
        }];

        let result_binary = simulate_route(
            deps.as_ref(),
            mock_env(),
            stages,
            Coin::new(1000u128, "inj"),
        )
        .unwrap();
        let result: SimulateRouteResponse = from_json(&result_binary).unwrap();
        assert_eq!(result.output_amount, Uint128::new(50000));
    }

    #[test]
    fn test_simulate_multi_hop_path() {
        let mut querier: MockQuerier<InjectiveQueryWrapper> = MockQuerier::new(&[]);

        let mock_response_hop1 = amm::SimulationResponse {
            return_amount: Uint128::new(20000), // 1000 INJ -> 20000 USDT
            spread_amount: Uint128::zero(),
            commission_amount: Uint128::zero(),
        };
        let mock_response_hop2 = amm::SimulationResponse {
            return_amount: Uint128::new(5000), // 20000 USDT -> 5000 AUSD
            spread_amount: Uint128::zero(),
            commission_amount: Uint128::zero(),
        };

        querier.update_wasm(move |q: &WasmQuery| match q {
            WasmQuery::Smart {
                contract_addr, msg, ..
            } => {
                let decoded: amm::QueryMsg = from_json(msg).unwrap();
                if contract_addr == POOL_A_ADDR {
                    match decoded {
                        amm::QueryMsg::Simulation { offer_asset } => {
                            assert_eq!(offer_asset.amount, Uint128::new(1000));
                            SystemResult::Ok(ContractResult::Ok(
                                to_json_binary(&mock_response_hop1).unwrap(),
                            ))
                        }
                        amm::QueryMsg::Pair {} => SystemResult::Ok(ContractResult::Ok(
                            pair_binary(native("inj"), native("usdt")),
                        )),
                    }
                } else if contract_addr == POOL_B_ADDR {
                    match decoded {
                        amm::QueryMsg::Simulation { offer_asset } => {
                            assert_eq!(offer_asset.amount, Uint128::new(20000));
                            SystemResult::Ok(ContractResult::Ok(
                                to_json_binary(&mock_response_hop2).unwrap(),
                            ))
                        }
                        amm::QueryMsg::Pair {} => SystemResult::Ok(ContractResult::Ok(
                            pair_binary(native("usdt"), native("ausd")),
                        )),
                    }
                } else {
                    panic!("Unexpected query to {}", contract_addr);
                }
            }
            _ => panic!("Unsupported query type"),
        });

        let mut deps = inj_mock_deps();
        deps.querier = querier;

        let stages = vec![Stage {
            splits: vec![Split {
                percent: 100,
                path: vec![
                    Operation::AmmSwap(AmmSwapOp { ask_asset_info: None, max_spread: None,
                        pool_address: POOL_A_ADDR.to_string(),
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    }),
                    Operation::AmmSwap(AmmSwapOp { ask_asset_info: None, max_spread: None,
                        pool_address: POOL_B_ADDR.to_string(),
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                    }),
                ],
            }],
        }];

        let result_binary = simulate_route(
            deps.as_ref(),
            mock_env(),
            stages,
            Coin::new(1000u128, "inj"),
        )
        .unwrap();
        let result: SimulateRouteResponse = from_json(&result_binary).unwrap();
        assert_eq!(result.output_amount, Uint128::new(5000));
    }

    #[test]
    fn test_simulate_multi_split_multi_stage() {
        // A pool address is one fixed pair, so each logical pair needs its own
        // address (the op no longer declares its output asset — it's derived from
        // the pool's `Pair {}`).
        //
        // NOTE: this used to split stage 2 across a USDT pool and an AUSD pool with
        // percents of 100 and 100. Both the executor (`MixedAssetsInStage`) and now
        // the simulator reject that shape — a stage's splits share ONE pile of
        // assets and their percents must sum to 100 — so the route is now a fan-out
        // into a common denom followed by a single fan-in, which is what a router
        // should emit anyway.
        const POOL_INJ_USDT_A: &str = "inj1pool000000000000000000000000injusdta";
        const POOL_INJ_USDT_B: &str = "inj1pool000000000000000000000000injusdtb";
        const POOL_USDT_SHROOM: &str = "inj1pool000000000000000000000000usdtshrm";

        let mut querier: MockQuerier<InjectiveQueryWrapper> = MockQuerier::new(&[]);

        querier.update_wasm(move |q: &WasmQuery| match q {
            WasmQuery::Smart {
                contract_addr, msg, ..
            } => {
                let decoded: amm::QueryMsg = from_json(msg).unwrap();
                let pair = match contract_addr.as_str() {
                    POOL_INJ_USDT_A | POOL_INJ_USDT_B => (native("inj"), native("usdt")),
                    POOL_USDT_SHROOM => (native("usdt"), native("shroom")),
                    other => panic!("Unexpected query to {}", other),
                };
                let offer_asset = match decoded {
                    amm::QueryMsg::Pair {} => {
                        return SystemResult::Ok(ContractResult::Ok(pair_binary(pair.0, pair.1)));
                    }
                    amm::QueryMsg::Simulation { offer_asset } => offer_asset,
                };

                let response_amount = match (contract_addr.as_str(), offer_asset.amount.u128()) {
                    // Stage 1: 1000 INJ split 50/50 -> 10000 + 20000 USDT
                    (POOL_INJ_USDT_A, 500) => 10000,
                    (POOL_INJ_USDT_B, 500) => 20000,
                    // Stage 2: the pooled 30000 USDT -> 15000 SHROOM
                    (POOL_USDT_SHROOM, 30000) => 15000,
                    _ => panic!(
                        "Unexpected query: {} with amount {}",
                        contract_addr, offer_asset.amount
                    ),
                };
                let mock_response = amm::SimulationResponse {
                    return_amount: Uint128::new(response_amount),
                    ..Default::default()
                };
                SystemResult::Ok(ContractResult::Ok(to_json_binary(&mock_response).unwrap()))
            }
            _ => panic!("Unsupported query type"),
        });

        let mut deps = inj_mock_deps();
        deps.querier = querier;

        let inj_split = |pool: &str| Split {
            percent: 50,
            path: vec![Operation::AmmSwap(AmmSwapOp {
                ask_asset_info: None,
                max_spread: None,
                pool_address: pool.to_string(),
                offer_asset_info: AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
            })],
        };

        let stages = vec![
            Stage {
                splits: vec![inj_split(POOL_INJ_USDT_A), inj_split(POOL_INJ_USDT_B)],
            },
            Stage {
                splits: vec![Split {
                    percent: 100,
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        ask_asset_info: None,
                        max_spread: None,
                        pool_address: POOL_USDT_SHROOM.to_string(),
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                    })],
                }],
            },
        ];

        let result_binary = simulate_route(
            deps.as_ref(),
            mock_env(),
            stages,
            Coin::new(1000u128, "inj"),
        )
        .unwrap();
        let result: SimulateRouteResponse = from_json(&result_binary).unwrap();
        assert_eq!(result.output_amount, Uint128::new(15000));
    }

    /// The gate must refuse a stage whose splits do not sum to 100 — the executor
    /// hands the last split whatever is left over, so any other sum silently means
    /// something different there than it reads as here.
    #[test]
    fn test_simulate_rejects_bad_percentage_sum_on_later_stage() {
        let deps = inj_mock_deps();
        let split = |pool: &str, pct: u8| Split {
            percent: pct,
            path: vec![Operation::AmmSwap(AmmSwapOp {
                ask_asset_info: None,
                max_spread: None,
                pool_address: pool.to_string(),
                offer_asset_info: AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
            })],
        };
        let stages = vec![
            Stage {
                splits: vec![split(POOL_A_ADDR, 100)],
            },
            // 100 + 100 = 200
            Stage {
                splits: vec![split(POOL_A_ADDR, 100), split(POOL_B_ADDR, 100)],
            },
        ];
        let err = simulate_route(
            deps.as_ref(),
            mock_env(),
            stages,
            Coin::new(1000u128, "inj"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("Percentages in a stage must sum to 100"),
            "unexpected error: {err}"
        );
    }

    /// Two different natives arriving in one stage is `MixedAssetsInStage` in the
    /// executor; the gate must not quote it as though the splits were independently
    /// funded.
    #[test]
    fn test_simulate_rejects_two_different_natives_in_one_stage() {
        const POOL_INJ_USDT: &str = "inj1pool0000000000000000000000000injusdt0";
        const POOL_INJ_AUSD: &str = "inj1pool0000000000000000000000000injausd0";

        let mut querier: MockQuerier<InjectiveQueryWrapper> = MockQuerier::new(&[]);
        querier.update_wasm(move |q: &WasmQuery| match q {
            WasmQuery::Smart {
                contract_addr, msg, ..
            } => {
                let pair = match contract_addr.as_str() {
                    POOL_INJ_USDT => (native("inj"), native("usdt")),
                    POOL_INJ_AUSD => (native("inj"), native("ausd")),
                    other => panic!("Unexpected query to {}", other),
                };
                match from_json::<amm::QueryMsg>(msg).unwrap() {
                    amm::QueryMsg::Pair {} => {
                        SystemResult::Ok(ContractResult::Ok(pair_binary(pair.0, pair.1)))
                    }
                    amm::QueryMsg::Simulation { .. } => {
                        SystemResult::Ok(ContractResult::Ok(
                            to_json_binary(&amm::SimulationResponse {
                                return_amount: Uint128::new(500),
                                ..Default::default()
                            })
                            .unwrap(),
                        ))
                    }
                }
            }
            _ => panic!("Unsupported query type"),
        });
        let mut deps = inj_mock_deps();
        deps.querier = querier;

        let split = |pool: &str| Split {
            percent: 50,
            path: vec![Operation::AmmSwap(AmmSwapOp {
                ask_asset_info: None,
                max_spread: None,
                pool_address: pool.to_string(),
                offer_asset_info: AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
            })],
        };
        // Stage 1 fans INJ out into USDT and AUSD; stage 2 would then receive two
        // different natives in one pile.
        let stages = vec![
            Stage {
                splits: vec![split(POOL_INJ_USDT), split(POOL_INJ_AUSD)],
            },
            Stage {
                splits: vec![Split {
                    percent: 100,
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        ask_asset_info: None,
                        max_spread: None,
                        pool_address: POOL_INJ_USDT.to_string(),
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                    })],
                }],
            },
        ];
        let err = simulate_route(
            deps.as_ref(),
            mock_env(),
            stages,
            Coin::new(1000u128, "inj"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("two different native assets"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_query_fee_for_pool() {
        // --- Setup using the proven litmus test pattern ---
        let mut deps = inj_mock_deps();
        deps.api = MockApi::default().with_prefix("inj");

        // Use the API to generate valid addresses for the test
        let pool_a_addr = deps.api.addr_make("pool_a");
        let pool_c_addr = deps.api.addr_make("pool_c_no_fee");

        // Arrange: Set up the state needed for this test
        let fee_a = Decimal::from_str("0.003").unwrap();
        FEE_MAP
            .save(deps.as_mut().storage, &pool_a_addr, &fee_a)
            .unwrap();

        // --- Act & Assert: Test Case 1 (Fee exists) ---
        let msg = QueryMsg::FeeForPool {
            pool_address: pool_a_addr.to_string(),
        };
        let res_binary = query(deps.as_ref(), mock_env(), msg).unwrap();
        let res: FeeResponse = from_json(&res_binary).unwrap();
        assert_eq!(res.fee, Some(Decimal::from_str("0.003").unwrap()));

        // --- Act & Assert: Test Case 2 (Fee does not exist) ---
        let msg = QueryMsg::FeeForPool {
            pool_address: pool_c_addr.to_string(),
        };
        let res_binary = query(deps.as_ref(), mock_env(), msg).unwrap();
        let res: FeeResponse = from_json(&res_binary).unwrap();
        assert_eq!(res.fee, None);
    }

    #[test]
    fn test_query_all_fees_with_pagination() {
        // --- Setup using the proven litmus test pattern ---
        let mut deps = inj_mock_deps();
        deps.api = MockApi::default().with_prefix("inj");

        // Use the API to generate valid addresses for the test.
        // We will generate them in a way that we can predict the alphabetical order.
        let pool_addr_1 = deps.api.addr_make("pool_alpha"); // Starts with "inj1..."
        let pool_addr_2 = deps.api.addr_make("pool_zulu"); // Starts with a different "inj1..."

        // To make the test robust, we must determine the correct order programmatically.
        let (first_addr, second_addr) = if pool_addr_1 < pool_addr_2 {
            (pool_addr_1.clone(), pool_addr_2.clone())
        } else {
            (pool_addr_2.clone(), pool_addr_1.clone())
        };

        // Arrange: Set up the state
        let fee_1 = Decimal::from_str("0.003").unwrap();
        let fee_2 = Decimal::from_str("0.015").unwrap();
        FEE_MAP
            .save(deps.as_mut().storage, &pool_addr_1, &fee_1)
            .unwrap();
        FEE_MAP
            .save(deps.as_mut().storage, &pool_addr_2, &fee_2)
            .unwrap();

        // --- Act & Assert: Query all and check the determined order ---
        let msg = QueryMsg::AllFees {
            start_after: None,
            limit: None,
        };
        let res_binary = query(deps.as_ref(), mock_env(), msg).unwrap();
        let res: AllFeesResponse = from_json(&res_binary).unwrap();

        assert_eq!(res.fees.len(), 2);
        assert_eq!(res.fees[0].pool_address, first_addr.to_string());
        assert_eq!(res.fees[1].pool_address, second_addr.to_string());
    }

    #[test]
    fn test_query_all_fees_empty() {
        // --- Setup using the proven litmus test pattern ---
        let mut deps = inj_mock_deps();
        deps.api = MockApi::default().with_prefix("inj");

        let msg = QueryMsg::AllFees {
            start_after: None,
            limit: None,
        };
        let res_binary = query(deps.as_ref(), mock_env(), msg).unwrap();
        let res: AllFeesResponse = from_json(&res_binary).unwrap();
        assert_eq!(res.fees.len(), 0);
    }
}
