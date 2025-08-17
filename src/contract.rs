use cosmwasm_std::{
    entry_point, Binary, Deps, DepsMut, Env, MessageInfo, Reply, Response, StdResult,
};
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};

use crate::error::ContractError;
use crate::msg::{ExecuteMsg, InstantiateMsg, QueryMsg};
use crate::state::{Config, CONFIG};

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
    let config = Config { admin: admin_addr };
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
