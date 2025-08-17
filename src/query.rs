use cosmwasm_std::{to_json_binary, Binary, Coin, Deps, Env, StdResult, Uint128, WasmQuery};
use crate::msg::{external, Route, SimulateRouteResponse, ActionDescription, AmmProtocol};
use crate::state::Config;

pub fn simulate_route(
    deps: Deps,
    _env: Env,
    route: Route,
    amount_in: Coin,
) -> StdResult<Binary> {
    // This will hold the sum of all simulated output amounts from the final steps.
    let mut total_output_amount = Uint128::zero();

    // Iterate over all steps provided in the route.
    // This logic assumes a simple split where all steps are initiated from the original input amount.
    // A full implementation would need a graph traversal algorithm.
    for step in route.steps {
        // Calculate the portion of the input amount for this specific step.
        let amount_to_swap = amount_in.amount.multiply_ratio(
            step.amount_in_percentage as u128,
            100u128,
        );

        // Match on the description to decide what to do.
        match &step.description {
            ActionDescription::AmmSwap {
                protocol,
                offer_asset_info,
                ask_asset_info,
            } => {
                // Construct the appropriate operations list based on the protocol.
                let operations = match protocol {
                    AmmProtocol::Choice => {
                        vec![external::SwapOperation::Choice {
                            offer_asset_info: offer_asset_info.clone(),
                            ask_asset_info: ask_asset_info.clone(),
                        }]
                    }
                };

                // Construct the query message for the external AMM router.
                let amm_query = external::QueryMsg::SimulateSwapOperations {
                    offer_amount: amount_to_swap,
                    operations,
                };

                // Perform the smart query to the external contract.
                let sim_response: external::SimulateSwapOperationsResponse =
                    deps.querier.query(&WasmQuery::Smart {
                        contract_addr: step.protocol_address.to_string(),
                        msg: to_json_binary(&amm_query)?,
                    }.into())?;

                // Add the simulated amount from this path to the total.
                total_output_amount += sim_response.amount;
            }
         
        }
    }

    // Return the final aggregated amount.
    let response = SimulateRouteResponse { output_amount: total_output_amount };
    to_json_binary(&response)
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
}