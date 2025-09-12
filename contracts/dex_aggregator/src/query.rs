use crate::msg::{external, orderbook, Operation, SimulateRouteResponse, Stage};
use crate::state::Config;
use cosmwasm_std::{
    to_json_binary, Binary, Coin, Deps, Env, QuerierWrapper, StdError, StdResult, Uint128,
    WasmQuery,
};

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

    let mut current_assets: Vec<external::Asset> = vec![external::Asset {
        info: external::AssetInfo::NativeToken {
            denom: amount_in.denom,
        },
        amount: amount_in.amount,
    }];

    for stage in stages {
        let mut next_stage_outputs: Vec<external::Asset> = vec![];

        // Group the current assets by their type to get the total for each pile.
        let mut grouped_inputs: Vec<(external::AssetInfo, Uint128)> = vec![];
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

        let mut amounts_allocated: Vec<(external::AssetInfo, Uint128)> = vec![];

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

            let mut current_path_asset = external::Asset {
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
    offer_asset: &external::Asset,
) -> StdResult<external::Asset> {
    match operation {
        Operation::AmmSwap(op) => {
            let pair_query = external::QueryMsg::Simulation {
                offer_asset: offer_asset.clone(),
            };
            let contract_addr = op.pool_address.to_string();

            let sim_response: external::SimulationResponse = querier.query(
                &WasmQuery::Smart {
                    contract_addr,
                    msg: to_json_binary(&pair_query)?,
                }
                .into(),
            )?;

            Ok(external::Asset {
                info: op.ask_asset_info.clone(),
                amount: sim_response.return_amount,
            })
        }
        Operation::OrderbookSwap(op) => {
            let source_denom = match &offer_asset.info {
                external::AssetInfo::NativeToken { denom } => denom.clone(),
                _ => {
                    return Err(StdError::generic_err(
                        "Orderbook simulation only supports native token inputs",
                    ))
                }
            };
            let target_denom = match &op.ask_asset_info {
                external::AssetInfo::NativeToken { denom } => denom.clone(),
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

            Ok(external::Asset {
                info: op.ask_asset_info.clone(),
                amount: sim_response.result_quantity.into(),
            })
        }
    }
}

fn get_path_start_info(path: &[Operation]) -> StdResult<external::AssetInfo> {
    let first_op = path
        .first()
        .ok_or_else(|| StdError::generic_err("Path cannot be empty"))?;
    Ok(match first_op {
        Operation::AmmSwap(op) => op.offer_asset_info.clone(),
        Operation::OrderbookSwap(op) => op.offer_asset_info.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::{AmmSwapOp, Split, Stage};
    use cosmwasm_std::testing::{mock_dependencies, mock_env, MockQuerier};
    use cosmwasm_std::{from_json, ContractResult, SystemResult};
    use external::AssetInfo;

    const FAKE_POOL_A: &str = "inj1pool_a";
    const FAKE_POOL_B: &str = "inj1pool_b";

    #[test]
    fn test_simulate_simple_path() {
        let mut querier = MockQuerier::new(&[]);
        let mock_response = external::SimulationResponse {
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
                        if contract_addr == FAKE_POOL_A {
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
                    pool_address: FAKE_POOL_A.to_string(),
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

        let mock_response_hop1 = external::SimulationResponse {
            return_amount: Uint128::new(20000), // 1000 INJ -> 20000 USDT
            spread_amount: Uint128::zero(),
            commission_amount: Uint128::zero(),
        };
        let mock_response_hop2 = external::SimulationResponse {
            return_amount: Uint128::new(5000), // 20000 USDT -> 5000 AUSD
            spread_amount: Uint128::zero(),
            commission_amount: Uint128::zero(),
        };

        querier.update_wasm(move |q: &WasmQuery| match q {
            WasmQuery::Smart {
                contract_addr, msg, ..
            } => {
                let decoded: external::QueryMsg = from_json(msg).unwrap();
                if contract_addr == FAKE_POOL_A {
                    let external::QueryMsg::Simulation { offer_asset } = decoded;
                    assert_eq!(offer_asset.amount, Uint128::new(1000));
                    SystemResult::Ok(ContractResult::Ok(
                        to_json_binary(&mock_response_hop1).unwrap(),
                    ))
                } else if contract_addr == FAKE_POOL_B {
                    let external::QueryMsg::Simulation { offer_asset } = decoded;
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
                        pool_address: FAKE_POOL_A.to_string(),
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                    }),
                    Operation::AmmSwap(AmmSwapOp {
                        pool_address: FAKE_POOL_B.to_string(),
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
                let decoded: external::QueryMsg = from_json(msg).unwrap();
                let external::QueryMsg::Simulation { offer_asset } = decoded;

                let response_amount = match (contract_addr.as_str(), offer_asset.amount.u128()) {
                    // Stage 1
                    (FAKE_POOL_A, 500) => 10000, // 50% of 1000 INJ -> 10000 USDT
                    (FAKE_POOL_B, 500) => 20000, // 50% of 1000 INJ -> 20000 AUSD
                    // Stage 2 (Totals: 10k USDT, 20k AUSD)
                    (FAKE_POOL_A, 10000) => 5000, // 10000 USDT -> 5000 SHROOM
                    (FAKE_POOL_B, 20000) => 8000, // 20000 AUSD -> 8000 SHROOM
                    _ => panic!(
                        "Unexpected query: {} with amount {}",
                        contract_addr, offer_asset.amount
                    ),
                };
                let mock_response = external::SimulationResponse {
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
                            pool_address: FAKE_POOL_A.to_string(),
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
                            pool_address: FAKE_POOL_B.to_string(),
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
                            pool_address: FAKE_POOL_A.to_string(),
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
                            pool_address: FAKE_POOL_B.to_string(),
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
}
