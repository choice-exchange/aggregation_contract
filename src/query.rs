use cosmwasm_std::{to_json_binary, Binary, Coin, Deps, Env, StdResult, Uint128, WasmQuery, QuerierWrapper};
use crate::msg::{external, orderbook, Route, SimulateRouteResponse, ActionDescription, AmmProtocol, Step};
use crate::state::Config;
use std::collections::{HashMap, HashSet}; 

// The main entry point for the query
pub fn simulate_route(
    deps: Deps,
    _env: Env,
    route: Route,
    amount_in: Coin,
) -> StdResult<Binary> {
    let mut total_output = Uint128::zero();

    // 1. Create a set of all possible indices.
    let all_indices: HashSet<usize> = (0..route.steps.len()).collect();

    // 2. Create a set of all destination indices.
    let destination_indices: HashSet<usize> = route
        .steps
        .iter()
        .flat_map(|step| step.next_steps.iter().cloned())
        .collect();

    // 3. The root nodes are the difference between the two sets.
    let root_node_indices: Vec<&usize> = all_indices.difference(&destination_indices).collect();

    if root_node_indices.is_empty() && !route.steps.is_empty() {
        return Err(cosmwasm_std::StdError::generic_err("Route has a cycle or is invalid; no root nodes found"));
    }

    // A memoization cache to avoid re-calculating the same step multiple times in complex (rejoin) routes.
    let mut memo: HashMap<usize, Uint128> = HashMap::new();

    // Iterate over the dynamically found root nodes and start the simulation for each.
    for &root_index in root_node_indices {
        let root_step = &route.steps[root_index];

        // The input for this specific starting path is determined by its percentage of the total input.
        let input_for_path = amount_in.amount.multiply_ratio(
            root_step.amount_in_percentage as u128,
            100u128
        );

        total_output += simulate_step_recursive(
            &deps.querier,
            root_index,
            &route.steps,
            input_for_path,
            &mut memo,
        )?;
    }

    let response = SimulateRouteResponse { output_amount: total_output };
    to_json_binary(&response)
}


/// A recursive helper function to traverse the route graph and simulate swaps.
fn simulate_step_recursive(
    querier: &QuerierWrapper, 
    step_index: usize,
    steps: &Vec<Step>,
    input_amount: Uint128,
    memo: &mut HashMap<usize, Uint128>,
) -> StdResult<Uint128> {
    // Memoization: If we've already calculated the output for this step, return it.
    if let Some(cached_amount) = memo.get(&step_index) {
        return Ok(*cached_amount);
    }

    let step = steps.get(step_index).ok_or_else(|| cosmwasm_std::StdError::generic_err("Invalid step index in route"))?;

    // --- 1. Simulate the current step's swap to find its output ---
    let current_step_output_amount = match &step.description {
        ActionDescription::AmmSwap { protocol, offer_asset_info, ask_asset_info } => {
            let operations = match protocol {
                AmmProtocol::Choice => vec![external::SwapOperation::Choice {
                    offer_asset_info: offer_asset_info.clone(),
                    ask_asset_info: ask_asset_info.clone(),
                }],
            };

            let amm_query = external::QueryMsg::SimulateSwapOperations {
                offer_amount: input_amount,
                operations,
            };

            let sim_response: external::SimulateSwapOperationsResponse = querier.query(&WasmQuery::Smart {
                contract_addr: step.protocol_address.to_string(),
                msg: to_json_binary(&amm_query)?,
            }.into())?;

            sim_response.amount
        }
        ActionDescription::OrderbookSwap { source_denom, target_denom } => {
            let orderbook_query = orderbook::QueryMsg::GetOutputQuantity {
                from_quantity: input_amount.into(), // Convert Uint128 to FPDecimal for the query
                source_denom: source_denom.clone(),
                target_denom: target_denom.clone(),
            };

            let sim_response: orderbook::GetOutputQuantityResponse = querier.query(&WasmQuery::Smart {
                contract_addr: step.protocol_address.to_string(),
                msg: to_json_binary(&orderbook_query)?,
            }.into())?;
            
            sim_response.return_amount
        }
    };

    // --- 2. Handle the next steps (recursion) ---
    if step.next_steps.is_empty() {
        // Base Case: This is a "leaf" node in the route, so its output is a final output.
        memo.insert(step_index, current_step_output_amount);
        Ok(current_step_output_amount)
    } else {
        // Recursive Step: This step is an intermediate step. Sum the results of its children.
        let mut total_output_from_children = Uint128::zero();
        for &next_step_index in &step.next_steps {
            let next_step = steps.get(next_step_index).ok_or_else(|| cosmwasm_std::StdError::generic_err("Invalid next_steps index"))?;
            
            // Calculate the amount to pass to the next step based on its percentage
            let amount_for_next_step = current_step_output_amount.multiply_ratio(
                next_step.amount_in_percentage as u128,
                100u128
            );

            // Recurse
            let output_from_child = simulate_step_recursive(
                querier,
                next_step_index,
                steps,
                amount_for_next_step,
                memo,
            )?;
            
            total_output_from_children += output_from_child;
        }

        memo.insert(step_index, total_output_from_children);
        Ok(total_output_from_children)
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
    use crate::msg::{ActionDescription, AmmProtocol, Step};
    use cosmwasm_std::testing::{mock_dependencies, mock_env, MockQuerier};
    use cosmwasm_std::{from_json, ContractResult, SystemResult, Addr};
    use external::{AssetInfo, SimulateSwapOperationsResponse};

    const FAKE_AMM_ROUTER: &str = "inj1ammrouteraddress";
    const FAKE_AMM_ROUTER_A: &str = "inj1ammrouter_a";
    const FAKE_AMM_ROUTER_B: &str = "inj1ammrouter_b";
    const FAKE_ORDERBOOK: &str = "inj1orderbook";

    #[test]
    fn test_simulate_simple_amm_route() {
        // --- 1. Setup the Mock Querier ---
        let mut querier = MockQuerier::new(&[]);
        
        // This is the fake response we want the AMM router to return
        let mock_response = SimulateSwapOperationsResponse { amount: Uint128::new(50000) };
        let mock_response_binary = to_json_binary(&mock_response).unwrap();

        // Teach the querier how to respond to smart queries sent to our fake AMM address
        querier.update_wasm(move |query: &WasmQuery| -> SystemResult<ContractResult<Binary>> {
            match query {
                WasmQuery::Smart { contract_addr, msg: _ } => {
                    if contract_addr == FAKE_AMM_ROUTER {
                        SystemResult::Ok(ContractResult::Ok(mock_response_binary.clone()))
                    } else {
                        panic!("Unexpected contract call to {}", contract_addr);
                    }
                }
                _ => panic!("Unsupported query type"),
            }
        });

        // --- 2. Setup Dependencies with our custom querier ---
        let mut deps = mock_dependencies();
        deps.querier = querier;

        // --- 3. Construct the Test Route ---
        let route = Route {
            steps: vec![Step {
                protocol_address: Addr::unchecked(FAKE_AMM_ROUTER),
                description: ActionDescription::AmmSwap {
                    protocol: AmmProtocol::Choice,
                    offer_asset_info: AssetInfo::NativeToken { denom: "inj".to_string() },
                    ask_asset_info: AssetInfo::Token { contract_addr: "some_token".to_string() },
                },
                amount_in_percentage: 100,
                next_steps: vec![],
            }],
        };

        // --- 4. Call the Function Under Test ---
        let result_binary = simulate_route(
            deps.as_ref(),
            mock_env(),
            route,
            Coin::new(1000u128, "inj"),
        ).unwrap();

        // --- 5. Assert the Result ---
        let result: SimulateRouteResponse = from_json(&result_binary).unwrap();

        // The output amount should be exactly what our mock told it to be!
        assert_eq!(result.output_amount, Uint128::new(50000));
    }

    #[test]
    fn test_simulate_split_route() {
        // --- 1. Setup a more advanced Mock Querier ---
        let mut querier = MockQuerier::new(&[]);

        // Define separate responses for each AMM router
        let mock_response_a = external::SimulateSwapOperationsResponse { amount: Uint128::new(30000) };
        let mock_response_a_binary = to_json_binary(&mock_response_a).unwrap();

        let mock_response_b = external::SimulateSwapOperationsResponse { amount: Uint128::new(45000) };
        let mock_response_b_binary = to_json_binary(&mock_response_b).unwrap();

        // Teach the querier how to handle calls to TWO different addresses
        querier.update_wasm(move |query: &WasmQuery| -> SystemResult<ContractResult<Binary>> {
            match query {
                WasmQuery::Smart { contract_addr, msg } => {
                    if contract_addr == FAKE_AMM_ROUTER_A {
                        // Check that the input amount was correctly split (50% of 1000 is 500)
                        let decoded_query: external::QueryMsg = from_json(msg).unwrap();
                        let external::QueryMsg::SimulateSwapOperations { offer_amount, .. } = decoded_query;
                        assert_eq!(offer_amount, Uint128::new(500));
                        
                        SystemResult::Ok(ContractResult::Ok(mock_response_a_binary.clone()))
                    } else if contract_addr == FAKE_AMM_ROUTER_B {
                        // Check that the input amount was correctly split (50% of 1000 is 500)
                        let decoded_query: external::QueryMsg = from_json(msg).unwrap();
                        let external::QueryMsg::SimulateSwapOperations { offer_amount, .. } = decoded_query;
                        assert_eq!(offer_amount, Uint128::new(500));
                        
                        SystemResult::Ok(ContractResult::Ok(mock_response_b_binary.clone()))
                    } else {
                        panic!("Unexpected contract query to: {}", contract_addr);
                    }
                }
                _ => panic!("Unsupported WasmQuery type"),
            }
        });

        // --- 2. Setup Dependencies ---
        let mut deps = mock_dependencies();
        deps.querier = querier;

        // --- 3. Construct the Split Route ---
        let route = Route {
            steps: vec![
                // Path A: 50% of input
                Step {
                    protocol_address: Addr::unchecked(FAKE_AMM_ROUTER_A),
                    description: ActionDescription::AmmSwap {
                        protocol: AmmProtocol::Choice,
                        offer_asset_info: AssetInfo::NativeToken { denom: "inj".to_string() },
                        ask_asset_info: AssetInfo::Token { contract_addr: Addr::unchecked("token_a").to_string() },
                    },
                    amount_in_percentage: 50,
                    next_steps: vec![],
                },
                // Path B: 50% of input
                Step {
                    protocol_address: Addr::unchecked(FAKE_AMM_ROUTER_B),
                    description: ActionDescription::AmmSwap {
                        protocol: AmmProtocol::Choice,
                        offer_asset_info: AssetInfo::NativeToken { denom: "inj".to_string() },
                        ask_asset_info: AssetInfo::Token { contract_addr: Addr::unchecked("token_b").to_string() },
                    },
                    amount_in_percentage: 50,
                    next_steps: vec![],
                },
            ],
        };

        // --- 4. Call the Function ---
        let result_binary = simulate_route(
            deps.as_ref(),
            mock_env(),
            route,
            Coin::new(1000u128, "inj"),
        ).unwrap();

        // --- 5. Assert the Result ---
        let result: SimulateRouteResponse = from_json(&result_binary).unwrap();

        // The final output should be the SUM of the outputs from both paths
        assert_eq!(result.output_amount, Uint128::new(30000 + 45000));
    }

    #[test]
    fn test_simulate_multi_step_split_route() {
        // --- 1. Setup the Mock Querier for all 3 contracts ---
        let mut querier = MockQuerier::new(&[]);

        // Define canned responses for each contract
        let orderbook_response = orderbook::GetOutputQuantityResponse { return_amount: Uint128::new(200_000) }; // 100 USDT -> 200 INJ
        let orderbook_response_bin = to_json_binary(&orderbook_response).unwrap();

        let amm_a_response = external::SimulateSwapOperationsResponse { amount: Uint128::new(50_000) }; // 43% split -> 50k final tokens
        let amm_a_response_bin = to_json_binary(&amm_a_response).unwrap();

        let amm_b_response = external::SimulateSwapOperationsResponse { amount: Uint128::new(70_000) }; // 57% split -> 70k final tokens
        let amm_b_response_bin = to_json_binary(&amm_b_response).unwrap();

        querier.update_wasm(move |query: &WasmQuery| -> SystemResult<ContractResult<Binary>> {
            match query {
                WasmQuery::Smart { contract_addr, msg } => {
                    if contract_addr == FAKE_ORDERBOOK {
                        SystemResult::Ok(ContractResult::Ok(orderbook_response_bin.clone()))
                    } else if contract_addr == FAKE_AMM_ROUTER_A {
                        // Assert that this AMM received the correct 43% of the order book's output
                        // 200_000 * 0.43 = 86_000
                        let decoded: external::QueryMsg = from_json(msg).unwrap();
                        let external::QueryMsg::SimulateSwapOperations { offer_amount, .. } = decoded;
                        assert_eq!(offer_amount, Uint128::new(86_000));
                        SystemResult::Ok(ContractResult::Ok(amm_a_response_bin.clone()))
                    } else if contract_addr == FAKE_AMM_ROUTER_B {
                        // Assert that this AMM received the correct 57% of the order book's output
                        // 200_000 * 0.57 = 114_000
                        let decoded: external::QueryMsg = from_json(msg).unwrap();
                        let external::QueryMsg::SimulateSwapOperations { offer_amount, .. } = decoded;
                        assert_eq!(offer_amount, Uint128::new(114_000));
                        SystemResult::Ok(ContractResult::Ok(amm_b_response_bin.clone()))
                    } else {
                        panic!("Unexpected contract query to {}", contract_addr)
                    }
                }
                _ => panic!("Unsupported query type"),
            }
        });

        // --- 2. Setup Dependencies ---
        let mut deps = mock_dependencies();
        deps.querier = querier;

        // --- 3. Construct the Multi-Step Route ---
        let route = Route {
            steps: vec![
                // Step 0: USDT -> INJ via Order Book, then splits
                Step {
                    protocol_address: Addr::unchecked(FAKE_ORDERBOOK),
                    description: ActionDescription::OrderbookSwap {
                        source_denom: "peggy0xdAC...".to_string(),
                        target_denom: "inj".to_string(),
                    },
                    amount_in_percentage: 100,
                    next_steps: vec![1, 2], // Points to the next two steps
                },
                // Step 1: First AMM path (43%)
                Step {
                    protocol_address: Addr::unchecked(FAKE_AMM_ROUTER_A),
                    description: ActionDescription::AmmSwap {
                        protocol: AmmProtocol::Choice,
                        offer_asset_info: AssetInfo::NativeToken { denom: "inj".to_string() },
                        ask_asset_info: AssetInfo::Token { contract_addr: Addr::unchecked("final_token").to_string() },
                    },
                    amount_in_percentage: 43,
                    next_steps: vec![], // Leaf node
                },
                // Step 2: Second AMM path (57%)
                Step {
                    protocol_address: Addr::unchecked(FAKE_AMM_ROUTER_B),
                    description: ActionDescription::AmmSwap {
                        protocol: AmmProtocol::Choice,
                        offer_asset_info: AssetInfo::NativeToken { denom: "inj".to_string() },
                        ask_asset_info: AssetInfo::Token { contract_addr: Addr::unchecked("final_token").to_string() },
                    },
                    amount_in_percentage: 57,
                    next_steps: vec![], // Leaf node
                },
            ],
        };

        // --- 4. Call the Function ---
        let result_binary = simulate_route(
            deps.as_ref(),
            mock_env(),
            route,
            Coin::new(100u128, "peggy0xdAC..."),
        ).unwrap();

        // --- 5. Assert the Result ---
        let result: SimulateRouteResponse = from_json(&result_binary).unwrap();

        // The final output should be the SUM of the outputs from the two leaf nodes
        assert_eq!(result.output_amount, Uint128::new(50_000 + 70_000));
    }
}