use cosmwasm_std::{
    entry_point, Binary, Deps, DepsMut, Env, Event, MessageInfo, Reply, Response, StdResult,
};
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};

use crate::error::ContractError;
use crate::execute;
use crate::msg::{external, Cw20HookMsg, ExecuteMsg, InstantiateMsg, QueryMsg};
use crate::state::{Config, CONFIG};
use cw20::Cw20ReceiveMsg;

pub const CONTRACT_NAME: &str = "crates.io:dex-aggregator";
pub const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    cw2::set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let admin_addr = deps.api.addr_validate(&msg.admin)?;
    let adapter_addr = deps.api.addr_validate(&msg.cw20_adapter_address)?;

    // Save the full config
    let config = Config {
        admin: admin_addr,
        cw20_adapter_address: adapter_addr,
    };
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attribute("method", "instantiate"))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    match msg {
        ExecuteMsg::AggregateSwaps {
            stages,
            minimum_receive,
        } => {
            // This is the entry point for NATIVE token swaps
            if info.funds.len() != 1 {
                return Err(ContractError::InvalidFunds {});
            }
            let offer_asset = external::Asset {
                info: external::AssetInfo::NativeToken {
                    denom: info.funds[0].denom.clone(),
                },
                amount: info.funds[0].amount,
            };
            execute::execute_aggregate_swaps_internal(
                deps,
                env,
                info.clone(),
                stages,
                minimum_receive,
                offer_asset,
                info.sender,
            )
        }
        ExecuteMsg::Receive(Cw20ReceiveMsg {
            sender,
            amount,
            msg,
        }) => {
            if let Ok(hook_msg) = cosmwasm_std::from_json::<Cw20HookMsg>(&msg) {
                // This is a user-initiated swap starting with a CW20 token.
                match hook_msg {
                    Cw20HookMsg::AggregateSwaps {
                        stages,
                        minimum_receive,
                    } => {
                        let offer_asset = external::Asset {
                            info: external::AssetInfo::Token {
                                contract_addr: info.sender.to_string(),
                            },
                            amount,
                        };
                        let initiator = deps.api.addr_validate(&sender)?;
                        execute::execute_aggregate_swaps_internal(
                            deps,
                            env,
                            info,
                            stages,
                            minimum_receive,
                            offer_asset,
                            initiator,
                        )
                    }
                }
            } else {
                Ok(Response::new()
                    .add_event(
                        Event::new("wasm")
                            .add_attribute("action", "internal_conversion_complete")
                            .add_attribute("recipient", env.contract.address.to_string())
                            .add_attribute("amount", amount.to_string()),
                    )
                    .add_attribute("info", "cw20_received_for_normalization"))
            }
        }
        ExecuteMsg::ExecuteRoute {
            route,
            minimum_receive,
        } => crate::execute::execute_route(deps, env, info, route, minimum_receive),
        ExecuteMsg::UpdateAdmin { new_admin } => {
            crate::execute::update_admin(deps, info, new_admin)
        }
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::SimulateRoute { route, amount_in } => {
            crate::query::simulate_route(deps, env, route, amount_in)
        }
        QueryMsg::Config {} => crate::query::query_config(deps),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    msg: Reply,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    crate::reply::handle_reply(deps, env, msg)
}
