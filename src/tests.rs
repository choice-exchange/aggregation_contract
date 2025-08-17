#![cfg(test)]

mod tests {
    // Removed the unused `Addr` import
    use cosmwasm_std::Coin;
    use injective_test_tube::{Account, InjectiveTestApp, Module, Wasm};

    use crate::error::ContractError;
    use crate::msg::{ExecuteMsg, InstantiateMsg, QueryMsg};
    use crate::state::Config;

    /// Helper function to load the wasm bytecode from the file system.
    fn get_wasm_byte_code() -> &'static [u8] {
        include_bytes!("../artifacts/dex_aggregator.wasm")
    }

    #[test]
    fn proper_instantiation() {
        // Create the appchain instance
        let app = InjectiveTestApp::new();

        // Initialize accounts
        let accounts = app
            // Added the `u128` suffix to the number
            .init_accounts(&[Coin::new(1_000_000_000_000_000_000u128, "inj")], 2) // 1 INJ
            .unwrap();
        
        let admin = &accounts[0];

        // Get the Wasm module
        let wasm = Wasm::new(&app);

        // Store the contract code
        let code_id = wasm
            .store_code(
                get_wasm_byte_code(),
                None,
                admin,
            )
            .unwrap()
            .data
            .code_id;

        // Instantiate the contract
        let contract_addr = wasm
            .instantiate(
                code_id,
                &InstantiateMsg {
                    admin: admin.address(),
                },
                Some(&admin.address()),
                Some("dex-aggregator"),
                &[], // funds
                admin, // signer
            )
            .unwrap()
            .data
            .address;

        // Query the config to verify the admin
        let config: Config = wasm
            .query(
                &contract_addr.to_string(),
                &QueryMsg::Config {},
            )
            .unwrap();

        assert_eq!(config.admin.to_string(), admin.address());
    }

    #[test]
    fn test_unauthorized_update_admin() {
        // Create the appchain instance
        let app = InjectiveTestApp::new();

        // Initialize accounts
        let accounts = app
            // Added the `u128` suffix to the number
            .init_accounts(&[Coin::new(1_000_000_000_000_000_000u128, "inj")], 2)
            .unwrap();
        
        let admin = &accounts[0];
        let user = &accounts[1];

        // Get the Wasm module
        let wasm = Wasm::new(&app);

        // Store and instantiate the contract
        let code_id = wasm.store_code(get_wasm_byte_code(), None, admin).unwrap().data.code_id;
        let contract_addr = wasm
            .instantiate(code_id, &InstantiateMsg { admin: admin.address() }, Some(&admin.address()), Some("dex-aggregator"), &[], admin)
            .unwrap()
            .data
            .address;

        let msg = ExecuteMsg::UpdateAdmin {
            new_admin: user.address(),
        };
        
        let err = wasm
            .execute(&contract_addr.to_string(), &msg, &[], user)
            .unwrap_err();

        assert!(
            err.to_string().contains(&ContractError::Unauthorized {}.to_string()),
            "Error did not contain the expected 'Unauthorized' message"
        );
    }
}