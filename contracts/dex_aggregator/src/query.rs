use crate::msg::{
    amm, orderbook, AllFeesResponse, FeeInfo, FeeResponse, Operation, SimulateRouteResponse, Stage,
};
use crate::state::{Config, FEE_MAP};
use cosmwasm_std::{
    to_json_binary, Binary, Coin, Deps, Env, Order, QuerierWrapper, StdError, StdResult, Uint128,
    WasmQuery,
};
use cw_storage_plus::Bound;

pub fn query_config(deps: Deps) -> StdResult<Binary> {
    let config: Config = crate::state::CONFIG.load(deps.storage)?;
    to_json_binary(&config)
}

pub fn simulate_route(
    deps: Deps,
    _env: Env,
    stages: Vec<Stage>,
    amount_in: Coin,
) -> StdResult<Binary> {
    if stages.is_empty() {
        return to_json_binary(&SimulateRouteResponse {
            output_amount: Uint128::zero(),
        });
    }

    let mut current_assets: Vec<amm::Asset> = vec![amm::Asset {
        info: amm::AssetInfo::NativeToken {
            denom: amount_in.denom,
        },
        amount: amount_in.amount,
    }];

    for stage in stages {
        let mut next_stage_outputs: Vec<amm::Asset> = vec![];

        // Group the current assets by their type to get the total for each pile.
        let mut grouped_inputs: Vec<(amm::AssetInfo, Uint128)> = vec![];
        for asset in current_assets {
            if let Some((_, amount)) = grouped_inputs
                .iter_mut()
                .find(|(info, _)| *info == asset.info)
            {
                *amount += asset.amount;
            } else {
                grouped_inputs.push((asset.info, asset.amount));
            }
        }

        let mut amounts_allocated: Vec<(amm::AssetInfo, Uint128)> = vec![];

        for (i, split) in stage.splits.iter().enumerate() {
            let path_input_info = get_path_start_info(&split.path)?;

            let total_amount_for_type = grouped_inputs
                .iter()
                .find(|(info, _)| *info == path_input_info)
                .map(|(_, amount)| *amount)
                .unwrap_or_else(Uint128::zero);

            let amount_for_split = if i < stage.splits.len() - 1 {
                total_amount_for_type.multiply_ratio(split.percent as u128, 100u128)
            } else {
                let already_allocated = amounts_allocated
                    .iter()
                    .find(|(info, _)| *info == path_input_info)
                    .map(|(_, amount)| *amount)
                    .unwrap_or_else(Uint128::zero);
                total_amount_for_type
                    .checked_sub(already_allocated)
                    .map_err(StdError::from)?
            };

            if let Some((_, allocated)) = amounts_allocated
                .iter_mut()
                .find(|(info, _)| *info == path_input_info)
            {
                *allocated += amount_for_split;
            } else {
                amounts_allocated.push((path_input_info.clone(), amount_for_split));
            }

            let mut current_path_asset = amm::Asset {
                info: path_input_info,
                amount: amount_for_split,
            };

            for operation in &split.path {
                let output_asset =
                    simulate_single_operation(&deps.querier, operation, &current_path_asset)?;
                current_path_asset = output_asset;
            }

            next_stage_outputs.push(current_path_asset);
        }

        current_assets = next_stage_outputs;
    }

    let total_output: Uint128 = current_assets.iter().map(|a| a.amount).sum();

    let response = SimulateRouteResponse {
        output_amount: total_output,
    };
    to_json_binary(&response)
}

/// Simulates a single swap operation.
fn simulate_single_operation(
    querier: &QuerierWrapper,
    operation: &Operation,
    offer_asset: &amm::Asset,
) -> StdResult<amm::Asset> {
    match operation {
        Operation::AmmSwap(op) => {
            let pair_query = amm::QueryMsg::Simulation {
                offer_asset: offer_asset.clone(),
            };
            let contract_addr = op.pool_address.to_string();

            let sim_response: amm::SimulationResponse = querier.query(
                &WasmQuery::Smart {
                    contract_addr,
                    msg: to_json_binary(&pair_query)?,
                }
                .into(),
            )?;

            Ok(amm::Asset {
                info: op.ask_asset_info.clone(),
                amount: sim_response.return_amount,
            })
        }
        Operation::OrderbookSwap(op) => {
            let source_denom = match &offer_asset.info {
                amm::AssetInfo::NativeToken { denom } => denom.clone(),
                _ => {
                    return Err(StdError::generic_err(
                        "Orderbook simulation only supports native token inputs",
                    ))
                }
            };
            let target_denom = match &op.ask_asset_info {
                amm::AssetInfo::NativeToken { denom } => denom.clone(),
                _ => {
                    return Err(StdError::generic_err(
                        "Orderbook simulation only supports native token outputs",
                    ))
                }
            };

            let orderbook_query = orderbook::QueryMsg::GetOutputQuantity {
                from_quantity: offer_asset.amount.into(),
                source_denom,
                target_denom,
            };
            let contract_addr = op.swap_contract.to_string();

            let sim_response: orderbook::SwapEstimationResult = querier.query(
                &WasmQuery::Smart {
                    contract_addr,
                    msg: to_json_binary(&orderbook_query)?,
                }
                .into(),
            )?;

            Ok(amm::Asset {
                info: op.ask_asset_info.clone(),
                amount: sim_response.result_quantity.into(),
            })
        }
    }
}

fn get_path_start_info(path: &[Operation]) -> StdResult<amm::AssetInfo> {
    let first_op = path
        .first()
        .ok_or_else(|| StdError::generic_err("Path cannot be empty"))?;
    Ok(match first_op {
        Operation::AmmSwap(op) => op.offer_asset_info.clone(),
        Operation::OrderbookSwap(op) => op.offer_asset_info.clone(),
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
            let (pool_addr, fee_percent) = item?;
            Ok(FeeInfo {
                pool_address: pool_addr.to_string(),
                fee_percent,
            })
        })
        .collect::<StdResult<_>>()?;

    to_json_binary(&AllFeesResponse { fees })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::query;
    use crate::msg::{AmmSwapOp, QueryMsg, Split, Stage};
    use amm::AssetInfo;
    use cosmwasm_std::testing::{mock_dependencies, mock_env, MockApi, MockQuerier};
    use cosmwasm_std::{from_json, ContractResult, Decimal, SystemResult};
    use std::str::FromStr;

    const POOL_A_ADDR: &str = "inj1hkhdaj2ts42k2x53h3w0f26g2xvy3a52e0u4gp";
    const POOL_B_ADDR: &str = "inj12sqy2n5qt52n5q2n5qt52n5q2n5qt52n5q2n5qt";

    #[test]
    fn test_simulate_simple_path() {
        let mut querier = MockQuerier::new(&[]);
        let mock_response = amm::SimulationResponse {
            return_amount: Uint128::new(50000),
            spread_amount: Uint128::zero(),
            commission_amount: Uint128::zero(),
        };
        let mock_response_binary = to_json_binary(&mock_response).unwrap();

        querier.update_wasm(
            move |query: &WasmQuery| -> SystemResult<ContractResult<Binary>> {
                match query {
                    WasmQuery::Smart {
                        contract_addr,
                        msg: _,
                    } => {
                        if contract_addr == POOL_A_ADDR {
                            SystemResult::Ok(ContractResult::Ok(mock_response_binary.clone()))
                        } else {
                            panic!("Unexpected contract call to {}", contract_addr);
                        }
                    }
                    _ => panic!("Unsupported query type"),
                }
            },
        );
        let mut deps = mock_dependencies();
        deps.querier = querier;

        let stages = vec![Stage {
            splits: vec![Split {
                percent: 100,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: POOL_A_ADDR.to_string(),
                    offer_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    ask_asset_info: AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
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
        let mut querier = MockQuerier::new(&[]);

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
                    let amm::QueryMsg::Simulation { offer_asset } = decoded;
                    assert_eq!(offer_asset.amount, Uint128::new(1000));
                    SystemResult::Ok(ContractResult::Ok(
                        to_json_binary(&mock_response_hop1).unwrap(),
                    ))
                } else if contract_addr == POOL_B_ADDR {
                    let amm::QueryMsg::Simulation { offer_asset } = decoded;
                    assert_eq!(offer_asset.amount, Uint128::new(20000));
                    SystemResult::Ok(ContractResult::Ok(
                        to_json_binary(&mock_response_hop2).unwrap(),
                    ))
                } else {
                    panic!("Unexpected query to {}", contract_addr);
                }
            }
            _ => panic!("Unsupported query type"),
        });

        let mut deps = mock_dependencies();
        deps.querier = querier;

        let stages = vec![Stage {
            splits: vec![Split {
                percent: 100,
                path: vec![
                    Operation::AmmSwap(AmmSwapOp {
                        pool_address: POOL_A_ADDR.to_string(),
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                    }),
                    Operation::AmmSwap(AmmSwapOp {
                        pool_address: POOL_B_ADDR.to_string(),
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        ask_asset_info: AssetInfo::NativeToken {
                            denom: "ausd".to_string(),
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
        let mut querier = MockQuerier::new(&[]);

        // Mock responses for all 4 swaps
        querier.update_wasm(move |q: &WasmQuery| match q {
            WasmQuery::Smart {
                contract_addr, msg, ..
            } => {
                let decoded: amm::QueryMsg = from_json(msg).unwrap();
                let amm::QueryMsg::Simulation { offer_asset } = decoded;

                let response_amount = match (contract_addr.as_str(), offer_asset.amount.u128()) {
                    // Stage 1
                    (POOL_A_ADDR, 500) => 10000, // 50% of 1000 INJ -> 10000 USDT
                    (POOL_B_ADDR, 500) => 20000, // 50% of 1000 INJ -> 20000 AUSD
                    // Stage 2 (Totals: 10k USDT, 20k AUSD)
                    (POOL_A_ADDR, 10000) => 5000, // 10000 USDT -> 5000 SHROOM
                    (POOL_B_ADDR, 20000) => 8000, // 20000 AUSD -> 8000 SHROOM
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

        let mut deps = mock_dependencies();
        deps.querier = querier;

        let stages = vec![
            // Stage 1: INJ -> USDT / AUSD
            Stage {
                splits: vec![
                    Split {
                        percent: 50,
                        path: vec![Operation::AmmSwap(AmmSwapOp {
                            pool_address: POOL_A_ADDR.to_string(),
                            offer_asset_info: AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                            ask_asset_info: AssetInfo::NativeToken {
                                denom: "usdt".to_string(),
                            },
                        })],
                    },
                    Split {
                        percent: 50,
                        path: vec![Operation::AmmSwap(AmmSwapOp {
                            pool_address: POOL_B_ADDR.to_string(),
                            offer_asset_info: AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                            ask_asset_info: AssetInfo::NativeToken {
                                denom: "ausd".to_string(),
                            },
                        })],
                    },
                ],
            },
            // Stage 2: USDT / AUSD -> SHROOM
            Stage {
                splits: vec![
                    Split {
                        percent: 100,
                        path: vec![Operation::AmmSwap(AmmSwapOp {
                            pool_address: POOL_A_ADDR.to_string(),
                            offer_asset_info: AssetInfo::NativeToken {
                                denom: "usdt".to_string(),
                            },
                            ask_asset_info: AssetInfo::NativeToken {
                                denom: "shroom".to_string(),
                            },
                        })],
                    },
                    Split {
                        percent: 100,
                        path: vec![Operation::AmmSwap(AmmSwapOp {
                            pool_address: POOL_B_ADDR.to_string(),
                            offer_asset_info: AssetInfo::NativeToken {
                                denom: "ausd".to_string(),
                            },
                            ask_asset_info: AssetInfo::NativeToken {
                                denom: "shroom".to_string(),
                            },
                        })],
                    },
                ],
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
        // Final output is the sum of the shroom from both paths
        assert_eq!(result.output_amount, Uint128::new(5000 + 8000));
    }

    #[test]
    fn test_query_fee_for_pool() {
        // --- Setup using the proven litmus test pattern ---
        let mut deps = mock_dependencies();
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
        let mut deps = mock_dependencies();
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
        let mut deps = mock_dependencies();
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
