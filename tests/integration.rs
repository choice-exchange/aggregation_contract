#![cfg(test)]

use std::slice;
use std::str::FromStr;

use cosmwasm_std::{to_json_binary, Addr, Coin, Decimal, Uint128};
use cw20::{BalanceResponse, Cw20QueryMsg};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
use dex_aggregator::msg::{
    amm, cw20_adapter, AmmSwapOp, ClmmSwapOp, Cw20HookMsg, ExecuteMsg, InstantiateMsg, Operation,
    OrderbookSwapOp, QueryMsg, Split, Stage,
};
use dex_aggregator::state::Config as AggregatorConfig;
use injective_cosmwasm::{get_default_subaccount_id_for_checked_address, MarketId};
use injective_math::FPDecimal;
use injective_test_tube::{
    injective_std::shim::Any,
    injective_std::types::{
        cosmos::{
            bank::v1beta1::{MsgSend, QueryBalanceRequest},
            base::v1beta1::Coin as ProtoCoin,
            gov::v1beta1::{MsgSubmitProposal, MsgVote, QueryProposalRequest},
        },
        injective::exchange::{
            v1beta1::{
                MsgCreateSpotLimitOrder, MsgInstantSpotMarketLaunch, OrderInfo,
                QuerySpotMarketsRequest, QuerySpotMidPriceAndTobRequest, SpotOrder as PbSpotOrder,
            },
            v2::{BatchExchangeModificationProposal, DenomMinNotional, DenomMinNotionalProposal},
        },
    },
    Account, Bank, Exchange, Gov, InjectiveTestApp, Module, SigningAccount, Wasm,
};
use mock_swap::{AssetInfo, InstantiateMsg as MockInstantiateMsg, ProtocolType, SwapConfig};
use prost::Message as _;

// ---------------------------------------------------------------------------
// Vendored CW20 test message types (replaces the cw20/cw20-base dev-deps, which
// were the last crates pulling cosmwasm-std 2 into the dev graph). These are
// local `mod cw20` / `mod cw20_base` so existing call sites compile unchanged;
// the JSON wire format matches cw20-base v2 (deployed by bytecode, version-agnostic).
// ---------------------------------------------------------------------------
mod cw20 {
    use cosmwasm_std::Uint128;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    pub struct Cw20Coin {
        pub address: String,
        pub amount: Uint128,
    }
    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    pub struct MinterResponse {
        pub minter: String,
        pub cap: Option<Uint128>,
    }
    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
    pub struct BalanceResponse {
        pub balance: Uint128,
    }
    #[derive(Serialize, Deserialize, Clone, Debug)]
    #[serde(rename_all = "snake_case")]
    pub enum Cw20QueryMsg {
        Balance { address: String },
    }
    #[derive(Serialize, Deserialize, Clone, Debug)]
    #[serde(rename_all = "snake_case")]
    pub enum Cw20ExecuteMsg {
        Send {
            contract: String,
            amount: Uint128,
            msg: cosmwasm_std::Binary,
        },
    }
}
mod cw20_base {
    pub mod msg {
        use crate::cw20::{Cw20Coin, MinterResponse};
        use cosmwasm_std::Uint128;
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, Clone, Debug)]
        pub struct InstantiateMsg {
            pub name: String,
            pub symbol: String,
            pub decimals: u8,
            pub initial_balances: Vec<Cw20Coin>,
            pub mint: Option<MinterResponse>,
            pub marketing: Option<serde_json::Value>,
        }
        #[derive(Serialize, Deserialize, Clone, Debug)]
        #[serde(rename_all = "snake_case")]
        pub enum ExecuteMsg {
            Mint { recipient: String, amount: Uint128 },
        }
    }
}

fn get_wasm_byte_code(filename: &str) -> &'static [u8] {
    match filename {
        "dex_aggregator.wasm" => include_bytes!("../artifacts/dex_aggregator.wasm"),
        "mock_swap.wasm" => include_bytes!("../artifacts/mock_swap.wasm"),
        "cw20_base.wasm" => include_bytes!("../cw20_base/cw20_base.wasm"),
        "cw20_adapter.wasm" => include_bytes!("../cw20_adapter/cw20_adapter.wasm"),
        _ => panic!("Unknown wasm file"),
    }
}

// ===========================================================================
// Live spot-market scaffolding (ported from orderbook_merge_proof/tests/proof.rs)
//
// Orderbook hops now place native atomic spot orders, which mock_swap cannot
// emulate — they need a real Injective spot market. These helpers launch and
// seed real markets on the test-tube chain.
// ===========================================================================

/// sdk.Dec proto string for an integer value: `value * 1e18`.
fn dec18(value: u128) -> String {
    format!("{value}{}", "0".repeat(18))
}
/// Bank micro-units: `human * 10^decimals`.
fn micro(human: u128, decimals: u32) -> u128 {
    human * 10u128.pow(decimals)
}
/// Proto price for an integer human price on a `base_dec`/`quote_dec` market:
/// chain price = human * 10^(quote_dec - base_dec); proto-encoded with *1e18.
fn price_proto(human: u128, base_dec: u32, quote_dec: u32) -> String {
    let exp = 18i32 + quote_dec as i32 - base_dec as i32;
    assert!(exp >= 0, "negative price exponent unsupported in tests");
    format!("{human}{}", "0".repeat(exp as usize))
}
/// Proto quantity for `human_base` base tokens: chain qty = human * 10^base_dec,
/// proto-encoded with *1e18.
fn qty_proto(human_base: u128, base_dec: u32) -> String {
    format!("{human_base}{}", "0".repeat((18 + base_dec) as usize))
}
/// Parse an sdk.Dec proto string into chain-scale `FPDecimal`.
fn dec_from_proto(s: &str) -> FPDecimal {
    FPDecimal::from_str(s).unwrap() / FPDecimal::from_str(&dec18(1)).unwrap()
}

/// Register denom min-notionals via governance (v1.19 prerequisite for spot
/// market launches). Decimals must already be registered (via
/// `init_account_decimals`). `funder` pays the validator's proposal deposit.
fn register_min_notionals(app: &InjectiveTestApp, funder: &SigningAccount, denoms: &[&str]) {
    let gov = Gov::new(app);
    let bank = Bank::new(app);
    let validator = app
        .get_first_validator_signing_account("inj".to_string(), 1.2f64)
        .unwrap();

    bank.send(
        MsgSend {
            from_address: funder.address(),
            to_address: validator.address(),
            amount: vec![ProtoCoin {
                amount: micro(1_000_000, 18).to_string(),
                denom: "inj".to_string(),
            }],
        },
        funder,
    )
    .unwrap();

    let min_notional = |d: &str| DenomMinNotional {
        denom: d.to_string(),
        min_notional: "1".to_string(),
    };
    let proposal = BatchExchangeModificationProposal {
        title: "register denom min notionals".to_string(),
        description: "integration test setup".to_string(),
        spot_market_param_update_proposals: vec![],
        derivative_market_param_update_proposals: vec![],
        spot_market_launch_proposals: vec![],
        perpetual_market_launch_proposals: vec![],
        expiry_futures_market_launch_proposals: vec![],
        trading_reward_campaign_update_proposal: None,
        binary_options_market_launch_proposals: vec![],
        binary_options_param_update_proposals: vec![],
        auction_exchange_transfer_denom_decimals_update_proposal: None,
        fee_discount_proposal: None,
        market_forced_settlement_proposals: vec![],
        denom_min_notional_proposal: Some(DenomMinNotionalProposal {
            title: "min notionals".to_string(),
            description: "integration test setup".to_string(),
            denom_min_notionals: denoms.iter().map(|d| min_notional(d)).collect(),
        }),
    };
    let mut buf = vec![];
    proposal.encode(&mut buf).unwrap();

    let res = gov
        .submit_proposal_v1beta1(
            MsgSubmitProposal {
                content: Some(Any {
                    type_url: "/injective.exchange.v2.BatchExchangeModificationProposal".to_string(),
                    value: buf,
                }),
                initial_deposit: vec![ProtoCoin {
                    amount: micro(100_000, 18).to_string(),
                    denom: "inj".to_string(),
                }],
                proposer: validator.address(),
            },
            &validator,
        )
        .unwrap();

    let proposal_id = res
        .events
        .iter()
        .find(|e| e.ty == "submit_proposal")
        .unwrap()
        .attributes[0]
        .value
        .clone();

    gov.vote_v1beta1(
        MsgVote {
            proposal_id: u64::from_str(&proposal_id).unwrap(),
            voter: validator.address(),
            option: 1i32,
        },
        &validator,
    )
    .unwrap();

    app.increase_time(20);
    let status = gov
        .query_proposal_v1beta1(&QueryProposalRequest {
            proposal_id: u64::from_str(&proposal_id).unwrap(),
        })
        .unwrap()
        .proposal
        .unwrap()
        .status;
    assert_eq!(status, 3, "min-notional proposal did not pass (status {status})");
    app.increase_time(200);
}

/// Launch a spot market and return its `market_id`.
#[allow(clippy::too_many_arguments)]
fn launch_spot_market(
    exchange: &Exchange<InjectiveTestApp>,
    signer: &SigningAccount,
    ticker: &str,
    base_denom: &str,
    quote_denom: &str,
    base_decimals: u32,
    quote_decimals: u32,
) -> String {
    exchange
        .instant_spot_market_launch(
            MsgInstantSpotMarketLaunch {
                sender: signer.address(),
                ticker: ticker.to_string(),
                base_denom: base_denom.to_string(),
                quote_denom: quote_denom.to_string(),
                // 0.000001 (quote/base) and 0.001 base, both proto-encoded.
                min_price_tick_size: price_proto(1, base_decimals, quote_decimals),
                min_quantity_tick_size: qty_proto(1, base_decimals)
                    .strip_suffix("000")
                    .unwrap()
                    .to_string(),
                min_notional: dec18(1),
                base_decimals,
                quote_decimals,
            },
            signer,
        )
        .unwrap();

    exchange
        .query_spot_markets(&QuerySpotMarketsRequest {
            status: "Active".to_string(),
            market_ids: vec![],
        })
        .unwrap()
        .markets
        .iter()
        .find(|m| m.ticker == ticker)
        .unwrap_or_else(|| panic!("market {ticker} not launched"))
        .market_id
        .clone()
}

/// Default subaccount id for an address (the bank-balance subaccount the
/// aggregator and makers trade from).
fn subaccount_of(addr: &str) -> String {
    get_default_subaccount_id_for_checked_address(&Addr::unchecked(addr)).to_string()
}

/// Place one resting spot limit order (order_type 1 = buy/bid, 2 = sell/ask).
fn limit_order(
    exchange: &Exchange<InjectiveTestApp>,
    trader: &SigningAccount,
    market_id: &str,
    order_type: i32,
    human_price: u128,
    human_qty_base: u128,
    base_decimals: u32,
    quote_decimals: u32,
) {
    exchange
        .create_spot_limit_order(
            MsgCreateSpotLimitOrder {
                sender: trader.address(),
                order: Some(PbSpotOrder {
                    market_id: market_id.to_string(),
                    order_info: Some(OrderInfo {
                        subaccount_id: subaccount_of(&trader.address()),
                        fee_recipient: trader.address(),
                        price: price_proto(human_price, base_decimals, quote_decimals),
                        quantity: qty_proto(human_qty_base, base_decimals),
                        cid: String::new(),
                    }),
                    order_type,
                    trigger_price: String::new(),
                }),
            },
            trader,
        )
        .unwrap();
}

/// (best_buy, best_sell) top-of-book prices in chain-scale `FPDecimal`.
fn tob(exchange: &Exchange<InjectiveTestApp>, market_id: &str) -> (FPDecimal, FPDecimal) {
    let r = exchange
        .query_spot_mid_price_and_tob(&QuerySpotMidPriceAndTobRequest {
            market_id: market_id.to_string(),
        })
        .unwrap();
    (
        dec_from_proto(&r.best_buy_price),
        dec_from_proto(&r.best_sell_price),
    )
}

pub struct TestEnv {
    pub app: InjectiveTestApp,
    pub admin: SigningAccount,
    pub user: SigningAccount,
    pub fee_collector: SigningAccount,
    pub aggregator_addr: String,
    pub mock_amm_1_addr: String,
    pub mock_amm_2_addr: String,
    /// Real INJ/USDT spot market (replaces the former mock orderbook contracts).
    pub market_inj_usdt: String,
    pub mock_clmm_inj_usdt_addr: String,
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

    // v1.19 spot-market launch prerequisite: register denom min-notionals via gov.
    register_min_notionals(&app, &admin, &["inj", "usdt"]);

    let wasm = Wasm::new(&app);

    // Store codes
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

    let _cw20_code_id = wasm
        .store_code(get_wasm_byte_code("cw20_base.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let cw20_adapter_code_id = wasm
        .store_code(get_wasm_byte_code("cw20_adapter.wasm"), None, &admin)
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

    // Real INJ/USDT spot market (base inj 18-dec, quote usdt 6-dec), seeded with a
    // book around ~10 USDT/INJ: asks 10/11/12 (buy INJ) and bids 9/8/7 (sell INJ),
    // 1000 INJ at each level — deep enough for any single test's hop.
    let exchange = Exchange::new(&app);
    let market_inj_usdt = launch_spot_market(&exchange, &admin, "INJ/USDT", "inj", "usdt", 18, 6);
    let maker = app
        .init_account(&[
            Coin::new(micro(10_000, 18), "inj"),
            Coin::new(micro(10_000_000, 6), "usdt"),
        ])
        .unwrap();
    for (px, qty) in [(10u128, 1000u128), (11, 1000), (12, 1000)] {
        limit_order(&exchange, &maker, &market_inj_usdt, 2, px, qty, 18, 6); // asks
    }
    for (px, qty) in [(9u128, 1000u128), (8, 1000), (7, 1000)] {
        limit_order(&exchange, &maker, &market_inj_usdt, 1, px, qty, 18, 6); // bids
    }
    app.increase_time(1);

    let mock_clmm_inj_usdt_addr = wasm
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
                    rate: "15.0".to_string(),
                    protocol_type: ProtocolType::Clmm,
                    input_decimals: 18,
                    output_decimals: 6,
                },
            },
            Some(&admin.address()),
            Some("mock-clmm-inj-usdt"),
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

    // Fund the mock AMM/CLMM contracts from the admin account (orderbook liquidity
    // now lives in the real market's book, not a funded mock contract).
    for addr in [&mock_amm_1_addr, &mock_amm_2_addr, &mock_clmm_inj_usdt_addr] {
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
        admin,
        user,
        fee_collector: fee_collector_account,
        aggregator_addr,
        mock_amm_1_addr,
        mock_amm_2_addr,
        market_inj_usdt,
        mock_clmm_inj_usdt_addr,
    }
}

#[test]
fn test_aggregate_swap_success() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    let bank = Bank::new(&env.app);

    // Input: 100 INJ
    // Split 1 (33%): 33 INJ -> AMM1 @ 10.0          = 330.000000 USDT
    // Split 2 (42%): 42 INJ -> AMM2 @ 20.0          = 840.000000 USDT
    // Split 3 (25%): 25 INJ -> live OB, best bid 9, less ~0.15% taker fee = 224.662500 USDT
    // Total Output: 330 + 840 + 224.6625 = 1394.6625 USDT

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![
                Split {
                    percent: 33,
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_1_addr.clone(),
                        ask_asset_info: amm::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: amm::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    })],
                },
                Split {
                    percent: 42,
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_2_addr.clone(),
                        ask_asset_info: amm::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: amm::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    })],
                },
                Split {
                    percent: 25,
                    path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(env.market_inj_usdt.clone()).unwrap(),
                        target_denom: "usdt".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
                },
            ],
        }],
        minimum_receive: Some(Uint128::new(1_390_000_000)), // Min 1390 USDT
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

    // Assert the total expected output is 1394.6625 USDT
    assert_eq!(total_received_attr.value, "1394662500");

    let balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: env.user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();

    // The user's final balance should be their initial balance + the swap output.
    // Initial: 1_000_000_000_000 (from setup)
    // Swap Output: 1_394_662_500 (1394.6625 USDT)
    // Expected Final: 1_001_394_662_500
    let expected_final_balance = Uint128::new(1_001_394_662_500u128);

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

    // Stage 1: 1,000 USDT -> live OB buy INJ @ best ask 10 (book depth 1000 INJ)
    // Stage 2: the resulting INJ split 49/51 across AMM1 (@10) and AMM2 (@20).
    // Input scaled to fit the seeded orderbook depth; exact output asserted below.

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![
            // Stage 1: 100% of USDT to the Orderbook to get INJ.
            Stage {
                splits: vec![Split {
                    percent: 100,
                    path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(env.market_inj_usdt.clone()).unwrap(),
                        target_denom: "inj".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
                }],
            },
            // Stage 2: The resulting INJ is split 49/51 across two AMMs to get final USDT.
            Stage {
                splits: vec![
                    Split {
                        percent: 49,
                        path: vec![Operation::AmmSwap(AmmSwapOp {
                            pool_address: env.mock_amm_1_addr.clone(),
                            ask_asset_info: amm::AssetInfo::NativeToken {
                                denom: "usdt".to_string(),
                            },
                            offer_asset_info: amm::AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                        })],
                    },
                    Split {
                        percent: 51,
                        path: vec![Operation::AmmSwap(AmmSwapOp {
                            pool_address: env.mock_amm_2_addr.clone(),
                            ask_asset_info: amm::AssetInfo::NativeToken {
                                denom: "usdt".to_string(),
                            },
                            offer_asset_info: amm::AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                        })],
                    },
                ],
            },
        ],
        // The minimum we expect from summing the Stage 2 outputs.
        minimum_receive: Some(Uint128::new(1)), // recalibrated after live-market run
    };

    // The initial funds for this route are 1,000 USDT
    let initial_funds = Coin::new(1_000_000_000u128, "usdt");

    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        slice::from_ref(&initial_funds),
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

    // 1000 USDT -> ~99.7506 INJ (ask 10, gross atomic fee) -> 49%@10 + 51%@20 = 1506.225 USDT
    let expected_final_amount = "1506225000";
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
        - Uint128::try_from(initial_funds.amount).unwrap()
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
    pub mock_inj_to_cw20_shroom_amm: String,
    pub mock_cw20_shroom_to_cw20_sai_amm: String,
    pub mock_cw20_shroom_to_usdt_amm: String,
    /// Live spot markets (replace the former mock orderbook contracts).
    pub market_inj_usdt: String,
    pub market_inj_shroom: String,
    pub market_shroom_usdt: String,
    /// The cw20-adapter tokenfactory denom for SHROOM (a market base/quote).
    pub native_shroom_denom: String,
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
        slice::from_ref(&total_fee),
        &admin,
    )
    .unwrap();
    wasm.execute(
        &adapter_addr,
        &cw20_adapter::ExecuteMsg::RegisterCw20Contract {
            addr: Addr::unchecked(sai_cw20_addr.clone()),
        },
        slice::from_ref(&total_fee),
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
                denom: native_shroom_denom.clone(),
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
            market_inj_usdt: "".to_string(),
            mock_clmm_inj_usdt_addr: "".to_string(),
        },
        shroom_cw20_addr,
        sai_cw20_addr,
        adapter_addr,
        mock_inj_to_cw20_shroom_amm,
        mock_cw20_shroom_to_cw20_sai_amm,
        mock_cw20_shroom_to_usdt_amm,
        // Live markets: placeholders until the factory-denom market launch (7d)
        // lands; the conversion tests that use them are #[ignore]d for now.
        market_inj_usdt: String::new(),
        market_inj_shroom: String::new(),
        market_shroom_usdt: String::new(),
        native_shroom_denom,
    }
}

#[test]
#[ignore = "pending live factory-denom (native_shroom) spot markets — 7d"]
fn test_full_normalization_route() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;

    // ROUTE: 10 INJ -> 50% to native SHROOM, 50% to cw20 SHROOM -> unified to cw20 SHROOM -> final swap to cw20 SAI
    // 10 INJ -> 1000 SHROOM total (500 native + 500 cw20)
    // 1000 SHROOM -> 100 SAI (rate of 0.1)

    let native_shroom_denom = format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr);

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![
            // Stage 1: INJ -> SHROOM (mixed native/cw20 output)
            Stage {
                splits: vec![
                    Split {
                        percent: 50,
                        path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(setup.market_inj_shroom.clone()).unwrap(),
                        target_denom: setup.native_shroom_denom.clone(),
                        quantity: None,
                        worst_price: None,
                    })],
                    },
                    Split {
                        percent: 50,
                        path: vec![Operation::AmmSwap(AmmSwapOp {
                            pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                            offer_asset_info: amm::AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                            ask_asset_info: amm::AssetInfo::Token {
                                contract_addr: setup.shroom_cw20_addr.clone(),
                            },
                        })],
                    },
                ],
            },
            // Stage 2: SHROOM (cw20) -> SAI (cw20)
            Stage {
                splits: vec![Split {
                    percent: 100,
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        pool_address: setup.mock_cw20_shroom_to_cw20_sai_amm.clone(),
                        offer_asset_info: amm::AssetInfo::Token {
                            contract_addr: setup.shroom_cw20_addr.clone(),
                        },
                        ask_asset_info: amm::AssetInfo::Token {
                            contract_addr: setup.sai_cw20_addr.clone(),
                        },
                    })],
                }],
            },
        ],
        minimum_receive: Some(Uint128::new(97000000)), // 97 SAI
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
#[ignore = "pending live factory-denom (native_shroom) spot markets — 7d"]
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

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![
            // Stage 1: 100% of USDT to the Orderbook to get INJ.
            Stage {
                splits: vec![Split {
                    percent: 100,
                    path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(setup.market_inj_usdt.clone()).unwrap(),
                        target_denom: "inj".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
                }],
            },
            // Stage 2: The resulting INJ is split 10/90 to get a mix of SHROOM types.
            Stage {
                splits: vec![
                    Split {
                        percent: 10, // 10% to CW20 SHROOM
                        path: vec![Operation::AmmSwap(AmmSwapOp {
                            pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                            offer_asset_info: amm::AssetInfo::NativeToken {
                                denom: "inj".to_string(),
                            },
                            ask_asset_info: amm::AssetInfo::Token {
                                contract_addr: setup.shroom_cw20_addr.clone(),
                            },
                        })],
                    },
                    Split {
                        percent: 90, // 90% to Native SHROOM
                        path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(setup.market_inj_shroom.clone()).unwrap(),
                        target_denom: setup.native_shroom_denom.clone(),
                        quantity: None,
                        worst_price: None,
                    })],
                    },
                ],
            },
        ],
        // The final expected output is unified CW20 SHROOM
        minimum_receive: Some(Uint128::new(9900000000)), // Min 9,900 CW20 SHROOM
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
    let hook_msg = Cw20HookMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![Split {
                percent: 100,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: setup.mock_cw20_shroom_to_cw20_sai_amm.clone(),
                    offer_asset_info: amm::AssetInfo::Token {
                        contract_addr: setup.shroom_cw20_addr.clone(),
                    },
                    ask_asset_info: amm::AssetInfo::Token {
                        contract_addr: setup.sai_cw20_addr.clone(),
                    },
                })],
            }],
        }],
        minimum_receive: Some(Uint128::new(99000000)), // Min 99 SAI
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
#[ignore = "pending live factory-denom (native_shroom) spot markets — 7d"]
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

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![
            // Stage 1: Get CW20 SHROOM
            Stage {
                splits: vec![Split {
                    percent: 100,
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                        offer_asset_info: amm::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: amm::AssetInfo::Token {
                            contract_addr: setup.shroom_cw20_addr.clone(),
                        },
                    })],
                }],
            },
            // Stage 2: Swap Native SHROOM for USDT
            Stage {
                splits: vec![Split {
                    percent: 100,
                    path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(setup.market_shroom_usdt.clone()).unwrap(),
                        target_denom: "usdt".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
                }],
            },
        ],
        minimum_receive: Some(Uint128::new(495000000)), // Min 495 USDT
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

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![
                Split {
                    percent: 33,
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_1_addr.clone(),
                        ask_asset_info: amm::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: amm::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    })],
                },
                Split {
                    percent: 42,
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_2_addr.clone(),
                        ask_asset_info: amm::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: amm::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    })],
                },
                Split {
                    percent: 25,
                    path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(env.market_inj_usdt.clone()).unwrap(),
                        target_denom: "usdt".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
                },
            ],
        }],

        minimum_receive: Some(Uint128::new(1920000001)),
    };

    let funds_to_send = Coin::new(100_000_000_000_000_000_000u128, "inj");
    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        slice::from_ref(&funds_to_send),
        user,
    );

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

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![
                Split {
                    percent: 50, // 50%
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_1_addr.clone(),
                        ask_asset_info: amm::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: amm::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    })],
                },
                Split {
                    percent: 49, // + 49% = 99% (Invalid!)
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_2_addr.clone(),
                        ask_asset_info: amm::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                        offer_asset_info: amm::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    })],
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
#[ignore = "pending live factory-denom (native_shroom) spot markets — 7d"]
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
    let cw20_shroom_info = amm::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let native_shroom_info = amm::AssetInfo::NativeToken {
        denom: format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr),
    };
    let usdt_info = amm::AssetInfo::NativeToken {
        denom: "usdt".to_string(),
    };

    // Stage 1: Get 1000 CW20 SHROOM
    let stage1 = Stage {
        splits: vec![Split {
            percent: 100,
            path: vec![Operation::AmmSwap(AmmSwapOp {
                pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                offer_asset_info: amm::AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
                ask_asset_info: cw20_shroom_info.clone(),
            })],
        }],
    };

    // Stage 2: Requires mixed SHROOM, outputs unified USDT
    let stage2 = Stage {
        splits: vec![
            Split {
                // 60% requires Native SHROOM
                percent: 60,
                path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(setup.market_shroom_usdt.clone()).unwrap(),
                        target_denom: "usdt".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
            },
            Split {
                // 40% requires CW20 SHROOM
                percent: 40,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: setup.mock_cw20_shroom_to_usdt_amm.clone(),
                    offer_asset_info: cw20_shroom_info.clone(),
                    ask_asset_info: usdt_info.clone(),
                })],
            },
        ],
    };

    let msg = ExecuteMsg::ExecuteRoute {
        minimum_receive: Some(Uint128::new(459000000)), // Min 459 USDT (Target is 460)
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
#[ignore = "pending live factory-denom (native_shroom) spot markets — 7d"]
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
    let cw20_shroom_info = amm::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let native_shroom_info = amm::AssetInfo::NativeToken {
        denom: format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr),
    };
    let usdt_info = amm::AssetInfo::NativeToken {
        denom: "usdt".to_string(),
    };

    let stage1 = Stage {
        splits: vec![
            Split {
                // 70% requires Native SHROOM
                percent: 70,
                path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(setup.market_shroom_usdt.clone()).unwrap(),
                        target_denom: "usdt".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
            },
            Split {
                // 30% requires CW20 SHROOM
                percent: 30,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: setup.mock_cw20_shroom_to_usdt_amm.clone(),
                    offer_asset_info: cw20_shroom_info.clone(),
                    ask_asset_info: usdt_info.clone(),
                })],
            },
        ],
    };

    // The hook message sent with the CW20 token
    let hook_msg = Cw20HookMsg::ExecuteRoute {
        minimum_receive: Some(Uint128::new(469000000)), // Min 469 USDT (Target is 470)
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
#[ignore = "pending live factory-denom (native_shroom) spot markets — 7d"]
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
    let cw20_shroom_info = amm::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let native_shroom_info = amm::AssetInfo::NativeToken {
        denom: format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr),
    };
    let usdt_info = amm::AssetInfo::NativeToken {
        denom: "usdt".to_string(),
    };
    let inj_info = amm::AssetInfo::NativeToken {
        denom: "inj".to_string(),
    };

    // Stage 1: 10 INJ -> Mixed SHROOM output
    let stage1 = Stage {
        splits: vec![
            Split {
                // 60% of INJ goes to create Native SHROOM
                percent: 60,
                path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(setup.market_inj_shroom.clone()).unwrap(),
                        target_denom: setup.native_shroom_denom.clone(),
                        quantity: None,
                        worst_price: None,
                    })],
            },
            Split {
                // 40% of INJ goes to create CW20 SHROOM
                percent: 40,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                    offer_asset_info: inj_info.clone(),
                    ask_asset_info: cw20_shroom_info.clone(),
                })],
            },
        ],
    };

    // Stage 2: Requires a different mix of SHROOM to output unified USDT
    let stage2 = Stage {
        splits: vec![
            Split {
                // 25% of total value requires Native SHROOM
                percent: 25,
                path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(setup.market_shroom_usdt.clone()).unwrap(),
                        target_denom: "usdt".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
            },
            Split {
                // 75% of total value requires CW20 SHROOM
                percent: 75,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: setup.mock_cw20_shroom_to_usdt_amm.clone(),
                    offer_asset_info: cw20_shroom_info.clone(),
                    ask_asset_info: usdt_info.clone(),
                })],
            },
        ],
    };

    let msg = ExecuteMsg::ExecuteRoute {
        minimum_receive: Some(Uint128::new(424000000)), // Min 424 USDT (Target is 425)
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
    let inj_info = amm::AssetInfo::NativeToken {
        denom: "inj".to_string(),
    };
    let cw20_shroom_info = amm::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let cw20_sai_info = amm::AssetInfo::Token {
        contract_addr: setup.sai_cw20_addr.clone(),
    };

    let stage1 = Stage {
        splits: vec![Split {
            percent: 100,
            path: vec![Operation::AmmSwap(AmmSwapOp {
                pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
                offer_asset_info: inj_info.clone(),
                ask_asset_info: cw20_shroom_info.clone(),
            })],
        }],
    };

    let stage2 = Stage {
        splits: vec![Split {
            percent: 100,
            path: vec![Operation::AmmSwap(AmmSwapOp {
                pool_address: setup.mock_cw20_shroom_to_cw20_sai_amm.clone(),
                offer_asset_info: cw20_shroom_info.clone(),
                ask_asset_info: cw20_sai_info.clone(),
            })],
        }],
    };

    let msg = ExecuteMsg::ExecuteRoute {
        minimum_receive: Some(Uint128::new(99000000)), // Min 99 SAI (Target is 100)
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
    let cw20_shroom_info = amm::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let cw20_sai_info = amm::AssetInfo::Token {
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
        admin,
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
        admin,
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
        admin,
    )
    .unwrap();

    // Stage 1: Requires CW20 SHROOM
    let stage1 = Stage {
        splits: vec![Split {
            percent: 100,
            path: vec![Operation::AmmSwap(AmmSwapOp {
                pool_address: setup.mock_cw20_shroom_to_cw20_sai_amm.clone(),
                offer_asset_info: cw20_shroom_info.clone(),
                ask_asset_info: cw20_sai_info.clone(),
            })],
        }],
    };

    let msg = ExecuteMsg::ExecuteRoute {
        minimum_receive: Some(Uint128::new(99000000)), // Min 99 SAI (Target is 100)
        stages: vec![stage1],
    };

    // Execute the transaction with native funds
    let res = wasm.execute(
        &setup.env.aggregator_addr,
        &msg,
        &[Coin {
            denom: native_shroom_denom,
            amount: amount_to_test.into(),
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
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: env.mock_amm_1_addr.clone(),
                    ask_asset_info: amm::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: amm::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                })],
            },
            Split {
                percent: 50,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: env.mock_amm_2_addr.clone(),
                    ask_asset_info: amm::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: amm::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                })],
            },
        ],
    };

    let msg = ExecuteMsg::ExecuteRoute {
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
            path: vec![Operation::AmmSwap(AmmSwapOp {
                pool_address: env.mock_amm_1_addr.clone(),
                ask_asset_info: amm::AssetInfo::NativeToken {
                    denom: "usdt".to_string(),
                },
                offer_asset_info: amm::AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
            })],
        }],
    };

    let stage2 = Stage {
        splits: vec![Split {
            percent: 100,
            path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(env.market_inj_usdt.clone()).unwrap(),
                        target_denom: "inj".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
        }],
    };

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![stage1, stage2],
        minimum_receive: Some(Uint128::new(99000000000000000000)), // Min 99 INJ
    };

    let funds_to_send = Coin::new(100_000_000_000_000_000_000u128, "inj"); // 100 INJ

    // Execute the transaction
    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        slice::from_ref(&funds_to_send),
        user,
    );
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
    // Buy INJ on the live book (best ask 10, gross atomic fee) = 99.75 INJ.
    let expected_final_amount = "99750000000000000000";
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
            path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(env.market_inj_usdt.clone()).unwrap(),
                        target_denom: "inj".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
        }],
    };

    // Stage 2: The resulting INJ is split, but one split is sent to a bad address.
    let stage2 = Stage {
        splits: vec![
            Split {
                // This split is valid.
                percent: 50,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: env.mock_amm_1_addr.clone(),
                    ask_asset_info: amm::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: amm::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                })],
            },
            Split {
                // THIS SPLIT IS INTENTIONALLY INVALID.
                percent: 50,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: "inj1invalidcontractaddressxxxxxxxxxxxxxx".to_string(),
                    ask_asset_info: amm::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: amm::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                })],
            },
        ],
    };

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![stage1, stage2],
        minimum_receive: None, // Not relevant, as the transaction should fail.
    };

    // Execute the transaction
    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        slice::from_ref(&initial_funds),
        user,
    );

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

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![Split {
                percent: 100,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: fee_pool_address.clone(),
                    ask_asset_info: amm::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: amm::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                })],
            }],
        }],
        minimum_receive: Some(Uint128::new(996000000)), // Min 996 USDT
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
            path: vec![Operation::AmmSwap(AmmSwapOp {
                pool_address: fee_pool_address,
                ask_asset_info: amm::AssetInfo::Token {
                    contract_addr: setup.shroom_cw20_addr.clone(),
                },
                offer_asset_info: amm::AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
            })],
        }],
    };

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![stage1],
        minimum_receive: Some(Uint128::new(984000000)), // Min 984 SHROOM
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
    let swap_msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![Split {
                percent: 100,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: fee_pool_address.clone(),
                    ask_asset_info: amm::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: amm::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                })],
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
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: taxed_pool,
                    ask_asset_info: amm::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: amm::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                })],
            },
            Split {
                // This split goes to the UNTAXED pool
                percent: 60,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: untaxed_pool,
                    ask_asset_info: amm::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: amm::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                })],
            },
        ],
    };

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![stage1],
        minimum_receive: Some(Uint128::new(1595000000)), // Min 1595 USDT
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

    let expected_net_output = Uint128::new(1_596_000_000_u128); // 396 + 1200 = 1596 USDT
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

    let swap_msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![Split {
                percent: 100,
                path: vec![Operation::AmmSwap(AmmSwapOp {
                    pool_address: fee_pool_address,
                    ask_asset_info: amm::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                    offer_asset_info: amm::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                })],
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

#[test]
#[ignore = "pending live factory-denom (native_shroom) spot markets — 7d"]
fn test_multi_hop_path_with_mid_path_conversion() {
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let user = &setup.env.user;
    let bank = Bank::new(&setup.env.app);

    // --- SCENARIO ---
    // This test validates the Awaiting::PathConversion state.
    // We create a single path where the output of Hop 1 (CW20 SHROOM) does not
    // match the required input of Hop 2 (Native SHROOM), forcing a conversion.

    // Asset definitions for clarity
    let inj_info = amm::AssetInfo::NativeToken {
        denom: "inj".to_string(),
    };
    let cw20_shroom_info = amm::AssetInfo::Token {
        contract_addr: setup.shroom_cw20_addr.clone(),
    };
    let native_shroom_info = amm::AssetInfo::NativeToken {
        denom: format!("factory/{}/{}", setup.adapter_addr, setup.shroom_cw20_addr),
    };
    let usdt_info = amm::AssetInfo::NativeToken {
        denom: "usdt".to_string(),
    };

    // The 3-hop path with a required conversion between hop 1 and 2
    let path = vec![
        // Hop 1: INJ -> CW20 SHROOM
        Operation::AmmSwap(AmmSwapOp {
            pool_address: setup.mock_inj_to_cw20_shroom_amm.clone(),
            offer_asset_info: inj_info.clone(),
            ask_asset_info: cw20_shroom_info.clone(),
        }),
        // Hop 2: Native SHROOM -> USDT (INPUT MISMATCH HERE)
        Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(setup.market_shroom_usdt.clone()).unwrap(),
                        target_denom: "usdt".to_string(),
                        quantity: None,
                        worst_price: None,
                    }),
        // Hop 3: USDT -> INJ
        Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(setup.market_inj_usdt.clone()).unwrap(),
                        target_denom: "inj".to_string(),
                        quantity: None,
                        worst_price: None,
                    }),
    ];

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![Split {
                percent: 100,
                path, // Use the complex path
            }],
        }],
        minimum_receive: Some(Uint128::new(49000000000000000000)), // Min 49 INJ
    };

    let funds_to_send = Coin::new(10_000_000_000_000_000_000u128, "inj"); // 10 INJ

    // Get the user's initial INJ balance to calculate the net change.
    let initial_inj_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "inj".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_inj_amount = Uint128::from_str(&initial_inj_balance.amount).unwrap();

    // Execute the transaction
    let res = wasm.execute(
        &setup.env.aggregator_addr,
        &msg,
        slice::from_ref(&funds_to_send),
        user,
    );

    assert!(
        res.is_ok(),
        "Execution with mid-path conversion failed: {:?}",
        res.unwrap_err()
    );
    println!("Gas Used: {}", res.unwrap().gas_info.gas_used);

    // --- ASSERT FINAL BALANCE ---
    let final_inj_balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: user.address(),
            denom: "inj".to_string(),
        })
        .unwrap();

    let final_inj_amount =
        Uint128::from_str(&final_inj_balance_response.balance.unwrap().amount).unwrap();

    // Expected change: -10 INJ (sent) + 50 INJ (received) = +40 INJ net gain.
    let expected_final_amount = initial_inj_amount
        .checked_sub(Uint128::try_from(funds_to_send.amount).unwrap())
        .unwrap()
        .checked_add(Uint128::new(50_000_000_000_000_000_000u128))
        .unwrap();

    // We must account for gas fees. The final amount will be slightly less than the expected amount.
    // A robust way to check is to ensure it's greater than the initial amount and close to the expected.
    assert!(final_inj_amount < expected_final_amount);
    assert!(
        final_inj_amount > initial_inj_amount,
        "Final balance should be greater than initial after a profitable swap"
    );
}

#[test]
fn test_emergency_withdraw() {
    // 1. --- SETUP ---
    let setup = setup_for_conversion_test();
    let wasm = Wasm::new(&setup.env.app);
    let bank = Bank::new(&setup.env.app);
    let admin = &setup.env.admin;
    let unauthorized_user = &setup.env.user;
    let aggregator_addr = &setup.env.aggregator_addr;
    let shroom_cw20_addr = &setup.shroom_cw20_addr;

    // 2. --- ARRANGE: Fund the aggregator contract with assets to withdraw ---
    let native_inj_to_send = Coin::new(100_000_000_000_000_000_000u128, "inj"); // 100 INJ
    let cw20_shroom_to_send = Uint128::new(500_000_000); // 500 SHROOM

    // Admin sends 100 INJ to the aggregator contract
    bank.send(
        MsgSend {
            from_address: admin.address(),
            to_address: aggregator_addr.clone(),
            amount: vec![ProtoCoin {
                denom: native_inj_to_send.denom.clone(),
                amount: native_inj_to_send.amount.to_string(),
            }],
        },
        admin,
    )
    .unwrap();

    // Admin mints and sends 500 SHROOM to the aggregator contract
    wasm.execute(
        shroom_cw20_addr,
        &cw20_base::msg::ExecuteMsg::Mint {
            recipient: aggregator_addr.clone(),
            amount: cw20_shroom_to_send,
        },
        &[],
        admin,
    )
    .unwrap();

    // 3. --- ACT & ASSERT ---

    // Test Case 1: Fails if called by an unauthorized user
    let msg_unauthorized = ExecuteMsg::EmergencyWithdraw {
        asset_info: amm::AssetInfo::NativeToken {
            denom: "inj".to_string(),
        },
    };
    let res_unauthorized = wasm.execute(aggregator_addr, &msg_unauthorized, &[], unauthorized_user);
    assert!(res_unauthorized.is_err());
    assert!(res_unauthorized
        .unwrap_err()
        .to_string()
        .contains("Unauthorized"));

    // Test Case 2: Admin successfully withdraws the native INJ
    let admin_inj_balance_before = bank
        .query_balance(&QueryBalanceRequest {
            address: admin.address(),
            denom: "inj".to_string(),
        })
        .unwrap()
        .balance
        .unwrap()
        .amount
        .parse::<u128>()
        .unwrap();

    let msg_inj = ExecuteMsg::EmergencyWithdraw {
        asset_info: amm::AssetInfo::NativeToken {
            denom: "inj".to_string(),
        },
    };
    wasm.execute(aggregator_addr, &msg_inj, &[], admin).unwrap();

    let admin_inj_balance_after = bank
        .query_balance(&QueryBalanceRequest {
            address: admin.address(),
            denom: "inj".to_string(),
        })
        .unwrap()
        .balance
        .unwrap()
        .amount
        .parse::<u128>()
        .unwrap();

    // Admin's balance should increase by (exactly 100 INJ - gas fees)
    // A simple check is to ensure it increased significantly.
    assert!(admin_inj_balance_after > admin_inj_balance_before);

    // Contract's INJ balance should now be zero
    let contract_inj_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: aggregator_addr.clone(),
            denom: "inj".to_string(),
        })
        .unwrap()
        .balance;
    assert!(contract_inj_balance.is_none() || contract_inj_balance.unwrap().amount == "0");

    // Test Case 3: Admin successfully withdraws the CW20 SHROOM
    let admin_shroom_balance_before: BalanceResponse = wasm
        .query(
            shroom_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: admin.address(),
            },
        )
        .unwrap();

    let msg_shroom = ExecuteMsg::EmergencyWithdraw {
        asset_info: amm::AssetInfo::Token {
            contract_addr: shroom_cw20_addr.clone(),
        },
    };
    wasm.execute(aggregator_addr, &msg_shroom, &[], admin)
        .unwrap();

    let admin_shroom_balance_after: BalanceResponse = wasm
        .query(
            shroom_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: admin.address(),
            },
        )
        .unwrap();

    assert_eq!(
        admin_shroom_balance_after.balance,
        admin_shroom_balance_before.balance + cw20_shroom_to_send
    );

    // Contract's SHROOM balance should now be zero
    let contract_shroom_balance: BalanceResponse = wasm
        .query(
            shroom_cw20_addr,
            &Cw20QueryMsg::Balance {
                address: aggregator_addr.clone(),
            },
        )
        .unwrap();
    assert_eq!(contract_shroom_balance.balance, Uint128::zero());
}

#[test]
fn test_multi_split_to_same_orderbook_contract() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // --- SCENARIO ---
    // This test ensures the aggregator can correctly handle a route where multiple
    // parallel operations (splits) are sent to the exact same contract address.
    //
    // ROUTE:
    // Input: 100 INJ
    // Split 1 (40%): 40 INJ -> Mock OB @ 30.0 = 1,200 USDT
    // Split 2 (60%): 60 INJ -> Mock OB @ 30.0 = 1,800 USDT
    // Total Expected Output: 3,000 USDT

    // Both splits route through the SAME live INJ/USDT market.
    let ob_split = |percent: u8| Split {
        percent,
        path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
            market_id: MarketId::new(env.market_inj_usdt.clone()).unwrap(),
            target_denom: "usdt".to_string(),
            quantity: None,
            worst_price: None,
        })],
    };

    // Define the message for the route execution.
    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![ob_split(40), ob_split(60)],
        }],
        minimum_receive: Some(Uint128::new(1)), // recalibrated after live-market run
    };

    // Get user's initial USDT balance for final assertion.
    let initial_usdt_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: env.user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_usdt_amount = Uint128::from_str(&initial_usdt_balance.amount).unwrap();

    // Execute the transaction with 100 INJ.
    let funds_to_send = Coin::new(100_000_000_000_000_000_000u128, "inj");
    let res = wasm.execute(&env.aggregator_addr, &msg, &[funds_to_send], &env.user);

    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());
    let response = res.unwrap();

    // --- ASSERTIONS ---

    // 1. Assert the total received amount from the event log.
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

    // 100 INJ sold into the live book (best bid 9, depth 1000) less ~0.15% taker
    // fee = 898.65 USDT, regardless of how the two splits divide it.
    let expected_total_output = "898650000";
    assert_eq!(total_received_attr.value, expected_total_output);

    // 2. Assert the user's final bank balance is correct.
    let final_balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: env.user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();
    let final_balance = final_balance_response.balance.unwrap();

    let expected_final_balance =
        initial_usdt_amount + Uint128::from_str(expected_total_output).unwrap();
    let final_amount = Uint128::from_str(&final_balance.amount).unwrap();

    assert_eq!(final_amount, expected_final_balance);
    assert_eq!(final_balance.denom, "usdt");
}

#[test]
fn test_multi_hop_consecutive_orderbook_swaps() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // --- SCENARIO ---
    // Hop 1: 100 INJ -> Mock OB 1 (rate 30.0) = 3,000 USDT
    // Hop 2: 3,000 USDT -> Mock OB 2 (rate 0.1) = 300 INJ
    // Final Expected Output: 300 INJ.

    let path = vec![
        Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(env.market_inj_usdt.clone()).unwrap(),
                        target_denom: "usdt".to_string(),
                        quantity: None,
                        worst_price: None,
                    }),
        Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(env.market_inj_usdt.clone()).unwrap(),
                        target_denom: "inj".to_string(),
                        quantity: None,
                        worst_price: None,
                    }),
    ];

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![Split { percent: 100, path }],
        }],
        minimum_receive: Some(Uint128::new(1)), // recalibrated after live-market run
    };

    let initial_inj_balance = bank
        .query_balance(&QueryBalanceRequest {
            address: env.user.address(),
            denom: "inj".to_string(),
        })
        .unwrap()
        .balance
        .unwrap();
    let initial_inj_amount = Uint128::from_str(&initial_inj_balance.amount).unwrap();

    let funds_to_send = Coin::new(100_000_000_000_000_000_000u128, "inj");
    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        slice::from_ref(&funds_to_send),
        &env.user,
    );
    assert!(res.is_ok(), "Execution failed: {:?}", res.unwrap_err());
    let response = res.unwrap(); // Keep the response to check events

    // --- ASSERTIONS ---

    // 1. Assert the event log for the correct, deterministic output amount.
    // This confirms the contract's logic is correct, regardless of gas fees.
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

    // 100 INJ -> sell @ bid 9 = 898.65 USDT -> buy @ ask 10 (gross fee) = 89.64 INJ.
    let expected_swap_output = Uint128::new(89_640_000_000_000_000_000u128);
    assert_eq!(final_received_attr.value, expected_swap_output.to_string());

    // 2. Assert the user's final bank balance, accounting for gas fees.
    let final_inj_balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: env.user.address(),
            denom: "inj".to_string(),
        })
        .unwrap();
    let final_inj_balance = final_inj_balance_response.balance.unwrap();
    let final_amount = Uint128::from_str(&final_inj_balance.amount).unwrap();

    // Calculate the "perfect world" final balance (without gas costs).
    let expected_final_amount_sans_gas = initial_inj_amount
        .checked_sub(Uint128::try_from(funds_to_send.amount).unwrap())
        .unwrap()
        .checked_add(expected_swap_output)
        .unwrap();

    // The actual final amount must be less than the perfect amount because of gas.
    assert!(
        final_amount < expected_final_amount_sans_gas,
        "Final amount should be less than the ideal amount due to gas fees"
    );

    // On a real market an INJ->USDT->INJ round trip through the same book is a net
    // LOSS (crosses the spread twice + pays two taker fees), so the user ends up
    // with less INJ than they started — confirming the two hops chained for real
    // (the old mock's 30.0/0.1 rates faked a profit; live books don't).
    assert!(
        final_amount < initial_inj_amount,
        "Round trip through one book should net a loss (spread + 2x taker fee)"
    );
}

#[test]
fn test_clmm_single_hop_swap() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // Input: 10 INJ (10 * 10^18)
    // CLMM pool rate: 15.0 -> 10 INJ = 150 USDT (150 * 10^6)
    // The aggregator queries Quote first, gets amount_out=150_000_000,
    // then applies 0.5% slippage for minimum_amount_out.

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![Split {
                percent: 100,
                path: vec![Operation::ClmmSwap(ClmmSwapOp {
                    pool_address: env.mock_clmm_inj_usdt_addr.clone(),
                    offer_asset_info: amm::AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    ask_asset_info: amm::AssetInfo::NativeToken {
                        denom: "usdt".to_string(),
                    },
                })],
            }],
        }],
        minimum_receive: Some(Uint128::new(149_000_000)), // 149 USDT
    };

    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        &[Coin::new(10_000_000_000_000_000_000u128, "inj")],
        &env.user,
    );

    assert!(res.is_ok(), "CLMM swap failed: {:?}", res.unwrap_err());

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
        .expect("Did not find success event");

    let total_received = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    // 10 INJ * 15.0 = 150 USDT = 150_000_000 (6 decimals)
    assert_eq!(total_received.value, "150000000");

    // Verify user's USDT balance increased
    let balance_response = bank
        .query_balance(&QueryBalanceRequest {
            address: env.user.address(),
            denom: "usdt".to_string(),
        })
        .unwrap();
    let final_balance = Uint128::from_str(&balance_response.balance.unwrap().amount).unwrap();
    // Initial: 1_000_000_000_000 + swap output: 150_000_000
    assert_eq!(final_balance, Uint128::new(1_000_150_000_000));
}

#[test]
fn test_clmm_mixed_with_amm_split() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    // Input: 100 INJ
    // Split 1 (50%): 50 INJ -> AMM1 @ 10.0 = 500 USDT
    // Split 2 (50%): 50 INJ -> CLMM @ 15.0 = 750 USDT
    // Total: 1250 USDT

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![Stage {
            splits: vec![
                Split {
                    percent: 50,
                    path: vec![Operation::AmmSwap(AmmSwapOp {
                        pool_address: env.mock_amm_1_addr.clone(),
                        offer_asset_info: amm::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: amm::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                    })],
                },
                Split {
                    percent: 50,
                    path: vec![Operation::ClmmSwap(ClmmSwapOp {
                        pool_address: env.mock_clmm_inj_usdt_addr.clone(),
                        offer_asset_info: amm::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: amm::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                    })],
                },
            ],
        }],
        minimum_receive: Some(Uint128::new(1_200_000_000)), // 1200 USDT
    };

    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        &[Coin::new(100_000_000_000_000_000_000u128, "inj")],
        &env.user,
    );

    assert!(
        res.is_ok(),
        "Mixed AMM+CLMM swap failed: {:?}",
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
        .expect("Did not find success event");

    let total_received = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    // 50 INJ * 10 = 500 USDT + 50 INJ * 15 = 750 USDT = 1250 USDT
    assert_eq!(total_received.value, "1250000000");
}

#[test]
fn test_clmm_multi_hop() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    // Multi-hop: USDT -> OB (rate 0.1) -> INJ -> CLMM (rate 15.0) -> USDT
    // Stage 1: 1000 USDT -> OB @ 0.1 = 100 INJ
    // Stage 2: 100 INJ -> CLMM @ 15.0 = 1500 USDT

    let msg = ExecuteMsg::ExecuteRoute {
        stages: vec![
            Stage {
                splits: vec![Split {
                    percent: 100,
                    path: vec![Operation::OrderbookSwap(OrderbookSwapOp {
                        market_id: MarketId::new(env.market_inj_usdt.clone()).unwrap(),
                        target_denom: "inj".to_string(),
                        quantity: None,
                        worst_price: None,
                    })],
                }],
            },
            Stage {
                splits: vec![Split {
                    percent: 100,
                    path: vec![Operation::ClmmSwap(ClmmSwapOp {
                        pool_address: env.mock_clmm_inj_usdt_addr.clone(),
                        offer_asset_info: amm::AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: amm::AssetInfo::NativeToken {
                            denom: "usdt".to_string(),
                        },
                    })],
                }],
            },
        ],
        minimum_receive: Some(Uint128::new(1_400_000_000)), // 1400 USDT
    };

    let res = wasm.execute(
        &env.aggregator_addr,
        &msg,
        &[Coin::new(1_000_000_000u128, "usdt")], // 1000 USDT
        &env.user,
    );

    assert!(
        res.is_ok(),
        "Multi-hop CLMM swap failed: {:?}",
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
        .expect("Did not find success event");

    let total_received = success_event
        .attributes
        .iter()
        .find(|a| a.key == "final_received")
        .unwrap();

    // 1000 USDT * 0.1 = 100 INJ, 100 INJ * 15 = 1500 USDT
    assert_eq!(total_received.value, "1496250000");
}
