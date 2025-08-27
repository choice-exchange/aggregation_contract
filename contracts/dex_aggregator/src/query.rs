use crate::msg::{
    external, orderbook, ActionDescription, Route, SimulateRouteResponse, Step,
};
use crate::state::Config;
use cosmwasm_std::{
    to_json_binary, Binary, Coin, Deps, Env, QuerierWrapper, StdResult, Uint128, WasmQuery,
};
use std::collections::{HashMap, HashSet};

// The main entry point for the query
pub fn simulate_route(deps: Deps, _env: Env, route: Route, amount_in: Coin) -> StdResult<Binary> {
    if route.steps.is_empty() {
        return to_json_binary(&SimulateRouteResponse {
            output_amount: Uint128::zero(),
        });
    }

    let mut step_outputs: HashMap<usize, Uint128> = HashMap::new();

    let all_indices: HashSet<usize> = (0..route.steps.len()).collect();
    let destination_indices: HashSet<usize> = route
        .steps
        .iter()
        .flat_map(|step| step.next_steps.iter().cloned())
        .collect();
    let root_node_indices: Vec<usize> = all_indices
        .difference(&destination_indices)
        .cloned()
        .collect();

    if root_node_indices.is_empty() {
        return Err(cosmwasm_std::StdError::generic_err(
            "Route has a cycle or is invalid; no root nodes found",
        ));
    }
    
    let mut to_process: Vec<usize> = root_node_indices;

    'main_loop: while let Some(step_index) = to_process.pop() {
        if step_outputs.contains_key(&step_index) {
            continue;
        }

        let step = &route.steps[step_index];

        let current_input_amount = if destination_indices.contains(&step_index) {
            let mut total_input = Uint128::zero();
            for (parent_index, parent_step) in route.steps.iter().enumerate() {
                if parent_step.next_steps.contains(&step_index) {
                    if let Some(parent_output) = step_outputs.get(&parent_index) {
                        total_input += parent_output.multiply_ratio(step.amount_in_percentage as u128, 100u128);
                    } else {
                        to_process.push(step_index);
                        to_process.push(parent_index);
                        continue 'main_loop; 
                    }
                }
            }
            total_input
        } else {
            amount_in.amount.multiply_ratio(step.amount_in_percentage as u128, 100u128)
        };

        let current_step_output_amount =
            simulate_single_step(&deps.querier, step, current_input_amount)?;

        step_outputs.insert(step_index, current_step_output_amount);

        for &next_step_index in &step.next_steps {
            if !step_outputs.contains_key(&next_step_index) {
                to_process.push(next_step_index);
            }
        }
    }

    let mut total_output = Uint128::zero();
    for (i, step) in route.steps.iter().enumerate() {
        if step.next_steps.is_empty() {
            if let Some(output) = step_outputs.get(&i) {
                total_output += output;
            } else {
                return Err(cosmwasm_std::StdError::generic_err(format!(
                    "Could not calculate output for leaf node {}",
                    i
                )));
            }
        }
    }

    let response = SimulateRouteResponse {
        output_amount: total_output,
    };
    to_json_binary(&response)
}


fn simulate_single_step(
    querier: &QuerierWrapper,
    step: &Step,
    input_amount: Uint128,
) -> StdResult<Uint128> {
    match &step.description {
        ActionDescription::AmmSwap {
            protocol: _, 
            offer_asset_info,
            .. // ask_asset_info is not needed for the simulation query
        } => {
            let offer_asset = external::Asset {
                info: offer_asset_info.clone(),
                amount: input_amount,
            };

            let pair_query = external::QueryMsg::Simulation { offer_asset };

            let sim_response: external::SimulationResponse =
                querier.query(&WasmQuery::Smart {
                    contract_addr: step.protocol_address.to_string(),
                    msg: to_json_binary(&pair_query)?,
                }.into())?;

            Ok(sim_response.return_amount)
        }
        ActionDescription::OrderbookSwap {
            source_denom,
            target_denom,
        } => {
            // This part of the logic was already direct and remains unchanged.
            let orderbook_query = orderbook::QueryMsg::GetOutputQuantity {
                from_quantity: input_amount.into(),
                source_denom: source_denom.clone(),
                target_denom: target_denom.clone(),
            };

            let sim_response: orderbook::SwapEstimationResult =
                querier.query(&WasmQuery::Smart {
                    contract_addr: step.protocol_address.to_string(),
                    msg: to_json_binary(&orderbook_query)?,
                }.into())?;

            Ok(sim_response.result_quantity.into())
        }
    }
}

pub fn query_config(deps: Deps) -> StdResult<Binary> {
    let config: Config = crate::state::CONFIG.load(deps.storage)?;
    to_json_binary(&config)
}

// --- UNIT TEST FOR simulate_route ---

#[cfg(test)]
mod tests {
    use super::*;
    // Import the new pair module for testing
    use crate::msg::external;
    use crate::msg::{ActionDescription, AmmProtocol, Step};
    use cosmwasm_std::testing::{mock_dependencies, mock_env, MockQuerier};
    use cosmwasm_std::{from_json, Addr, ContractResult, SystemResult};
    use external::AssetInfo;

    // Renaming for clarity: these are now PAIR contracts, not routers.
    const FAKE_PAIR_CONTRACT_A: &str = "inj1pair_a";
    const FAKE_PAIR_CONTRACT_B: &str = "inj1pair_b";
    const FAKE_ORDERBOOK: &str = "inj1orderbook";
    const FINAL_TOKEN: &str = "inj1final_token_contract";

    #[test]
    fn test_simulate_simple_amm_route_direct_to_pair() {
        let mut querier = MockQuerier::new(&[]);

        // Mock the response from a PAIR contract's `Simulation` query.
        let mock_response = external::SimulationResponse {
            return_amount: Uint128::new(50000),
            spread_amount: Uint128::new(100), // Dummy data
            commission_amount: Uint128::new(50), // Dummy data
        };
        let mock_response_binary = to_json_binary(&mock_response).unwrap();

        querier.update_wasm(
            move |query: &WasmQuery| -> SystemResult<ContractResult<Binary>> {
                match query {
                    WasmQuery::Smart {
                        contract_addr,
                        msg: _, // No need to inspect msg for this simple test
                    } => {
                        if contract_addr == FAKE_PAIR_CONTRACT_A {
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

        let route = Route {
            steps: vec![Step {
                // Address is now the PAIR address.
                protocol_address: Addr::unchecked(FAKE_PAIR_CONTRACT_A),
                description: ActionDescription::AmmSwap {
                    protocol: AmmProtocol::Choice, // Protocol is just for context now
                    offer_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    ask_asset_info: AssetInfo::Token {
                        contract_addr: "some_token".to_string(),
                    },
                },
                amount_in_percentage: 100,
                next_steps: vec![],
            }],
        };

        let result_binary =
            simulate_route(deps.as_ref(), mock_env(), route, Coin::new(1000u128, "inj")).unwrap();
        let result: SimulateRouteResponse = from_json(&result_binary).unwrap();
        assert_eq!(result.output_amount, Uint128::new(50000));
    }

    #[test]
    fn test_simulate_split_route_direct_to_pair() {

        let mut querier = MockQuerier::new(&[]);

        // Mock responses for PAIR A and PAIR B
        let mock_response_a = external::SimulationResponse {
            return_amount: Uint128::new(30000),
            spread_amount: Uint128::zero(), commission_amount: Uint128::zero(),
        };
        let mock_response_a_binary = to_json_binary(&mock_response_a).unwrap();

        let mock_response_b = external::SimulationResponse {
            return_amount: Uint128::new(45000),
            spread_amount: Uint128::zero(), commission_amount: Uint128::zero(),
        };
        let mock_response_b_binary = to_json_binary(&mock_response_b).unwrap();

        querier.update_wasm(
            move |query: &WasmQuery| -> SystemResult<ContractResult<Binary>> {
                match query {
                    WasmQuery::Smart { contract_addr, msg } => {
                        // Decode the new pair::QueryMsg to inspect the offer amount
                        let decoded_query: external::QueryMsg = from_json(msg).unwrap();
                        let external::QueryMsg::Simulation { offer_asset } = decoded_query;

                        if contract_addr == FAKE_PAIR_CONTRACT_A {
                            assert_eq!(offer_asset.amount, Uint128::new(500));
                            SystemResult::Ok(ContractResult::Ok(mock_response_a_binary.clone()))
                        } else if contract_addr == FAKE_PAIR_CONTRACT_B {
                            assert_eq!(offer_asset.amount, Uint128::new(500));
                            SystemResult::Ok(ContractResult::Ok(mock_response_b_binary.clone()))
                        } else {
                            panic!("Unexpected contract query to: {}", contract_addr);
                        }
                    }
                    _ => panic!("Unsupported WasmQuery type"),
                }
            },
        );

        let mut deps = mock_dependencies();
        deps.querier = querier;

        let route = Route {
            steps: vec![
                Step {
                    protocol_address: Addr::unchecked(FAKE_PAIR_CONTRACT_A),
                    description: ActionDescription::AmmSwap {
                        protocol: AmmProtocol::Choice,
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: AssetInfo::Token {
                            contract_addr: Addr::unchecked(FINAL_TOKEN).to_string(),
                        },
                    },
                    amount_in_percentage: 50,
                    next_steps: vec![],
                },
                Step {
                    protocol_address: Addr::unchecked(FAKE_PAIR_CONTRACT_B),
                    description: ActionDescription::AmmSwap {
                        protocol: AmmProtocol::DojoSwap,
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: AssetInfo::Token {
                            contract_addr: Addr::unchecked(FINAL_TOKEN).to_string(),
                        },
                    },
                    amount_in_percentage: 50,
                    next_steps: vec![],
                },
            ],
        };
        
        let result_binary =
            simulate_route(deps.as_ref(), mock_env(), route, Coin::new(1000u128, "inj")).unwrap();
        let result: SimulateRouteResponse = from_json(&result_binary).unwrap();
        assert_eq!(result.output_amount, Uint128::new(30000 + 45000));
    }

    #[test]
    fn test_simulate_multi_step_split_route_with_different_protocols() {
        use injective_math::FPDecimal;

        let mut querier = MockQuerier::new(&[]);

        // 1. Orderbook Response (Step 0): 100 USDT -> 200,000 INJ
        let orderbook_response = orderbook::SwapEstimationResult {
            expected_fees: vec![],
            result_quantity: FPDecimal::from(200_000u128),
        };
        let orderbook_response_bin = to_json_binary(&orderbook_response).unwrap();

        // 2. Pair A Response (Step 1): Takes its share of INJ, outputs 50,000 final tokens
        let amm_a_response = external::SimulationResponse {
            return_amount: Uint128::new(50_000),
            spread_amount: Uint128::zero(), // Dummy data
            commission_amount: Uint128::zero(), // Dummy data
        };
        let amm_a_response_bin = to_json_binary(&amm_a_response).unwrap();

        // 3. Pair B Response (Step 2): Takes its share of INJ, outputs 70,000 final tokens
        let amm_b_response = external::SimulationResponse {
            return_amount: Uint128::new(70_000),
            spread_amount: Uint128::zero(),
            commission_amount: Uint128::zero(),
        };
        let amm_b_response_bin = to_json_binary(&amm_b_response).unwrap();

        // Teach the querier how to handle calls to all three distinct contract addresses
        querier.update_wasm(
            move |query: &WasmQuery| -> SystemResult<ContractResult<Binary>> {
                match query {
                    WasmQuery::Smart { contract_addr, msg } => {
                        if contract_addr == FAKE_ORDERBOOK {
                            // This is Step 0, just return the canned response
                            SystemResult::Ok(ContractResult::Ok(orderbook_response_bin.clone()))
                        } else if contract_addr == FAKE_PAIR_CONTRACT_A {
                            // This is Step 1. It should receive a direct `Simulation` query.
                            // Assert it received the correct 43% of the order book's output.
                            // 200,000 * 0.43 = 86,000
                            let decoded_query: external::QueryMsg = from_json(msg).unwrap();
                            let external::QueryMsg::Simulation { offer_asset } = decoded_query;
                            assert_eq!(offer_asset.amount, Uint128::new(86_000));
                            SystemResult::Ok(ContractResult::Ok(amm_a_response_bin.clone()))
                        } else if contract_addr == FAKE_PAIR_CONTRACT_B {
                            // This is Step 2. It should receive a direct `Simulation` query.
                            // Assert it received the correct 57% of the order book's output.
                            // 200,000 * 0.57 = 114,000
                            let decoded_query: external::QueryMsg = from_json(msg).unwrap();
                             let external::QueryMsg::Simulation { offer_asset } = decoded_query;
                            assert_eq!(offer_asset.amount, Uint128::new(114_000));
                            SystemResult::Ok(ContractResult::Ok(amm_b_response_bin.clone()))
                        } else {
                            panic!("Unexpected contract query to {}", contract_addr)
                        }
                    }
                    _ => panic!("Unsupported query type"),
                }
            },
        );

        let mut deps = mock_dependencies();
        deps.querier = querier;

        // --- Construct the Multi-Step Route with direct PAIR addresses ---
        let route = Route {
            steps: vec![
                // Step 0: USDT -> INJ via Order Book, then splits to steps 1 and 2
                Step {
                    protocol_address: Addr::unchecked(FAKE_ORDERBOOK),
                    description: ActionDescription::OrderbookSwap {
                        source_denom: "peggy0xdAC...".to_string(), // Some USDT denom
                        target_denom: "inj".to_string(),
                    },
                    amount_in_percentage: 100,
                    next_steps: vec![1, 2], // Points to the next two steps
                },
                // Step 1: First AMM path (43% of INJ from step 0), uses PAIR_A
                Step {
                    protocol_address: Addr::unchecked(FAKE_PAIR_CONTRACT_A),
                    description: ActionDescription::AmmSwap {
                        protocol: AmmProtocol::Choice, // Protocol for context
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: AssetInfo::Token { // Not used in query, but good for route description
                            contract_addr: Addr::unchecked(FINAL_TOKEN).to_string(),
                        },
                    },
                    amount_in_percentage: 43,
                    next_steps: vec![], // This is a leaf node
                },
                // Step 2: Second AMM path (57% of INJ from step 0), uses PAIR_B
                Step {
                    protocol_address: Addr::unchecked(FAKE_PAIR_CONTRACT_B),
                    description: ActionDescription::AmmSwap {
                        protocol: AmmProtocol::DojoSwap, // Protocol for context
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: AssetInfo::Token {
                            contract_addr: Addr::unchecked(FINAL_TOKEN).to_string(),
                        },
                    },
                    amount_in_percentage: 57,
                    next_steps: vec![], // This is a leaf node
                },
            ],
        };

        let result_binary = simulate_route(
            deps.as_ref(),
            mock_env(),
            route,
            Coin::new(100u128, "peggy0xdAC..."),
        )
        .unwrap();

        let result: SimulateRouteResponse = from_json(&result_binary).unwrap();

        assert_eq!(result.output_amount, Uint128::new(50_000 + 70_000));
    }
}