#![cfg(test)]

use cosmwasm_std::{Coin};
use injective_test_tube::{Account, InjectiveTestApp, Module, SigningAccount, Wasm};

// IMPORTANT: We now refer to the dex_aggregator crate by its package name
use dex_aggregator::msg::{
    ExecuteMsg, InstantiateMsg, Operation, Split, Stage, AmmSwapOp, OrderbookSwapOp, external
};
use mock_swap::InstantiateMsg as MockInstantiateMsg;


fn get_wasm_byte_code(filename: &str) -> &'static [u8] {
    match filename {
        "dex_aggregator.wasm" => include_bytes!("../artifacts/dex_aggregator.wasm"),
        "mock_swap.wasm" => include_bytes!("../artifacts/mock_swap.wasm"),
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
    pub mock_ob_addr: String,
}

/// Sets up the test environment, deploying the aggregator and three mock swap contracts.
fn setup() -> TestEnv {
    let app = InjectiveTestApp::new();
    let accounts = app
        .init_accounts(&[Coin::new(1_000_000_000_000_000_000_000u128, "inj")], 2) // 1000 INJ
        .unwrap();
    let mut accounts_iter = accounts.into_iter();
    let admin = accounts_iter.next().unwrap();
    let user = accounts_iter.next().unwrap();
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

    // Instantiate aggregator
    let aggregator_addr = wasm
        .instantiate(
            aggregator_code_id,
            &InstantiateMsg { admin: admin.address() },
            Some(&admin.address()),
            Some("dex-aggregator"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // Instantiate mock contracts
    let mock_amm_1_addr = wasm.instantiate(mock_swap_code_id, &MockInstantiateMsg {}, Some(&admin.address()), Some("mock-amm-1"), &[], &admin).unwrap().data.address;
    let mock_amm_2_addr = wasm.instantiate(mock_swap_code_id, &MockInstantiateMsg {}, Some(&admin.address()), Some("mock-amm-2"), &[], &admin).unwrap().data.address;
    let mock_ob_addr = wasm.instantiate(mock_swap_code_id, &MockInstantiateMsg {}, Some(&admin.address()), Some("mock-ob"), &[], &admin).unwrap().data.address;


    TestEnv {
        app,
        admin: admin,
        user: user,
        aggregator_addr,
        mock_amm_1_addr,
        mock_amm_2_addr,
        mock_ob_addr,
    }
}

#[test]
fn test_aggregate_swap_success() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    let msg = ExecuteMsg::AggregateSwaps {
        stages: vec![Stage {
            splits: vec![
                Split {
                    percent: 33,
                    operation: Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_1_addr.clone(),
                        ask_asset_info: external::AssetInfo::Token { contract_addr: "ignored".to_string() },
                        min_output: "5147352144459891590000000".to_string(), // 5.14e24
                    }),
                },
                Split {
                    percent: 42,
                    operation: Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_2_addr.clone(),
                        ask_asset_info: external::AssetInfo::Token { contract_addr: "ignored".to_string() },
                        min_output: "6558961275218033430000000".to_string(), // 6.55e24
                    }),
                },
                Split {
                    percent: 25,
                    operation: Operation::OrderbookSwap(OrderbookSwapOp {
                        swap_contract: env.mock_ob_addr.clone(),
                        ask_asset_info: external::AssetInfo::NativeToken { denom: "ignored".to_string() },
                        min_output: "3752098724165681000000000".to_string(), // 3.75e24
                    }),
                },
            ],
        }],
        // Total returned will be 5.2 + 6.6 + 3.75 = 15.55e24
        // Our minimum is 15.45e24, so this should pass.
        minimum_receive: Some("15458412143843606020000000".to_string()),
    };

    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        &[Coin::new(100_000_000_000_000_000_000u128, "inj")],
        &env.user,
    );

    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());
    
    let response = res.unwrap();

    // Check for the final success event from our reply handler
    let success_event = response.events.iter().find(|e| {
        e.ty == "wasm" && e.attributes.iter().any(|a| a.key == "action" && a.value == "aggregate_swap_reply_success")
    });

    assert!(success_event.is_some(), "Did not find success event in reply");

    let total_received_attr = success_event.unwrap().attributes.iter().find(|a| a.key == "total_received").unwrap();
    
    // 5.2e24 + 6.6e24 + 3.75e24 = 1.555e25
    assert_eq!(total_received_attr.value, "15552098724165681000000000");
}