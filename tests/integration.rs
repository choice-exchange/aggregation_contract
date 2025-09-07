#![cfg(test)]

use std::str::FromStr;

use cosmwasm_std::{to_json_binary, Addr, Coin, Decimal, Uint128};
use cw20::{BalanceResponse, Cw20QueryMsg};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
use dex_aggregator::msg::{
    cw20_adapter, external, AmmSwapOp, Cw20HookMsg, ExecuteMsg, InstantiateMsg, Operation,
    OrderbookSwapOp, QueryMsg, Split, Stage,
};
use dex_aggregator::state::Config as AggregatorConfig;
use injective_test_tube::{
    injective_std::types::cosmos::{
        bank::v1beta1::{MsgSend, QueryBalanceRequest},
        base::v1beta1::Coin as ProtoCoin,
    },
    Account, Bank, InjectiveTestApp, Module, SigningAccount, Wasm,
};
use mock_swap::{AssetInfo, InstantiateMsg as MockInstantiateMsg, ProtocolType, SwapConfig};

fn get_wasm_byte_code(filename: &str) -> &'static [u8] {
    match filename {
        "dex_aggregator.wasm" => include_bytes!("../artifacts/dex_aggregator.wasm"),
        "mock_swap.wasm" => include_bytes!("../artifacts/mock_swap.wasm"),
        "cw20_base.wasm" => include_bytes!("../cw20_base/cw20_base.wasm"),
        "cw20_adapter.wasm" => include_bytes!("../cw20_adapter/cw20_adapter.wasm"),
        _ => panic!("Unknown wasm file"),
    }
}

pub struct TestEnv {
    pub app: InjectiveTestApp,
    pub admin: SigningAccount,
    pub user: SigningAccount,
    pub fee_collector: SigningAccount,
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
        Coin::new(1_000_000_000_000_000_000u128, "usdt"),
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
            Coin::new(1_000_000_000_000u128, "usdt"),
        ])
        .unwrap();

    let fee_collector_account = app.init_account(&[]).unwrap();

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
    let cw20_adapter_code_id = wasm
        .store_code(&get_wasm_byte_code("cw20_adapter.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;

    let adapter_addr = wasm
        .instantiate(
            cw20_adapter_code_id,
            &cw20_adapter::InstantiateMsg {},
            Some(&admin.address()),
            Some("cw20-adapter"),
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
                cw20_adapter_address: adapter_addr,
                fee_collector_address: fee_collector_account.address(),
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
    let mock_amm_1_addr = wasm
        .instantiate(
            mock_swap_code_id,
            &MockInstantiateMsg {
                config: SwapConfig {
                    input_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    output_asset_info: AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    rate: "10.0".to_string(),
                    protocol_type: ProtocolType::Amm, // This is an AMM
                    input_decimals: 18,
                    output_decimals: 6,
                },
            },
            Some(&admin.address()),
            Some("mock-amm-1"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let mock_amm_2_addr = wasm
        .instantiate(
            mock_swap_code_id,
            &MockInstantiateMsg {
                config: SwapConfig {
                    input_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    output_asset_info: AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    rate: "20.0".to_string(),
                    protocol_type: ProtocolType::Amm, // This is an AMM
                    input_decimals: 18,
                    output_decimals: 6,
                },
            },
            Some(&admin.address()),
            Some("mock-amm-2"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let mock_ob_inj_usdt_addr = wasm
        .instantiate(
            mock_swap_code_id,
            &MockInstantiateMsg {
                config: SwapConfig {
                    input_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    output_asset_info: AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    rate: "30.0".to_string(),
                    protocol_type: ProtocolType::Orderbook, // This is an Orderbook
                    input_decimals: 18,
                    output_decimals: 6,
                },
            },
            Some(&admin.address()),
            Some("mock-ob-inj-usdt"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let mock_ob_usdt_inj_addr = wasm
        .instantiate(
            mock_swap_code_id,
            &MockInstantiateMsg {
                config: SwapConfig {
                    input_asset_info: AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    output_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    rate: "0.1".to_string(),
                    protocol_type: ProtocolType::Orderbook, // This is an Orderbook
                    input_decimals: 6,
                    output_decimals: 18,
                },
            },
            Some(&admin.address()),
            Some("mock-ob-usdt-inj"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let bank = Bank::new(&app);
    let funds_to_send = vec![
        ProtoCoin {
            denom: "inj".to_string(),
            amount: "1000000000000000000000000000".to_string(),
        },
        ProtoCoin {
            denom: "usdt".to_string(),
            amount: "1000000000000000".to_string(),
        },
    ];

    // Fund all three mock contracts from the admin account.
    for addr in [
        &mock_amm_1_addr,
        &mock_amm_2_addr,
        &mock_ob_inj_usdt_addr,
        &mock_ob_usdt_inj_addr,
    ] {
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
        fee_collector: fee_collector_account,
        aggregator_addr,
        mock_amm_1_addr,
        mock_amm_2_addr,
        mock_ob_inj_usdt_addr,
        mock_ob_usdt_inj_addr,
    }
}

#[test]
fn test_aggregate_swap_success() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    let bank = Bank::new(&env.app);

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
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    }),
                },
                Split {
                    percent: 42,
                    operation: Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_2_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    }),
                },
                Split {
                    percent: 25,
                    operation: Operation::OrderbookSwap(OrderbookSwapOp {
                        swap_contract: env.mock_ob_inj_usdt_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    }),
                },
            ],
        }],
        minimum_receive: Some("1910000000".to_string()), // Min 1910 USDT
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
    let success_event = response
        .events
        .iter()
        .find(|e| {
            e.ty == "wasm"
                && e.attributes
                    .iter()
                    .any(|a| a.key == "action" && a.value == "aggregate_swap_complete")
        })
        .expect("Did not find success event in reply");

    let total_received_attr = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    // Assert the total expected output is 1920 USDT
    assert_eq!(total_received_attr.value, "1920000000");

    let balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: env.user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();

    // The user's final balance should be their initial balance + the swap output.
    // Initial: 1_000_000_000_000 (from setup)
    // Swap Output: 1_920_000_000 (1920 USDT)
    // Expected Final: 1_001_920_000_000
    let expected_final_balance = Uint128::new(1_001_920_000_000u128);

    // Extract the amount from the query response
    let final_balance = balance_response.balance.unwrap();
    let final_amount = Uint128::from_str(&final_balance.amount).unwrap();

    // Assert the final balance is correct
    assert_eq!(final_amount, expected_final_balance);
    assert_eq!(final_balance.denom, "usdt");
}

#[test]
fn test_multi_stage_aggregate_swap_success() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

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
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
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
                            offer_asset_info: external::AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                        }),
                    },
                    Split {
                        percent: 51,
                        operation: Operation::AmmSwap(AmmSwapOp {
                            pool_address: env.mock_amm_2_addr.clone(),
                            ask_asset_info: external::AssetInfo::NativeToken {
                                denom: "usdt".to_string(),
                            },
                            offer_asset_info: external::AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                        }),
                    },
                ],
            },
        ],
        // The minimum we expect from summing the Stage 2 outputs.
        minimum_receive: Some("1500000000000".to_string()), // 1,500,000 USDT
    };

    // The initial funds for this route are 1,000,000 USDT
    let initial_funds = Coin::new(1_000_000_000_000u128, "usdt");

    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        &[initial_funds.clone()],
        &env.user,
    );

    assert!(
        res.is_ok(),
        "Multi-stage execution failed: {:?}",
        res.unwrap_err()
    );
    let response = res.unwrap();

    let success_event = response
        .events
        .iter()
        .find(|e| {
            e.ty.starts_with("wasm")
                && e.attributes
                    .iter()
                    .any(|a| a.key == "action" && a.value == "aggregate_swap_complete")
        })
        .expect("Did not find final aggregate_swap_complete event");

    let final_received_attr = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    // Expected final amount is 1,510,000 USDT
    let expected_final_amount = "1510000000000";
    assert_eq!(final_received_attr.value, expected_final_amount);

    let balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: env.user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();

    // The user's final balance should be their initial balance minus the input amount, plus the swap output.
    // Initial: 1_000_000_000_000 (from setup)
    // Input:   1_000_000_000_000
    // Output:  1_510_000_000_000
    // Expected Final: 1_000_000_000_000 - 1_000_000_000_000 + 1_510_000_000_000 = 1_510_000_000_000
    let initial_user_balance = 1_000_000_000_000u128; // Assuming this is the initial balance from setup()
    let expected_final_balance = Uint128::new(initial_user_balance)
        - Uint128::new(initial_funds.amount.u128())
        + Uint128::from_str(expected_final_amount).unwrap();

    // Extract the amount from the query response
    let final_balance = balance_response.balance.unwrap();
    let final_amount = Uint128::from_str(&final_balance.amount).unwrap();

    // Assert the final balance is correct
    assert_eq!(final_amount, expected_final_balance);
    assert_eq!(final_balance.denom, "usdt");
}

pub struct ConversionTestSetup {
    pub env: TestEnv,
    pub shroom_cw20_addr: String,
    pub sai_cw20_addr: String,
    pub adapter_addr: String,
    pub mock_inj_to_native_shroom_ob: String,
    pub mock_inj_to_cw20_shroom_amm: String,
    pub mock_cw20_shroom_to_cw20_sai_amm: String,
    pub mock_usdt_to_inj_ob: String,
    pub mock_native_shroom_to_usdt_ob: String,
    pub mock_cw20_shroom_to_usdt_amm: String,
}

fn setup_for_conversion_test() -> ConversionTestSetup {
    let app = InjectiveTestApp::new();
    let admin = app
        .init_account(&[
            Coin::new(1_000_000_000_000_000_000_000_000u128, "inj"),
            Coin::new(1_000_000_000_000u128, "usdt"),
        ])
        .unwrap();
    let user = app
        .init_account(&[
            Coin::new(100_000_000_000_000_000_000u128, "inj"),
            Coin::new(1_000_000_000_000u128, "usdt"),
        ])
        .unwrap();
    let fee_collector_account = app.init_account(&[]).unwrap();

    let wasm = Wasm::new(&app);

    // 1. Store all contract codes
    let aggregator_code_id = wasm
        .store_code(get_wasm_byte_code("dex_aggregator.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let mock_swap_code_id = wasm
        .store_code(get_wasm_byte_code("mock_swap.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let cw20_code_id = wasm
        .store_code(get_wasm_byte_code("cw20_base.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let adapter_code_id = wasm
        .store_code(get_wasm_byte_code("cw20_adapter.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;

    // 2. Deploy core infrastructure
    let adapter_addr = wasm
        .instantiate(
            adapter_code_id,
            &cw20_adapter::InstantiateMsg {},
            Some(&admin.address()),
            Some("adapter"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;
    let aggregator_addr = wasm
        .instantiate(
            aggregator_code_id,
            &InstantiateMsg {
                admin: admin.address(),
                cw20_adapter_address: adapter_addr.clone(),
                fee_collector_address: fee_collector_account.address(),
            },
            Some(&admin.address()),
            Some("aggregator"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // 3. Deploy Token Contracts (SHROOM and SAI)
    let shroom_cw20_addr = wasm
        .instantiate(
            cw20_code_id,
            &Cw20InstantiateMsg {
                name: "Shroom".to_string(),
                symbol: "SHROOM".to_string(),
                decimals: 6,
                initial_balances: vec![],
                mint: Some(cw20::MinterResponse {
                    minter: admin.address(),
                    cap: None,
                }),
                marketing: None,
            },
            Some(&admin.address()),
            Some("shroom"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;
    let sai_cw20_addr = wasm
        .instantiate(
            cw20_code_id,
            &Cw20InstantiateMsg {
                name: "Sai".to_string(),
                symbol: "SAI".to_string(),
                decimals: 6,
                initial_balances: vec![],
                mint: Some(cw20::MinterResponse {
                    minter: admin.address(),
                    cap: None,
                }),
                marketing: None,
            },
            Some(&admin.address()),
            Some("sai"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let total_fee = Coin::new(10_000_000_000_000_000_000u128, "inj");

    // 4. Register tokens with the adapter
    wasm.execute(
        &adapter_addr,
        &cw20_adapter::ExecuteMsg::RegisterCw20Contract {
            addr: Addr::unchecked(shroom_cw20_addr.clone()),
        },
        &[total_fee.clone()],
        &admin,
    )
    .unwrap();
    wasm.execute(
        &adapter_addr,
        &cw20_adapter::ExecuteMsg::RegisterCw20Contract {
            addr: Addr::unchecked(sai_cw20_addr.clone()),
        },
        &[total_fee.clone()],
        &admin,
    )
    .unwrap();
    // 5. Deploy and Fund Mock DEXs
    let native_shroom_denom = format!("factory/{}/{}", adapter_addr, shroom_cw20_addr);

    // DEX 1: INJ -> SHROOM (native)
    let mock_inj_to_native_shroom_ob = wasm
        .instantiate(
            mock_swap_code_id,
            &MockInstantiateMsg {
                config: SwapConfig {
                    input_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    output_asset_info: AssetInfo::NativeToken {
                        denom: native_shroom_denom.clone(),
                    },
                    rate: "100.0".to_string(),
                    protocol_type: ProtocolType::Orderbook,
                    input_decimals: 18,
                    output_decimals: 6,
                },
            },
            Some(&admin.address()),
            Some("ob-inj-shroom"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // DEX 2: INJ -> SHROOM (cw20)
    let mock_inj_to_cw20_shroom_amm = wasm
        .instantiate(
            mock_swap_code_id,
            &MockInstantiateMsg {
                config: SwapConfig {
                    input_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    output_asset_info: AssetInfo::Token {
                        contract_addr: shroom_cw20_addr.clone(),
                    },
                    rate: "100.0".to_string(),
                    protocol_type: ProtocolType::Amm,
                    input_decimals: 18,
                    output_decimals: 6,
                },
            },
            Some(&admin.address()),
            Some("amm-inj-shroom"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // DEX 3: SHROOM (cw20) -> SAI (cw20)
    let mock_cw20_shroom_to_cw20_sai_amm = wasm
        .instantiate(
            mock_swap_code_id,
            &MockInstantiateMsg {
                config: SwapConfig {
                    input_asset_info: AssetInfo::Token {
                        contract_addr: shroom_cw20_addr.clone(),
                    },
                    output_asset_info: AssetInfo::Token {
                        contract_addr: sai_cw20_addr.clone(),
                    },
                    rate: "0.1".to_string(),
                    protocol_type: ProtocolType::Amm,
                    input_decimals: 6,
                    output_decimals: 6,
                },
            },
            Some(&admin.address()),
            Some("amm-shroom-sai"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let mock_usdt_to_inj_ob = wasm
        .instantiate(
            mock_swap_code_id,
            &MockInstantiateMsg {
                config: SwapConfig {
                    input_asset_info: AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    output_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    rate: "0.1".to_string(), // Rate: 1 USDT = 0.1 INJ
                    protocol_type: ProtocolType::Orderbook,
                    input_decimals: 6,
                    output_decimals: 18,
                },
            },
            Some(&admin.address()),
            Some("ob-usdt-inj"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let mock_native_shroom_to_usdt_ob = wasm
        .instantiate(
            mock_swap_code_id,
            &MockInstantiateMsg {
                config: SwapConfig {
                    input_asset_info: AssetInfo::NativeToken {
                        denom: native_shroom_denom.clone(), // ACCEPTS NATIVE SHROOM
                    },
                    output_asset_info: AssetInfo::NativeToken {
                        denom: "usdt".to_string(), // PAYS OUT USDT
                    },
                    rate: "0.5".to_string(), // Rate: 1 SHROOM = 0.5 USDT
                    protocol_type: ProtocolType::Orderbook,
                    input_decimals: 6,
                    output_decimals: 6,
                },
            },
            Some(&admin.address()),
            Some("ob-native-shroom-usdt"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let mock_cw20_shroom_to_usdt_amm = wasm
        .instantiate(
            mock_swap_code_id,
            &MockInstantiateMsg {
                config: SwapConfig {
                    input_asset_info: AssetInfo::Token {
                        contract_addr: shroom_cw20_addr.clone(),
                    },
                    output_asset_info: AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    rate: "0.4".to_string(), // The specific rate for our test
                    protocol_type: ProtocolType::Amm,
                    input_decimals: 6,  // SHROOM decimals
                    output_decimals: 6, // USDT decimals
                },
            },
            Some(&admin.address()),
            Some("amm-cw20-shroom-usdt"), // A clear, new label
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    wasm.execute(
        &shroom_cw20_addr,
        &cw20_base::msg::ExecuteMsg::Mint {
            recipient: mock_inj_to_cw20_shroom_amm.clone(),
            amount: Uint128::new(100_000_000_000),
        },
        &[],
        &admin,
    )
    .unwrap();
    wasm.execute(
        &sai_cw20_addr,
        &cw20_base::msg::ExecuteMsg::Mint {
            recipient: mock_cw20_shroom_to_cw20_sai_amm.clone(),
            amount: Uint128::new(100_000_000_000),
        },
        &[],
        &admin,
    )
    .unwrap();

    // 2. Fund the ADAPTER with a liquidity pool of CW20 SHROOM for conversions.
    wasm.execute(
        &shroom_cw20_addr,
        &cw20_base::msg::ExecuteMsg::Mint {
            recipient: adapter_addr.clone(),
            amount: Uint128::new(100_000_000_000),
        },
        &[],
        &admin,
    )
    .unwrap();

    // 3. Fund the DEX that pays out in NATIVE SHROOM.
    // To do this, the admin first needs to create some native shroom.
    let native_shroom_to_create = Uint128::new(1_000_000_000_000); // 100k
                                                                   // Mint cw20 to admin
    wasm.execute(
        &shroom_cw20_addr,
        &cw20_base::msg::ExecuteMsg::Mint {
            recipient: admin.address(),
            amount: native_shroom_to_create,
        },
        &[],
        &admin,
    )
    .unwrap();
    // Admin sends cw20 to adapter, which mints native shroom and sends it back to the admin.
    wasm.execute(
        &shroom_cw20_addr,
        &cw20::Cw20ExecuteMsg::Send {
            contract: adapter_addr.clone(),
            amount: native_shroom_to_create,
            msg: to_json_binary(&"{}").unwrap(),
        },
        &[],
        &admin,
    )
    .unwrap();

    // Now admin has native shroom and can fund the DEX.
    let bank = Bank::new(&app);
    bank.send(
        MsgSend {
            from_address: admin.address(),
            to_address: mock_inj_to_native_shroom_ob.clone(),
            amount: vec![ProtoCoin {
                denom: native_shroom_denom,
                amount: native_shroom_to_create.to_string(),
            }],
        },
        &admin,
    )
    .unwrap();

    bank.send(
        MsgSend {
            from_address: admin.address(),
            to_address: mock_usdt_to_inj_ob.clone(),
            amount: vec![ProtoCoin {
                denom: "inj".to_string(),
                amount: "10000000000000000000000".to_string(), // 10,000 INJ
            }],
        },
        &admin,
    )
    .unwrap();

    bank.send(
        MsgSend {
            from_address: admin.address(),
            to_address: mock_native_shroom_to_usdt_ob.clone(),
            amount: vec![ProtoCoin {
                denom: "usdt".to_string(),
                amount: "10000000000".to_string(), // 10,000 USDT
            }],
        },
        &admin,
    )
    .unwrap();

    bank.send(
        MsgSend {
            from_address: admin.address(),
            to_address: mock_cw20_shroom_to_usdt_amm.clone(),
            amount: vec![ProtoCoin {
                denom: "usdt".to_string(),
                amount: "10000000000".to_string(), // 10,000 USDT
            }],
        },
        &admin,
    )
    .unwrap();

    ConversionTestSetup {
        env: TestEnv {
            app,
            admin,
            user,
            fee_collector: fee_collector_account,
            aggregator_addr,
            mock_amm_1_addr: "".to_string(),
            mock_amm_2_addr: "".to_string(),
            mock_ob_inj_usdt_addr: "".to_string(),
            mock_ob_usdt_inj_addr: "".to_string(),
        },
        shroom_cw20_addr,
        sai_cw20_addr,
        adapter_addr,
        mock_inj_to_native_shroom_ob,
        mock_inj_to_cw20_shroom_amm,
        mock_cw20_shroom_to_cw20_sai_amm,
        mock_usdt_to_inj_ob,
        mock_native_shroom_to_usdt_ob,
        mock_cw20_shroom_to_usdt_amm,
    }
}

#[test]
fn test_full_normalization_route() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;

    // ROUTE: 10 INJ -> 50% to native SHROOM, 50% to cw20 SHROOM -> unified to cw20 SHROOM -> final swap to cw20 SAI
    // 10 INJ -> 1000 SHROOM total (500 native + 500 cw20)
    // 1000 SHROOM -> 100 SAI (rate of 0.1)

    let native_shroom_denom = format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr);

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![
            // Stage 1: INJ -> SHROOM (mixed native/cw20 output)
            Stage {
                splits: vec![
                    Split {
                        percent: 50,
                        operation: Operation::OrderbookSwap(OrderbookSwapOp {
                            swap_contract: setup.mock_inj_to_native_shroom_ob.clone(),
                            offer_asset_info: external::AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                            ask_asset_info: external::AssetInfo::NativeToken {
                                denom: native_shroom_denom.clone(),
                            },
                        }),
                    },
                    Split {
                        percent: 50,
                        operation: Operation::AmmSwap(AmmSwapOp {
                            pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                            offer_asset_info: external::AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                            ask_asset_info: external::AssetInfo::Token {
                                contract_addr: setup.shroom_cw20_addr.clone(),
                            },
                        }),
                    },
                ],
            },
            // Stage 2: SHROOM (cw20) -> SAI (cw20)
            Stage {
                splits: vec![Split {
                    percent: 100,
                    operation: Operation::AmmSwap(AmmSwapOp {
                        pool_address: setup.mock_cw20_shroom_to_cw20_sai_amm.clone(),
                        offer_asset_info: external::AssetInfo::Token {
                            contract_addr: setup.shroom_cw20_addr.clone(),
                        },
                        ask_asset_info: external::AssetInfo::Token {
                            contract_addr: setup.sai_cw20_addr.clone(),
                        },
                    }),
                }],
            },
        ],
        minimum_receive: Some("97000000".to_string()), // 97 SAI
    };

    let res = wasm.execute(
        &setup.env.aggregator_addr,
        &msg,
        &[Coin::new(10_000_000_000_000_000_000u128, "inj")],
        user,
    );
    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());

    let balance: BalanceResponse = wasm
        .query(
            &setup.sai_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: user.address(),
            },
        )
        .unwrap();

    assert_eq!(balance.balance, Uint128::new(100_000_000));
}

#[test]
fn test_multi_stage_with_final_normalization() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;

    // THE ROUTE:
    // Stage 1: 1,000 USDT -> OB @ 0.1 = 100 INJ
    // Stage 2: 100 INJ is split:
    //   - 10% (10 INJ) -> AMM @ 100.0 = 1,000 CW20 SHROOM
    //   - 90% (90 INJ) -> OB  @ 100.0 = 9,000 Native SHROOM
    // Final Result: The aggregator normalizes the 9,000 Native SHROOM and sends the
    // total 10,000 CW20 SHROOM to the user.

    let native_shroom_denom = format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr);

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![
            // Stage 1: 100% of USDT to the Orderbook to get INJ.
            Stage {
                splits: vec![Split {
                    percent: 100,
                    operation: Operation::OrderbookSwap(OrderbookSwapOp {
                        swap_contract: setup.mock_usdt_to_inj_ob.clone(),
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    }),
                }],
            },
            // Stage 2: The resulting INJ is split 10/90 to get a mix of SHROOM types.
            Stage {
                splits: vec![
                    Split {
                        percent: 10, // 10% to CW20 SHROOM
                        operation: Operation::AmmSwap(AmmSwapOp {
                            pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                            offer_asset_info: external::AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                            ask_asset_info: external::AssetInfo::Token {
                                contract_addr: setup.shroom_cw20_addr.clone(),
                            },
                        }),
                    },
                    Split {
                        percent: 90, // 90% to Native SHROOM
                        operation: Operation::OrderbookSwap(OrderbookSwapOp {
                            swap_contract: setup.mock_inj_to_native_shroom_ob.clone(),
                            offer_asset_info: external::AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                            ask_asset_info: external::AssetInfo::NativeToken {
                                denom: native_shroom_denom.clone(),
                            },
                        }),
                    },
                ],
            },
        ],
        // The final expected output is unified CW20 SHROOM
        minimum_receive: Some("9900000000".to_string()), // Min 9,900 CW20 SHROOM
    };

    // The user initiates the swap with 1,000 USDT
    let initial_funds = Coin::new(1_000_000_000u128, "usdt"); // 1,000 USDT with 6 decimals

    let res = wasm.execute(&setup.env.aggregator_addr, &msg, &[initial_funds], user);
    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());

    // Assert the final outcome.
    // The aggregator should have performed the swaps, normalized the assets, and sent
    // the final unified CW20 SHROOM to the user.
    let balance: BalanceResponse = wasm
        .query(
            &setup.shroom_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: user.address(),
            },
        )
        .unwrap();

    // Expected final amount: 10,000 SHROOM (with 6 decimals)
    let expected_final_balance = Uint128::new(10_000_000_000u128);
    assert_eq!(balance.balance, expected_final_balance);
}

#[test]
fn test_cw20_entry_point_swap_success() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;
    let admin = &setup.env.admin;

    // Mint some SHROOM tokens directly to the user so they can initiate the swap.
    let initial_shroom_amount = Uint128::new(1_000_000_000u128); // 1,000 SHROOM
    wasm.execute(
        &setup.shroom_cw20_addr,
        &cw20_base::msg::ExecuteMsg::Mint {
            recipient: user.address(),
            amount: initial_shroom_amount,
        },
        &[],
        admin,
    )
    .unwrap();

    let initial_shroom_balance: BalanceResponse = wasm
        .query(
            &setup.shroom_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: user.address(),
            },
        )
        .unwrap();
    assert_eq!(initial_shroom_balance.balance, initial_shroom_amount);

    let initial_sai_balance: BalanceResponse = wasm
        .query(
            &setup.sai_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: user.address(),
            },
        )
        .unwrap();
    assert_eq!(initial_sai_balance.balance, Uint128::zero());

    // --- Define the Swap ---
    // The user wants to swap 1,000 SHROOM for SAI.
    // The mock AMM rate is 0.1, so they expect 100 SAI in return.
    let hook_msg = Cw20HookMsg::AggregateSwaps {
        stages: vec![Stage {
            splits: vec![Split {
                percent: 100,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: setup.mock_cw20_shroom_to_cw20_sai_amm.clone(),
                    offer_asset_info: external::AssetInfo::Token {
                        contract_addr: setup.shroom_cw20_addr.clone(),
                    },
                    ask_asset_info: external::AssetInfo::Token {
                        contract_addr: setup.sai_cw20_addr.clone(),
                    },
                }),
            }],
        }],
        minimum_receive: Some("99000000".to_string()), // Min 99 SAI
    };

    let res = wasm.execute(
        &setup.shroom_cw20_addr,
        &cw20::Cw20ExecuteMsg::Send {
            contract: setup.env.aggregator_addr.clone(),
            amount: initial_shroom_amount,
            msg: to_json_binary(&hook_msg).unwrap(),
        },
        &[],
        user,
    );

    assert!(
        res.is_ok(),
        "CW20 entry point execution failed: {:?}",
        res.unwrap_err()
    );

    let final_shroom_balance: BalanceResponse = wasm
        .query(
            &setup.shroom_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: user.address(),
            },
        )
        .unwrap();
    assert_eq!(final_shroom_balance.balance, Uint128::zero());

    let final_sai_balance: BalanceResponse = wasm
        .query(
            &setup.sai_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: user.address(),
            },
        )
        .unwrap();

    let expected_sai_balance = Uint128::new(100_000_000u128);
    assert_eq!(final_sai_balance.balance, expected_sai_balance);
}

#[test]
fn test_reverse_normalization_route() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;
    let bank = Bank::new(&setup.env.app);

    // --- THE ROUTE ---
    // Stage 1: 10 INJ -> AMM @ 100.0 = 1,000 CW20 SHROOM
    //   - After this, the aggregator holds 1,000 CW20 SHROOM.
    // Stage 2: 1,000 Native SHROOM -> OB @ 0.5 = 500 USDT
    //   - This stage REQUIRES Native SHROOM. The aggregator must automatically convert
    //     its CW20 SHROOM balance from Stage 1 into Native SHROOM to proceed.
    // Final Result: The user receives 500 USDT.

    let native_shroom_denom = format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr);

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![
            // Stage 1: Get CW20 SHROOM
            Stage {
                splits: vec![Split {
                    percent: 100,
                    operation: Operation::AmmSwap(AmmSwapOp {
                        pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: external::AssetInfo::Token {
                            contract_addr: setup.shroom_cw20_addr.clone(),
                        },
                    }),
                }],
            },
            // Stage 2: Swap Native SHROOM for USDT
            Stage {
                splits: vec![Split {
                    percent: 100,
                    operation: Operation::OrderbookSwap(OrderbookSwapOp {
                        swap_contract: setup.mock_native_shroom_to_usdt_ob.clone(),
                        // This is the key part of the test: the offer asset is NATIVE
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: native_shroom_denom.clone(),
                        },
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                    }),
                }],
            },
        ],
        minimum_receive: Some("495000000".to_string()), // Min 495 USDT
    };

    let initial_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_amount = Uint128::from_str(&initial_balance.amount).unwrap();

    let res = wasm.execute(
        &setup.env.aggregator_addr,
        &msg,
        &[Coin::new(10_000_000_000_000_000_000u128, "inj")], // User sends 10 INJ
        user,
    );
    assert!(
        res.is_ok(),
        "Reverse normalization execution failed: {:?}",
        res.unwrap_err()
    );

    let final_balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();

    let swap_output = Uint128::new(500_000_000u128);
    let expected_final_balance = initial_amount + swap_output;

    let final_balance = final_balance_response.balance.unwrap();
    let final_amount = Uint128::from_str(&final_balance.amount).unwrap();

    assert_eq!(final_amount, expected_final_balance);
}

#[test]
fn test_failure_if_minimum_receive_not_met() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let user = &env.user;
    let bank = Bank::new(&env.app);

    let initial_inj_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "inj".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_inj_amount = Uint128::from_str(&initial_inj_balance.amount).unwrap();

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
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    }),
                },
                Split {
                    percent: 42,
                    operation: Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_2_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    }),
                },
                Split {
                    percent: 25,
                    operation: Operation::OrderbookSwap(OrderbookSwapOp {
                        swap_contract: env.mock_ob_inj_usdt_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    }),
                },
            ],
        }],

        minimum_receive: Some("1920000001".to_string()),
    };

    let funds_to_send = Coin::new(100_000_000_000_000_000_000u128, "inj");
    let res = wasm.execute(&env.aggregator_addr, &msg, &[funds_to_send.clone()], user);

    assert!(
        res.is_err(),
        "Transaction should have failed due to not meeting minimum receive, but it succeeded"
    );

    let error = res.unwrap_err();
    assert!(
        error.to_string().contains("Minimum receive amount not met"),
        "Error message was not the expected 'MinimumReceiveNotMet'. Got: {}",
        error
    );

    let final_inj_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "inj".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let final_inj_amount = Uint128::from_str(&final_inj_balance.amount).unwrap();

    assert_eq!(
        initial_inj_amount, final_inj_amount,
        "User's INJ balance changed despite the transaction failing"
    );
}

#[test]
fn test_failure_on_invalid_percentage_sum() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let user = &env.user;
    let bank = Bank::new(&env.app);

    let initial_inj_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "inj".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_inj_amount = Uint128::from_str(&initial_inj_balance.amount).unwrap();

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![Stage {
            splits: vec![
                Split {
                    percent: 50, // 50%
                    operation: Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_1_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    }),
                },
                Split {
                    percent: 49, // + 49% = 99% (Invalid!)
                    operation: Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_2_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: external::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    }),
                },
            ],
        }],
        minimum_receive: None,
    };

    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        &[Coin::new(100_000_000_000_000_000_000u128, "inj")],
        user,
    );

    assert!(
        res.is_err(),
        "Transaction should have failed due to invalid percentage sum, but it succeeded"
    );

    let error = res.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Percentages in a stage must sum to 100"),
        "Error message was not the expected 'InvalidPercentageSum'. Got: {}",
        error
    );

    let final_inj_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "inj".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let final_inj_amount = Uint128::from_str(&final_inj_balance.amount).unwrap();

    assert_eq!(
        initial_inj_amount, final_inj_amount,
        "User's INJ balance changed despite the transaction failing due to invalid input"
    );
}

#[test]
fn test_mixed_input_unified_output_reconciliation() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;
    let bank = Bank::new(&setup.env.app);

    // --- SCENARIO ---
    // Stage 1: 10 INJ -> 1000 CW20 SHROOM.
    // Stage 2: Requires a mixed input (600 Native SHROOM, 400 CW20 SHROOM).
    // Reconciliation: Must convert 600 of the CW20 SHROOM to Native SHROOM.
    // Final Output: Both splits result in USDT.
    //  - 600 Native SHROOM @ 0.5 rate -> 300 USDT
    //  - 400 CW20 SHROOM  @ 0.4 rate -> 160 USDT
    //  - TOTAL: 460 USDT

    // Asset definitions
    let cw20_shroom_info = external::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let native_shroom_info = external::AssetInfo::NativeToken {
        denom: format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr),
    };
    let usdt_info = external::AssetInfo::NativeToken {
        denom: "usdt".to_string(),
    };

    // Stage 1: Get 1000 CW20 SHROOM
    let stage1 = Stage {
        splits: vec![Split {
            percent: 100,
            operation: Operation::AmmSwap(AmmSwapOp {
                pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                offer_asset_info: external::AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
                ask_asset_info: cw20_shroom_info.clone(),
            }),
        }],
    };

    // Stage 2: Requires mixed SHROOM, outputs unified USDT
    let stage2 = Stage {
        splits: vec![
            Split {
                // 60% requires Native SHROOM
                percent: 60,
                operation: Operation::OrderbookSwap(OrderbookSwapOp {
                    swap_contract: setup.mock_native_shroom_to_usdt_ob.clone(),
                    offer_asset_info: native_shroom_info.clone(),
                    ask_asset_info: usdt_info.clone(),
                }),
            },
            Split {
                // 40% requires CW20 SHROOM
                percent: 40,
                operation: Operation::AmmSwap(AmmSwapOp {
                    // --- USING THE NEW, DEDICATED POOL ---
                    pool_address: setup.mock_cw20_shroom_to_usdt_amm.clone(),
                    offer_asset_info: cw20_shroom_info.clone(),
                    ask_asset_info: usdt_info.clone(),
                }),
            },
        ],
    };

    let msg = ExecuteMsg::AggregateSwaps {
        minimum_receive: Some("459000000".to_string()), // Min 459 USDT (Target is 460)
        stages: vec![stage1, stage2],
    };

    let initial_usdt_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_usdt_amount = Uint128::from_str(&initial_usdt_balance.amount).unwrap();

    // Execute the transaction
    let res = wasm.execute(
        &setup.env.aggregator_addr,
        &msg,
        &[Coin::new(10_000_000_000_000_000_000u128, "inj")],
        user,
    );

    // Use the original, simple assert. The error message will now be informative.
    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());

    // --- ASSERT FINAL BALANCE ---
    let final_usdt_balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();

    // Expected Output: 300 USDT + 160 USDT = 460 USDT
    let total_swap_output = Uint128::new(460_000_000u128);
    let expected_final_usdt = initial_usdt_amount + total_swap_output;

    let final_usdt_amount =
        Uint128::from_str(&final_usdt_balance_response.balance.unwrap().amount).unwrap();

    assert_eq!(final_usdt_amount, expected_final_usdt);
}

#[test]
fn test_cw20_input_with_initial_reconciliation() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;
    let admin = &setup.env.admin;
    let bank = Bank::new(&setup.env.app);

    // --- SCENARIO ---
    // Input: User sends 1,000 CW20 SHROOM to the contract.
    // Stage 1: Requires a MIXED input (700 Native SHROOM, 300 CW20 SHROOM).
    // Reconciliation: Contract must convert 700 of the input CW20 SHROOM to Native SHROOM.
    // Final Output: Both splits result in USDT.
    //  - 700 Native SHROOM @ 0.5 rate -> 350 USDT
    //  - 300 CW20 SHROOM  @ 0.4 rate -> 120 USDT
    //  - TOTAL: 470 USDT

    // Mint the initial CW20 SHROOM to the user.
    let initial_user_shroom = Uint128::new(1_000_000_000); // 1,000 SHROOM (6 decimals)
    wasm.execute(
        &setup.shroom_cw20_addr,
        &cw20_base::msg::ExecuteMsg::Mint {
            recipient: user.address(),
            amount: initial_user_shroom,
        },
        &[],
        admin,
    )
    .unwrap();

    // Asset definitions for the stage
    let cw20_shroom_info = external::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let native_shroom_info = external::AssetInfo::NativeToken {
        denom: format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr),
    };
    let usdt_info = external::AssetInfo::NativeToken {
        denom: "usdt".to_string(),
    };

    let stage1 = Stage {
        splits: vec![
            Split {
                // 70% requires Native SHROOM
                percent: 70,
                operation: Operation::OrderbookSwap(OrderbookSwapOp {
                    swap_contract: setup.mock_native_shroom_to_usdt_ob.clone(),
                    offer_asset_info: native_shroom_info.clone(),
                    ask_asset_info: usdt_info.clone(),
                }),
            },
            Split {
                // 30% requires CW20 SHROOM
                percent: 30,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: setup.mock_cw20_shroom_to_usdt_amm.clone(),
                    offer_asset_info: cw20_shroom_info.clone(),
                    ask_asset_info: usdt_info.clone(),
                }),
            },
        ],
    };

    // The hook message sent with the CW20 token
    let hook_msg = Cw20HookMsg::AggregateSwaps {
        minimum_receive: Some("469000000".to_string()), // Min 469 USDT (Target is 470)
        stages: vec![stage1],
    };

    let initial_usdt_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_usdt_amount = Uint128::from_str(&initial_usdt_balance.amount).unwrap();

    // Execute the transaction via Cw20::Send
    let res = wasm.execute(
        &setup.shroom_cw20_addr,
        &cw20::Cw20ExecuteMsg::Send {
            contract: setup.env.aggregator_addr.clone(),
            amount: initial_user_shroom,
            msg: to_json_binary(&hook_msg).unwrap(),
        },
        &[],
        user,
    );
    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());

    // --- ASSERT FINAL BALANCE ---
    let final_usdt_balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();

    // Expected Output: 350 USDT (Native split) + 120 USDT (CW20 split) = 470 USDT
    let total_swap_output = Uint128::new(470_000_000u128);
    let expected_final_usdt = initial_usdt_amount + total_swap_output;
    let final_usdt_amount =
        Uint128::from_str(&final_usdt_balance_response.balance.unwrap().amount).unwrap();

    assert_eq!(final_usdt_amount, expected_final_usdt);
}

#[test]
fn test_complex_reconciliation_mixed_to_mixed() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;
    let bank = Bank::new(&setup.env.app);

    // --- SCENARIO ---
    // Stage 1: 10 INJ -> Mixed output of 600 Native SHROOM and 400 CW20 SHROOM.
    // Reconciliation: The contract now holds a mixed pile.
    // Stage 2: Requires a *different* mixed input: 250 Native SHROOM and 750 CW20 SHROOM.
    // Planner Logic:
    //  - Native: Have 600, Need 250 -> Surplus of 350.
    //  - CW20:   Have 400, Need 750 -> Deficit of 350.
    //  - Action: Must convert 350 Native SHROOM into CW20 SHROOM.
    // Final Output: Both splits result in USDT.
    //  - 250 Native SHROOM @ 0.5 rate -> 125 USDT
    //  - 750 CW20 SHROOM  @ 0.4 rate -> 300 USDT
    //  - TOTAL: 425 USDT

    // Asset definitions for clarity
    let cw20_shroom_info = external::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let native_shroom_info = external::AssetInfo::NativeToken {
        denom: format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr),
    };
    let usdt_info = external::AssetInfo::NativeToken {
        denom: "usdt".to_string(),
    };
    let inj_info = external::AssetInfo::NativeToken {
        denom: "inj".to_string(),
    };

    // Stage 1: 10 INJ -> Mixed SHROOM output
    let stage1 = Stage {
        splits: vec![
            Split {
                // 60% of INJ goes to create Native SHROOM
                percent: 60,
                operation: Operation::OrderbookSwap(OrderbookSwapOp {
                    swap_contract: setup.mock_inj_to_native_shroom_ob.clone(),
                    offer_asset_info: inj_info.clone(),
                    ask_asset_info: native_shroom_info.clone(),
                }),
            },
            Split {
                // 40% of INJ goes to create CW20 SHROOM
                percent: 40,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                    offer_asset_info: inj_info.clone(),
                    ask_asset_info: cw20_shroom_info.clone(),
                }),
            },
        ],
    };

    // Stage 2: Requires a different mix of SHROOM to output unified USDT
    let stage2 = Stage {
        splits: vec![
            Split {
                // 25% of total value requires Native SHROOM
                percent: 25,
                operation: Operation::OrderbookSwap(OrderbookSwapOp {
                    swap_contract: setup.mock_native_shroom_to_usdt_ob.clone(),
                    offer_asset_info: native_shroom_info.clone(),
                    ask_asset_info: usdt_info.clone(),
                }),
            },
            Split {
                // 75% of total value requires CW20 SHROOM
                percent: 75,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: setup.mock_cw20_shroom_to_usdt_amm.clone(),
                    offer_asset_info: cw20_shroom_info.clone(),
                    ask_asset_info: usdt_info.clone(),
                }),
            },
        ],
    };

    let msg = ExecuteMsg::AggregateSwaps {
        minimum_receive: Some("424000000".to_string()), // Min 424 USDT (Target is 425)
        stages: vec![stage1, stage2],
    };

    let initial_usdt_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_usdt_amount = Uint128::from_str(&initial_usdt_balance.amount).unwrap();

    // Execute the transaction
    let res = wasm.execute(
        &setup.env.aggregator_addr,
        &msg,
        &[Coin::new(10_000_000_000_000_000_000u128, "inj")], // User sends 10 INJ
        user,
    );
    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());

    // --- ASSERT FINAL BALANCE ---
    let final_usdt_balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();

    // Expected Output: 125 USDT + 300 USDT = 425 USDT
    let total_swap_output = Uint128::new(425_000_000u128);
    let expected_final_usdt = initial_usdt_amount + total_swap_output;

    let final_usdt_amount =
        Uint128::from_str(&final_usdt_balance_response.balance.unwrap().amount).unwrap();

    assert_eq!(final_usdt_amount, expected_final_usdt);
}

#[test]
fn test_final_output_is_cw20_token() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;

    // --- SCENARIO ---
    // A simple two-stage swap where the final output is a CW20 token (SAI).
    // This tests the contract's ability to transfer the final CW20 balance to the user.
    // Stage 1: 10 INJ -> 1000 CW20 SHROOM
    // Stage 2: 1000 CW20 SHROOM -> 100 CW20 SAI

    // Asset definitions for clarity
    let inj_info = external::AssetInfo::NativeToken {
        denom: "inj".to_string(),
    };
    let cw20_shroom_info = external::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let cw20_sai_info = external::AssetInfo::Token {
        contract_addr: setup.sai_cw20_addr.clone(),
    };

    let stage1 = Stage {
        splits: vec![Split {
            percent: 100,
            operation: Operation::AmmSwap(AmmSwapOp {
                pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                offer_asset_info: inj_info.clone(),
                ask_asset_info: cw20_shroom_info.clone(),
            }),
        }],
    };

    let stage2 = Stage {
        splits: vec![Split {
            percent: 100,
            operation: Operation::AmmSwap(AmmSwapOp {
                pool_address: setup.mock_cw20_shroom_to_cw20_sai_amm.clone(),
                offer_asset_info: cw20_shroom_info.clone(),
                ask_asset_info: cw20_sai_info.clone(),
            }),
        }],
    };

    let msg = ExecuteMsg::AggregateSwaps {
        minimum_receive: Some("99000000".to_string()), // Min 99 SAI (Target is 100)
        stages: vec![stage1, stage2],
    };

    // Check initial SAI balance is zero.
    let initial_sai_balance: BalanceResponse = wasm
        .query(
            &setup.sai_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: user.address(),
            },
        )
        .unwrap();
    assert_eq!(initial_sai_balance.balance, Uint128::zero());

    // Execute the transaction
    let res = wasm.execute(
        &setup.env.aggregator_addr,
        &msg,
        &[Coin::new(10_000_000_000_000_000_000u128, "inj")], // User sends 10 INJ
        user,
    );
    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());

    // --- ASSERT FINAL BALANCE ---
    // The user should now have the final CW20 SAI tokens.
    let final_sai_balance: BalanceResponse = wasm
        .query(
            &setup.sai_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: user.address(),
            },
        )
        .unwrap();

    // Expected Output: 100 SAI (6 decimals)
    let expected_final_sai = Uint128::new(100_000_000u128);
    assert_eq!(final_sai_balance.balance, expected_final_sai);
}

#[test]
fn test_native_input_with_initial_cw20_requirement() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;
    let admin = &setup.env.admin;
    let bank = Bank::new(&setup.env.app);

    // --- SCENARIO ---
    // User sends Native SHROOM, but the first stage requires CW20 SHROOM.
    // The contract must perform an initial conversion before the first swap.
    // 1. Input: 1000 Native SHROOM
    // 2. Reconciliation: Convert 1000 Native SHROOM -> 1000 CW20 SHROOM
    // 3. Stage 1: 1000 CW20 SHROOM -> 100 CW20 SAI

    // Asset definitions
    let native_shroom_denom = format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr);
    let cw20_shroom_info = external::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let cw20_sai_info = external::AssetInfo::Token {
        contract_addr: setup.sai_cw20_addr.clone(),
    };

    // First, we need to get some Native SHROOM to the user.
    // Admin mints CW20 -> sends to Adapter -> Adapter sends Native SHROOM to Admin -> Admin sends to User.
    let amount_to_test = Uint128::new(1_000_000_000); // 1,000 SHROOM
    wasm.execute(
        // Admin gets CW20
        &setup.shroom_cw20_addr,
        &cw20_base::msg::ExecuteMsg::Mint {
            recipient: admin.address(),
            amount: amount_to_test,
        },
        &[],
        &admin,
    )
    .unwrap();
    wasm.execute(
        // Admin converts to Native
        &setup.shroom_cw20_addr,
        &cw20::Cw20ExecuteMsg::Send {
            contract: setup.adapter_addr.clone(),
            amount: amount_to_test,
            msg: to_json_binary(&"{}").unwrap(),
        },
        &[],
        &admin,
    )
    .unwrap();
    bank.send(
        // Admin sends Native to User
        MsgSend {
            from_address: admin.address(),
            to_address: user.address(),
            amount: vec![ProtoCoin {
                denom: native_shroom_denom.clone(),
                amount: amount_to_test.to_string(),
            }],
        },
        &admin,
    )
    .unwrap();

    // Stage 1: Requires CW20 SHROOM
    let stage1 = Stage {
        splits: vec![Split {
            percent: 100,
            operation: Operation::AmmSwap(AmmSwapOp {
                pool_address: setup.mock_cw20_shroom_to_cw20_sai_amm.clone(),
                offer_asset_info: cw20_shroom_info.clone(),
                ask_asset_info: cw20_sai_info.clone(),
            }),
        }],
    };

    let msg = ExecuteMsg::AggregateSwaps {
        minimum_receive: Some("99000000".to_string()), // Min 99 SAI (Target is 100)
        stages: vec![stage1],
    };

    // Execute the transaction with native funds
    let res = wasm.execute(
        &setup.env.aggregator_addr,
        &msg,
        &[Coin {
            denom: native_shroom_denom,
            amount: amount_to_test,
        }],
        user,
    );
    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());

    // --- ASSERT FINAL BALANCE ---
    // The user should have received the final CW20 SAI tokens.
    let final_sai_balance: BalanceResponse = wasm
        .query(
            &setup.sai_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: user.address(),
            },
        )
        .unwrap();

    // Expected Output: 100 SAI (6 decimals)
    let expected_final_sai = Uint128::new(100_000_000u128);
    assert_eq!(final_sai_balance.balance, expected_final_sai);
}

#[test]
fn test_zero_amount_from_split_is_handled_gracefully() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let user = &env.user;
    let bank = Bank::new(&env.app);

    // --- SCENARIO ---
    // User sends a tiny amount (1 wei) that, when split, will result in at least one
    // of the splits having an amount of 0. The contract must not panic and should
    // proceed with only the non-zero splits.

    // Stage 1: Split 1 wei of INJ 50/50 across two pools.
    // - Split A (50%): 1 * 50 / 100 = 0. This split should be ignored or result in a no-op.
    // - Split B (50%, remainder): 1 - 0 = 1. This split should proceed.
    let stage1 = Stage {
        splits: vec![
            Split {
                percent: 50,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: env.mock_amm_1_addr.clone(),
                    ask_asset_info: external::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: external::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                }),
            },
            Split {
                percent: 50,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: env.mock_amm_2_addr.clone(),
                    ask_asset_info: external::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: external::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                }),
            },
        ],
    };

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![stage1],
        minimum_receive: None, // We don't care about the output amount, only that it doesn't fail.
    };

    let initial_usdt_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_usdt_amount = Uint128::from_str(&initial_usdt_balance.amount).unwrap();

    // Execute the transaction with 1 wei of INJ.
    let res = wasm.execute(&env.aggregator_addr, &msg, &[Coin::new(1u128, "inj")], user);
    assert!(
        res.is_ok(),
        "Execution with a zero-amount split failed: {:?}",
        res.unwrap_err()
    );

    // --- ASSERT FINAL BALANCE ---
    // Due to the mock pool's decimal conversion (18 for INJ, 6 for USDT), swapping
    // just 1 wei of INJ will result in 0 USDT. Therefore, the user's balance should not change.
    let final_usdt_balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();
    let final_usdt_amount =
        Uint128::from_str(&final_usdt_balance_response.balance.unwrap().amount).unwrap();

    assert_eq!(
        final_usdt_amount, initial_usdt_amount,
        "User's USDT balance should not change for a 1 wei swap"
    );
}

#[test]
fn test_stage_with_single_hundred_percent_split() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let user = &env.user;

    // --- SCENARIO ---
    // Stage 1: 100 INJ -> 1000 USDT (using a single 100% split)
    // Stage 2: 1000 USDT -> 100 INJ (using a single 100% split)

    let stage1 = Stage {
        splits: vec![Split {
            percent: 100,
            operation: Operation::AmmSwap(AmmSwapOp {
                pool_address: env.mock_amm_1_addr.clone(),
                ask_asset_info: external::AssetInfo::NativeToken {
                    denom: "usdt".to_string(),
                },
                offer_asset_info: external::AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
            }),
        }],
    };

    let stage2 = Stage {
        splits: vec![Split {
            percent: 100,
            operation: Operation::OrderbookSwap(OrderbookSwapOp {
                swap_contract: env.mock_ob_usdt_inj_addr.clone(),
                ask_asset_info: external::AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
                offer_asset_info: external::AssetInfo::NativeToken {
                    denom: "usdt".to_string(),
                },
            }),
        }],
    };

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![stage1, stage2],
        minimum_receive: Some("99000000000000000000".to_string()), // Min 99 INJ
    };

    let funds_to_send = Coin::new(100_000_000_000_000_000_000u128, "inj"); // 100 INJ

    // Execute the transaction
    let res = wasm.execute(&env.aggregator_addr, &msg, &[funds_to_send.clone()], user);
    assert!(
        res.is_ok(),
        "Execution with single-split stage failed: {:?}",
        res.unwrap_err()
    );

    let response = res.unwrap();
    let success_event = response
        .events
        .iter()
        .find(|e| {
            e.ty == "wasm"
                && e.attributes
                    .iter()
                    .any(|a| a.key == "action" && a.value == "aggregate_swap_complete")
        })
        .expect("Did not find final aggregate_swap_complete event");

    let final_received_attr = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    // The final swap (1000 USDT -> INJ @ rate 0.1) should yield exactly 100 INJ.
    let expected_final_amount = "100000000000000000000"; // 100 INJ with 18 decimals
    assert_eq!(final_received_attr.value, expected_final_amount);
}

#[test]
fn test_intermediate_swap_failure_reverts_transaction() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let user = &env.user;
    let bank = Bank::new(&env.app);

    // --- SCENARIO ---
    // We create a route where an intermediate step is guaranteed to fail by using
    // an invalid contract address. We then assert that the entire transaction
    // is reverted and the user's initial funds are returned.

    // Get the user's initial USDT balance to confirm the rollback.
    let initial_funds = Coin::new(1_000_000_000u128, "usdt"); // 1,000 USDT
    let initial_usdt_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_usdt_amount = Uint128::from_str(&initial_usdt_balance.amount).unwrap();

    // Stage 1: A valid swap from USDT to INJ. This part will succeed internally.
    let stage1 = Stage {
        splits: vec![Split {
            percent: 100,
            operation: Operation::OrderbookSwap(OrderbookSwapOp {
                swap_contract: env.mock_ob_usdt_inj_addr.clone(),
                ask_asset_info: external::AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
                offer_asset_info: external::AssetInfo::NativeToken {
                    denom: "usdt".to_string(),
                },
            }),
        }],
    };

    // Stage 2: The resulting INJ is split, but one split is sent to a bad address.
    let stage2 = Stage {
        splits: vec![
            Split {
                // This split is valid.
                percent: 50,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: env.mock_amm_1_addr.clone(),
                    ask_asset_info: external::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: external::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                }),
            },
            Split {
                // THIS SPLIT IS INTENTIONALLY INVALID.
                percent: 50,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: "inj1invalidcontractaddressxxxxxxxxxxxxxx".to_string(),
                    ask_asset_info: external::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: external::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                }),
            },
        ],
    };

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![stage1, stage2],
        minimum_receive: None, // Not relevant, as the transaction should fail.
    };

    // Execute the transaction
    let res = wasm.execute(&env.aggregator_addr, &msg, &[initial_funds.clone()], user);

    // --- ASSERT FAILURE AND ROLLBACK ---

    // 1. Assert that the transaction failed.
    assert!(
        res.is_err(),
        "Transaction should have failed due to an invalid contract address, but it succeeded"
    );

    // 2. Assert that the user's funds were returned.
    let final_usdt_balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();
    let final_usdt_amount =
        Uint128::from_str(&final_usdt_balance_response.balance.unwrap().amount).unwrap();

    assert_eq!(
        final_usdt_amount, initial_usdt_amount,
        "User's funds were not rolled back after a failed intermediate swap"
    );
}

#[test]
fn test_fee_collection_on_single_swap() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    let admin = &env.admin;
    let user = &env.user;
    let fee_collector = &env.fee_collector;

    // --- 1. SETUP: Admin sets a 0.3% fee on the first mock AMM pool ---
    let fee_pool_address = env.mock_amm_1_addr.clone();
    let fee_percent = Decimal::from_str("0.003").unwrap(); // 0.3%

    wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::SetFee {
            pool_address: fee_pool_address.clone(),
            fee_percent,
        },
        &[],
        admin,
    )
    .unwrap();

    // --- 2. EXECUTION: User performs a swap through the taxed pool ---
    // User sends 100 INJ. The pool rate is 10.0.
    // Gross Output: 100 INJ * 10.0 = 1,000 USDT.
    // Fee: 1,000 USDT * 0.3% = 3 USDT.
    // Net Output to User: 1000 - 3 = 997 USDT.

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![Stage {
            splits: vec![Split {
                percent: 100,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: fee_pool_address.clone(),
                    ask_asset_info: external::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: external::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                }),
            }],
        }],
        minimum_receive: Some("996000000".to_string()), // Min 996 USDT
    };

    let initial_collector_balance_res = bank
        .query_balance(&QueryBalanceRequest {
            address: fee_collector.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();

    let initial_collector_amount = initial_collector_balance_res
        .balance
        .map(|c| Uint128::from_str(&c.amount).unwrap()) // If Some(coin), parse its amount
        .unwrap_or_else(Uint128::zero); // If None, treat it as zero

    assert_eq!(
        initial_collector_amount,
        Uint128::zero(),
        "Fee collector should start with zero USDT"
    );

    // Execute the swap
    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        &[Coin::new(100_000_000_000_000_000_000u128, "inj")], // 100 INJ
        user,
    );
    assert!(
        res.is_ok(),
        "Swap execution with fee failed: {:?}",
        res.unwrap_err()
    );
    let response = res.unwrap();

    // --- 3. ASSERTIONS ---

    // Assertion A: Check the event logs for the user's net amount
    let success_event = response
        .events
        .iter()
        .find(|e| {
            e.ty == "wasm"
                && e.attributes
                    .iter()
                    .any(|a| a.key == "action" && a.value == "aggregate_swap_complete")
        })
        .expect("Did not find final aggregate_swap_complete event");

    let final_received_attr = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    let expected_net_output_to_user = Uint128::new(997_000_000u128); // 997 USDT (6 decimals)
    assert_eq!(
        final_received_attr.value,
        expected_net_output_to_user.to_string()
    );

    // Assertion B: Check the fee event attribute
    let fee_event = response
        .events
        .iter()
        .find(|e| e.ty == "wasm" && e.attributes.iter().any(|a| a.key == "fee_collected"))
        .expect("Did not find fee_collected event");

    let fee_collected_attr = fee_event
        .attributes
        .iter()
        .find(|a| a.key == "fee_collected")
        .unwrap();

    let expected_fee = Uint128::new(3_000_000u128); // 3 USDT (6 decimals)
    assert_eq!(fee_collected_attr.value, expected_fee.to_string());

    // Assertion C: Check the fee collector's final bank balance
    let final_collector_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: fee_collector.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();

    assert_eq!(final_collector_balance.amount, expected_fee.to_string());
    assert_eq!(final_collector_balance.denom, "usdt");
}

#[test]
fn test_fee_collection_on_cw20_output() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let admin = &setup.env.admin;
    let user = &setup.env.user;
    let fee_collector = &setup.env.fee_collector;

    // --- 1. SETUP: Admin sets a 1.5% fee on the INJ -> CW20 SHROOM pool ---
    let fee_pool_address = setup.mock_inj_to_cw20_shroom_amm.clone();
    let fee_percent = Decimal::from_str("0.015").unwrap(); // 1.5%

    wasm.execute(
        &setup.env.aggregator_addr,
        &ExecuteMsg::SetFee {
            pool_address: fee_pool_address.clone(),
            fee_percent,
        },
        &[],
        admin,
    )
    .unwrap();

    // --- 2. EXECUTION: User swaps 10 INJ, which should produce 1000 CW20 SHROOM ---
    // Gross Output: 1000 SHROOM
    // Fee: 1000 * 1.5% = 15 SHROOM
    // Net Output to User: 1000 - 15 = 985 SHROOM

    let stage1 = Stage {
        splits: vec![Split {
            percent: 100,
            operation: Operation::AmmSwap(AmmSwapOp {
                pool_address: fee_pool_address,
                ask_asset_info: external::AssetInfo::Token {
                    contract_addr: setup.shroom_cw20_addr.clone(),
                },
                offer_asset_info: external::AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
            }),
        }],
    };

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![stage1],
        minimum_receive: Some("984000000".to_string()), // Min 984 SHROOM
    };

    // Execute the transaction
    let res = wasm.execute(
        &setup.env.aggregator_addr,
        &msg,
        &[Coin::new(10_000_000_000_000_000_000u128, "inj")], // 10 INJ
        user,
    );
    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());

    // --- 3. ASSERTIONS ---

    // Assertion A: Check the user's final CW20 balance
    let user_balance: BalanceResponse = wasm
        .query(
            &setup.shroom_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: user.address(),
            },
        )
        .unwrap();
    let expected_net_output = Uint128::new(985_000_000); // 985 SHROOM (6 decimals)
    assert_eq!(user_balance.balance, expected_net_output);

    // Assertion B: Check the fee collector's final CW20 balance
    let collector_balance: BalanceResponse = wasm
        .query(
            &setup.shroom_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: fee_collector.address(),
            },
        )
        .unwrap();
    let expected_fee = Uint128::new(15_000_000); // 15 SHROOM (6 decimals)
    assert_eq!(collector_balance.balance, expected_fee);
}

#[test]
fn test_admin_functions_fail_for_unauthorized_user() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let unauthorized_user = &env.user; // Use the regular 'user' as the attacker

    // --- SetFee ---
    let res_set_fee = wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::SetFee {
            pool_address: env.mock_amm_1_addr.clone(),
            fee_percent: Decimal::from_str("0.01").unwrap(),
        },
        &[],
        unauthorized_user,
    );
    assert!(
        res_set_fee.is_err(),
        "SetFee should fail for unauthorized user"
    );
    assert!(res_set_fee
        .unwrap_err()
        .to_string()
        .contains("Unauthorized"));

    // --- RemoveFee ---
    let res_remove_fee = wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::RemoveFee {
            pool_address: env.mock_amm_1_addr.clone(),
        },
        &[],
        unauthorized_user,
    );
    assert!(
        res_remove_fee.is_err(),
        "RemoveFee should fail for unauthorized user"
    );
    assert!(res_remove_fee
        .unwrap_err()
        .to_string()
        .contains("Unauthorized"));

    // --- UpdateFeeCollector ---
    let res_update_collector = wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::UpdateFeeCollector {
            new_fee_collector: unauthorized_user.address(),
        },
        &[],
        unauthorized_user,
    );
    assert!(
        res_update_collector.is_err(),
        "UpdateFeeCollector should fail for unauthorized user"
    );
    assert!(res_update_collector
        .unwrap_err()
        .to_string()
        .contains("Unauthorized"));
}

#[test]
fn test_full_admin_fee_lifecycle() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);
    let admin = &env.admin;
    let user = &env.user;
    let original_collector = &env.fee_collector;

    let fee_pool_address = env.mock_amm_1_addr.clone();
    let fee_percent = Decimal::from_str("0.01").unwrap(); // 1%
    let expected_fee = Uint128::new(10_000_000); // 10 USDT fee

    // --- 1. Admin sets a fee ---
    wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::SetFee {
            pool_address: fee_pool_address.clone(),
            fee_percent,
        },
        &[],
        admin,
    )
    .unwrap();

    // --- 2. User swaps, fee goes to ORIGINAL collector ---
    let swap_msg = ExecuteMsg::AggregateSwaps {
        stages: vec![Stage {
            splits: vec![Split {
                percent: 100,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: fee_pool_address.clone(),
                    ask_asset_info: external::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: external::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                }),
            }],
        }],
        minimum_receive: None,
    };
    wasm.execute(
        &env.aggregator_addr,
        &swap_msg,
        &[Coin::new(100_000_000_000_000_000_000u128, "inj")],
        user,
    )
    .unwrap();

    // Assert fee was collected
    let collector1_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: original_collector.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    assert_eq!(collector1_balance.amount, expected_fee.to_string());

    // --- 3. Admin REMOVES the fee ---
    wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::RemoveFee {
            pool_address: fee_pool_address.clone(),
        },
        &[],
        admin,
    )
    .unwrap();

    // --- 4. User swaps again, NO fee is collected ---
    wasm.execute(
        &env.aggregator_addr,
        &swap_msg,
        &[Coin::new(100_000_000_000_000_000_000u128, "inj")],
        user,
    )
    .unwrap();

    // Assert balance of original collector has NOT changed
    let collector1_balance_after_remove = bank
        .query_balance(&QueryBalanceRequest {
            address: original_collector.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    assert_eq!(
        collector1_balance_after_remove.amount,
        expected_fee.to_string()
    );

    // --- 5. Admin sets fee again and UPDATES collector ---
    let new_collector = env.app.init_account(&[]).unwrap();
    wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::SetFee {
            pool_address: fee_pool_address.clone(),
            fee_percent,
        },
        &[],
        admin,
    )
    .unwrap();
    wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::UpdateFeeCollector {
            new_fee_collector: new_collector.address(),
        },
        &[],
        admin,
    )
    .unwrap();

    // --- 6. User swaps, fee goes to NEW collector ---
    wasm.execute(
        &env.aggregator_addr,
        &swap_msg,
        &[Coin::new(100_000_000_000_000_000_000u128, "inj")],
        user,
    )
    .unwrap();

    // Assert new collector received the fee
    let collector2_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: new_collector.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    assert_eq!(collector2_balance.amount, expected_fee.to_string());

    // Assert original collector's balance is still unchanged
    let collector1_final_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: original_collector.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    assert_eq!(collector1_final_balance.amount, expected_fee.to_string());
}

#[test]
fn test_multi_split_with_mixed_fees() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);
    let admin = &env.admin;
    let user = &env.user;
    let fee_collector = &env.fee_collector;

    // --- 1. SETUP: Admin sets a 1% fee on AMM1, but NO fee on AMM2 ---
    let taxed_pool = env.mock_amm_1_addr.clone();
    let untaxed_pool = env.mock_amm_2_addr.clone();
    let fee_percent = Decimal::from_str("0.01").unwrap(); // 1%

    wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::SetFee {
            pool_address: taxed_pool.clone(),
            fee_percent,
        },
        &[],
        admin,
    )
    .unwrap();

    // --- 2. EXECUTION: User swaps through a stage with splits to both pools ---
    let stage1 = Stage {
        splits: vec![
            Split {
                // This split goes to the TAXED pool
                percent: 40,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: taxed_pool,
                    ask_asset_info: external::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: external::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                }),
            },
            Split {
                // This split goes to the UNTAXED pool
                percent: 60,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: untaxed_pool,
                    ask_asset_info: external::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: external::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                }),
            },
        ],
    };

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![stage1],
        minimum_receive: Some("1595000000".to_string()), // Min 1595 USDT
    };

    // Execute the transaction
    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        &[Coin::new(100_000_000_000_000_000_000u128, "inj")], // 100 INJ
        user,
    );
    assert!(
        res.is_ok(),
        "Execution with mixed fees failed: {:?}",
        res.unwrap_err()
    );
    let response = res.unwrap();

    // --- 3. ASSERTIONS ---

    // Assertion A: Check the user's final received amount
    let success_event = response
        .events
        .iter()
        .find(|e| {
            e.ty == "wasm"
                && e.attributes
                    .iter()
                    .any(|a| a.key == "action" && a.value == "aggregate_swap_complete")
        })
        .expect("Did not find final aggregate_swap_complete event");

    let final_received_attr = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    let expected_net_output = Uint128::new(1596_000_000u128); // 396 + 1200 = 1596 USDT
    assert_eq!(final_received_attr.value, expected_net_output.to_string());

    // Assertion B: Check the fee collector's final balance
    let collector_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: fee_collector.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();

    let expected_fee = Uint128::new(4_000_000u128); // 4 USDT fee from the 400 USDT gross output
    assert_eq!(collector_balance.amount, expected_fee.to_string());
}

#[test]
fn test_fee_truncates_to_zero() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);
    let admin = &env.admin;
    let user = &env.user;
    let fee_collector = &env.fee_collector;

    // --- 1. SETUP: Admin sets a tiny fee on a pool ---
    let fee_pool_address = env.mock_amm_1_addr.clone();
    // This fee is 0.0001%, which is 0.000001 as a decimal.
    let tiny_fee_percent = Decimal::from_str("0.000001").unwrap();

    wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::SetFee {
            pool_address: fee_pool_address.clone(),
            fee_percent: tiny_fee_percent,
        },
        &[],
        admin,
    )
    .unwrap();

    // --- 2. EXECUTION: User swaps an amount that is small, but not dust. ---
    // We send 10^16 wei INJ.
    // Mock Pool Math: (10^16 * 10.0) * 10^6 / 10^18 = 10^17 * 10^-12 = 10^5 = 100,000 uusdt.
    // Gross Output: 100,000 uusdt (or 0.1 USDT).
    // Fee Calculation: 100,000 * 0.000001 = 0.1, which truncates to 0.
    let input_amount = Uint128::new(10_000_000_000_000_000u128); // 10^16

    let swap_msg = ExecuteMsg::AggregateSwaps {
        stages: vec![Stage {
            splits: vec![Split {
                percent: 100,
                operation: Operation::AmmSwap(AmmSwapOp {
                    pool_address: fee_pool_address,
                    ask_asset_info: external::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: external::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                }),
            }],
        }],
        minimum_receive: None,
    };

    // Execute the transaction
    let res = wasm.execute(
        &env.aggregator_addr,
        &swap_msg,
        &[Coin::new(input_amount.u128(), "inj")],
        user,
    );
    assert!(
        res.is_ok(),
        "Execution with zero-fee truncation failed: {:?}",
        res.unwrap_err()
    );
    let response = res.unwrap();

    // --- 3. ASSERTIONS ---

    // Assertion A: The user should receive the FULL gross amount.
    let success_event = response
        .events
        .iter()
        .find(|e| {
            e.ty == "wasm"
                && e.attributes
                    .iter()
                    .any(|a| a.key == "action" && a.value == "aggregate_swap_complete")
        })
        .expect("Did not find final aggregate_swap_complete event");

    let final_received_attr = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    let expected_gross_output = Uint128::new(100_000u128);
    assert_eq!(final_received_attr.value, expected_gross_output.to_string());

    // Assertion B: The fee collector's balance should be zero.
    let collector_balance_res = bank
        .query_balance(&QueryBalanceRequest {
            address: fee_collector.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();

    let collector_amount = collector_balance_res
        .balance
        .map(|c| Uint128::from_str(&c.amount).unwrap())
        .unwrap_or_else(Uint128::zero);

    assert_eq!(
        collector_amount,
        Uint128::zero(),
        "Fee collector should have a zero balance"
    );
}

#[test]
fn test_update_admin_success_and_failure() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let admin = &env.admin;
    let unauthorized_user = &env.user;

    // --- 1. Initial State Check ---
    // First, let's query the config to confirm the initial admin is correct.
    let initial_config: AggregatorConfig = wasm
        .query(&env.aggregator_addr, &QueryMsg::Config {})
        .unwrap();
    assert_eq!(initial_config.admin.to_string(), admin.address());

    // --- 2. SUCCESS PATH: The current admin changes the admin ---
    // Create a new, distinct account to be the new admin.
    let new_admin_account = env.app.init_account(&[]).unwrap();

    let res_success = wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::UpdateAdmin {
            new_admin: new_admin_account.address(),
        },
        &[],   // No funds needed
        admin, // Executed by the current admin
    );
    assert!(
        res_success.is_ok(),
        "Admin update should succeed when called by the current admin. Error: {:?}",
        res_success.unwrap_err()
    );

    // --- 3. Verify State Change ---
    // Query the config again to ensure the admin was actually updated in the state.
    let updated_config: AggregatorConfig = wasm
        .query(&env.aggregator_addr, &QueryMsg::Config {})
        .unwrap();
    assert_eq!(
        updated_config.admin.to_string(),
        new_admin_account.address()
    );
    assert_ne!(updated_config.admin.to_string(), admin.address()); // Also check it's not the old admin

    // --- 4. FAILURE PATH: An unauthorized user tries to change the admin ---
    let res_fail = wasm.execute(
        &env.aggregator_addr,
        &ExecuteMsg::UpdateAdmin {
            new_admin: unauthorized_user.address(), // The target doesn't matter, it should fail before this.
        },
        &[],
        unauthorized_user, // Executed by a random user
    );
    assert!(
        res_fail.is_err(),
        "Admin update should fail when called by a non-admin user"
    );

    // Check that the error message is the one we expect.
    let error = res_fail.unwrap_err();
    assert!(
        error.to_string().contains("Unauthorized"),
        "Error message was not the expected 'Unauthorized'. Got: {}",
        error
    );

    // --- 5. Verify No State Change After Failure ---
    // Query the config one last time to ensure the failed transaction did not change the admin.
    let final_config: AggregatorConfig = wasm
        .query(&env.aggregator_addr, &QueryMsg::Config {})
        .unwrap();
    assert_eq!(
        final_config.admin.to_string(),
        new_admin_account.address(),
        "Admin should not change after a failed update attempt"
    );
}
