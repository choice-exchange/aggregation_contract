#![cfg(test)]

use cosmwasm_std::{Coin};
use dex_aggregator::msg::{
    external, AmmSwapOp, ExecuteMsg, InstantiateMsg, Operation, OrderbookSwapOp, Split, Stage,
};
use injective_test_tube::{
    injective_std::types::cosmos::{bank::v1beta1::MsgSend, base::v1beta1::Coin as ProtoCoin},
    Account, Bank, InjectiveTestApp, Module, SigningAccount, Wasm,
};
use mock_swap::{InstantiateMsg as MockInstantiateMsg, ProtocolType, SwapConfig};

fn get_wasm_byte_code(filename: &str) -> &'static [u8] {
    match filename {
        "dex_aggregator.wasm" => include_bytes!("../artifacts/dex_aggregator.wasm"),
        "mock_swap.wasm" => include_bytes!("../artifacts/mock_swap.wasm"),
        "cw20_base.wasm" => include_bytes!("../cw20_base/cw20_base.wasm"),
        "cw20_adapter.wasm" => include_bytes!("../cw20_adapter/cw20_adapter.wasm"),
        _ => panic!("Unknown wasm file"),
    }
}

// ... The rest of the test setup and test cases from the previous answer go here ...
// Setup function and test cases remain the same.
pub struct TestEnv {
    pub app: InjectiveTestApp,
    pub admin: SigningAccount,
    pub user: SigningAccount,
    pub aggregator_addr: String,
    pub mock_amm_1_addr: String,
    pub mock_amm_2_addr: String,
    pub mock_ob_inj_usdt_addr: String, 
    pub mock_ob_usdt_inj_addr: String, 
}

/// Sets up the test environment, deploying the aggregator and three mock swap contracts.
fn setup() -> TestEnv {
    let app = InjectiveTestApp::new();

    let admin_initial_coins = &[
        Coin::new(1_000_000_000_000_000_000_000_000_000_000u128, "inj"),
        Coin::new(1_000_000_000_000_000_000_000_000_000_000u128, "usdt"),
    ];
    let admin_initial_decimals = &[
        18, // inj
        6,  // usdt
    ];

    let admin = app
        .init_account_decimals(admin_initial_coins, admin_initial_decimals)
        .unwrap();

    let user = app
        .init_account(&[
            Coin::new(1_000_000_000_000_000_000_000_000u128, "inj"),
            Coin::new(1_000_000_000_000_000_000_000_000u128, "usdt"),
        ])
        .unwrap();

    let wasm = Wasm::new(&app);

    // Store codes
    let aggregator_code_id = wasm
        .store_code(&get_wasm_byte_code("dex_aggregator.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let mock_swap_code_id = wasm
        .store_code(&get_wasm_byte_code("mock_swap.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;

    let _cw20_code_id = wasm
        .store_code(&get_wasm_byte_code("cw20_base.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let _cw20_adapter_code_id = wasm
        .store_code(&get_wasm_byte_code("cw20_adapter.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;

    // Instantiate aggregator
    let _aggregator_addr = wasm
        .instantiate(
            aggregator_code_id,
            &InstantiateMsg {
                admin: admin.address(),
            },
            Some(&admin.address()),
            Some("dex-aggregator"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // Instantiate mock contracts
    let aggregator_addr = wasm
        .instantiate(
            aggregator_code_id,
            &InstantiateMsg {
                admin: admin.address(),
            },
            Some(&admin.address()),
            Some("dex-aggregator"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // Instantiate mock contracts with our simple, clear rates
    let mock_amm_1_addr = wasm.instantiate(mock_swap_code_id, &MockInstantiateMsg {
        config: SwapConfig {
            input_denom: "inj".to_string(), output_denom: "usdt".to_string(), rate: "10.0".to_string(),
            protocol_type: ProtocolType::Amm, // This is an AMM
        },
    }, Some(&admin.address()), Some("mock-amm-1"), &[], &admin).unwrap().data.address;

    let mock_amm_2_addr = wasm.instantiate(mock_swap_code_id, &MockInstantiateMsg {
        config: SwapConfig {
            input_denom: "inj".to_string(), output_denom: "usdt".to_string(), rate: "20.0".to_string(),
            protocol_type: ProtocolType::Amm, // This is an AMM
        },
    }, Some(&admin.address()), Some("mock-amm-2"), &[], &admin).unwrap().data.address;

    let mock_ob_inj_usdt_addr = wasm.instantiate(mock_swap_code_id, &MockInstantiateMsg {
        config: SwapConfig {
            input_denom: "inj".to_string(), output_denom: "usdt".to_string(), rate: "30.0".to_string(),
            protocol_type: ProtocolType::Orderbook, // This is an Orderbook
        },
    }, Some(&admin.address()), Some("mock-ob-inj-usdt"), &[], &admin).unwrap().data.address;

    let mock_ob_usdt_inj_addr = wasm.instantiate(mock_swap_code_id, &MockInstantiateMsg {
        config: SwapConfig {
            input_denom: "usdt".to_string(), output_denom: "inj".to_string(), rate: "0.1".to_string(),
            protocol_type: ProtocolType::Orderbook, // This is an Orderbook
        },
    }, Some(&admin.address()), Some("mock-ob-usdt-inj"), &[], &admin).unwrap().data.address;

    let bank = Bank::new(&app);
    let funds_to_send = vec![
        ProtoCoin {
            denom: "inj".to_string(),
            amount: "1000000000000000000000000000".to_string(),
        },
        ProtoCoin {
            denom: "usdt".to_string(),
            amount: "1000000000000000000000000000".to_string(),
        },
    ];

    // Fund all three mock contracts from the admin account.
    for addr in [&mock_amm_1_addr, &mock_amm_2_addr, &mock_ob_inj_usdt_addr, &mock_ob_usdt_inj_addr] {
        bank.send(
            MsgSend {
                from_address: admin.address(),
                to_address: addr.clone(),
                amount: funds_to_send.clone(),
            },
            &admin,
        )
        .unwrap();
    }

    TestEnv {
        app,
        admin: admin,
        user: user,
        aggregator_addr,
        mock_amm_1_addr,
        mock_amm_2_addr,
        mock_ob_inj_usdt_addr,
        mock_ob_usdt_inj_addr
    }
}

#[test]
fn test_aggregate_swap_success() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    // Input: 100 INJ
    // Split 1 (33%): 33 INJ -> AMM1 @ 10.0 = 330 USDT
    // Split 2 (42%): 42 INJ -> AMM2 @ 20.0 = 840 USDT
    // Split 3 (25%): 25 INJ -> OB   @ 30.0 = 750 USDT
    // Total Output: 330 + 840 + 750 = 1920 USDT

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![Stage {
            splits: vec![
                Split {
                    percent: 33,
                    operation: Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_1_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        min_output: "320000000000000000000".to_string(), // 320 USDT
                    }),
                },
                Split {
                    percent: 42,
                    operation: Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_2_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        min_output: "830000000000000000000".to_string(), // 830 USDT
                    }),
                },
                Split {
                    percent: 25,
                    operation: Operation::OrderbookSwap(OrderbookSwapOp {
                        swap_contract: env.mock_ob_inj_usdt_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        min_output: "740000000000000000000".to_string(), // 740 USDT
                    }),
                },
            ],
        }],
        minimum_receive: Some("1910000000000000000000".to_string()), // Min 1910 USDT
    };

    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        // User sends 100 INJ
        &[Coin::new(100_000_000_000_000_000_000u128, "inj")],
        &env.user,
    );

    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());

    let response = res.unwrap();
    let success_event = response.events.iter().find(|e| {
        e.ty == "wasm"
            && e.attributes
                .iter()
                .any(|a| a.key == "action" && a.value == "aggregate_swap_complete")
    }).expect("Did not find success event in reply");

    let total_received_attr = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    // Assert the total expected output is 1920 USDT
    assert_eq!(total_received_attr.value, "1920000000000000000000");
}

#[test]
fn test_multi_stage_aggregate_swap_success() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    // Stage 1: 1,000,000 USDT -> OB @ 0.1 = 100,000 INJ
    // Stage 2:
    //   Split 1 (49%): 49,000 INJ -> AMM1 @ 10.0 = 490,000 USDT
    //   Split 2 (51%): 51,000 INJ -> AMM2 @ 20.0 = 1,020,000 USDT
    // Total Final Output: 490,000 + 1,020,000 = 1,510,000 USDT

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![
            // Stage 1: 100% of USDT to the Orderbook to get INJ.
            Stage {
                splits: vec![Split {
                    percent: 100,
                    operation: Operation::OrderbookSwap(OrderbookSwapOp {
                        swap_contract: env.mock_ob_usdt_inj_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        min_output: "99000000000000000000000".to_string(), // 99,000 INJ
                    }),
                }],
            },
            // Stage 2: The resulting INJ is split 49/51 across two AMMs to get final USDT.
            Stage {
                splits: vec![
                    Split {
                        percent: 49,
                        operation: Operation::AmmSwap(AmmSwapOp {
                            pool_address: env.mock_amm_1_addr.clone(),
                            ask_asset_info: external::AssetInfo::NativeToken {
                                denom: "usdt".to_string(),
                            },
                            min_output: "480000000000000000000000".to_string(), // 480,000 USDT
                        }),
                    },
                    Split {
                        percent: 51,
                        operation: Operation::AmmSwap(AmmSwapOp {
                            pool_address: env.mock_amm_2_addr.clone(),
                            ask_asset_info: external::AssetInfo::NativeToken {
                                denom: "usdt".to_string(),
                            },
                            min_output: "1010000000000000000000000".to_string(), // 1,010,000 USDT
                        }),
                    },
                ],
            },
        ],
        // The minimum we expect from summing the Stage 2 outputs.
        minimum_receive: Some("1500000000000000000000000".to_string()), // 1,500,000 USDT
    };

    // The initial funds for this route are 1,000,000 USDT
    let initial_funds = Coin::new(1_000_000_000_000_000_000_000_000u128, "usdt");

    let res = wasm.execute(&env.aggregator_addr, &msg, &[initial_funds], &env.user);

    assert!(
        res.is_ok(),
        "Multi-stage execution failed: {:?}",
        res.unwrap_err()
    );
    let response = res.unwrap();

    let success_event = response.events.iter().find(|e| {
        e.ty.starts_with("wasm")
            && e.attributes
                .iter()
                .any(|a| a.key == "action" && a.value == "aggregate_swap_complete")
    }).expect("Did not find final aggregate_swap_complete event");

    let final_received_attr = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    // Expected final amount is 1,510,000 USDT
    let expected_final_amount = "1510000000000000000000000";
    assert_eq!(final_received_attr.value, expected_final_amount);
}