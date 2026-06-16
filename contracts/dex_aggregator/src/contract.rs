use crate::cw20::Cw20ReceiveMsg;
use cosmwasm_std::{
    entry_point, Binary, Deps, DepsMut, Env, MessageInfo, Reply, Response, StdError,
    StdResult, Uint128,
};
use injective_cosmwasm::{InjectiveMsgWrapper, InjectiveQueryWrapper};

use crate::error::ContractError;
use crate::execute::{self, remove_fee, set_fee, update_fee_collector};
use crate::msg::{amm, Cw20HookMsg, ExecuteMsg, InstantiateMsg, MigrateMsg, QueryMsg};
use crate::state::{Config, CONFIG, REPLY_ID_COUNTER};

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
    let fee_collector_addr = deps.api.addr_validate(&msg.fee_collector_address)?;

    let config = Config {
        admin: admin_addr,
        cw20_adapter_address: adapter_addr,
        fee_collector: fee_collector_addr,
    };
    CONFIG.save(deps.storage, &config)?;
    REPLY_ID_COUNTER.save(deps.storage, &0u64)?;

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
            stages,
            minimum_receive,
        } => {
            // This is the entry point for NATIVE token swaps
            if info.funds.len() != 1 {
                return Err(ContractError::InvalidFunds {
                    sent: info.funds.len(),
                });
            }
            let offer_asset = amm::Asset {
                info: amm::AssetInfo::NativeToken {
                    denom: info.funds[0].denom.clone(),
                },
                // cosmwasm-std 3.0: Coin.amount is Uint256; our assets are Uint128.
                amount: Uint128::try_from(info.funds[0].amount).map_err(StdError::from)?,
            };
            execute::execute_aggregate_swaps_internal(
                deps,
                env,
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
            // A CW20 only reaches this entry point via `Cw20::Send`, which always
            // carries a hook `msg`; the sole legitimate hook is `ExecuteRoute`. Any
            // other / malformed payload (e.g. a route built for a different
            // aggregator version — a `clmm_swap` op against a CLMM-less build, or the
            // pre-merge `orderbook_swap` shape against the merged build) MUST revert
            // so the cw20 `send` transfer rolls back and the sender keeps their
            // tokens. Returning `Ok` here previously emitted a fake
            // "internal_conversion_complete" success and silently stranded the funds
            // in the contract (SHROOM incident, tx 3CA5FC2B...).
            let hook_msg: Cw20HookMsg = cosmwasm_std::from_json(&msg)
                .map_err(|e| ContractError::InvalidCw20Hook {
                    reason: e.to_string(),
                })?;
            match hook_msg {
                Cw20HookMsg::ExecuteRoute {
                    stages,
                    minimum_receive,
                } => {
                    // This is a user-initiated swap starting with a CW20 token.
                    let offer_asset = amm::Asset {
                        info: amm::AssetInfo::Token {
                            contract_addr: info.sender.to_string(),
                        },
                        amount,
                    };
                    let initiator = deps.api.addr_validate(&sender)?;
                    execute::execute_aggregate_swaps_internal(
                        deps,
                        env,
                        stages,
                        minimum_receive,
                        offer_asset,
                        initiator,
                    )
                }
            }
        }
        ExecuteMsg::UpdateAdmin { new_admin } => {
            crate::execute::update_admin(deps, info, new_admin)
        }
        ExecuteMsg::SetFee {
            pool_address,
            fee_fraction,
        } => set_fee(deps, info, pool_address, fee_fraction),
        ExecuteMsg::RemoveFee { pool_address } => remove_fee(deps, info, pool_address),
        ExecuteMsg::UpdateFeeCollector { new_fee_collector } => {
            update_fee_collector(deps, info, new_fee_collector)
        }
        ExecuteMsg::EmergencyWithdraw { asset_info } => {
            crate::execute::emergency_withdraw(deps, env, info, asset_info)
        }
        ExecuteMsg::RegisterTaxToken { contract_addr } => {
            crate::execute::register_tax_token(deps, info, contract_addr)
        }
        ExecuteMsg::DeregisterTaxToken { contract_addr } => {
            crate::execute::deregister_tax_token(deps, info, contract_addr)
        }
        ExecuteMsg::AuthorizeFlashSigner { signer } => {
            crate::execute::authorize_flash_signer(deps, info, signer)
        }
        ExecuteMsg::RevokeFlashSigner { signer } => {
            crate::execute::revoke_flash_signer(deps, info, signer)
        }
        ExecuteMsg::SetFlashUnrestricted { open } => {
            crate::execute::set_flash_unrestricted(deps, info, open)
        }
        ExecuteMsg::FlashRoute {
            flash_pool,
            flash_asset,
            flash_amount,
            stages,
            min_profit,
        } => execute::execute_flash_route(
            deps,
            env,
            info,
            flash_pool,
            flash_asset,
            flash_amount,
            stages,
            min_profit,
        ),
        ExecuteMsg::FlashCallback {
            fee0,
            fee1,
            data: _,
        } => execute::execute_flash_callback(deps, env, info, fee0, fee1),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps<InjectiveQueryWrapper>, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        // SimulateRoute walks Injective spot markets, so it needs the typed querier.
        QueryMsg::SimulateRoute { stages, amount_in } => {
            crate::query::simulate_route(deps, env, stages, amount_in)
        }
        // The rest only touch generic storage; drop the custom query type.
        QueryMsg::Config {} => crate::query::query_config(deps.into_empty()),
        QueryMsg::FeeForPool { pool_address } => {
            crate::query::query_fee_for_pool(deps.into_empty(), pool_address)
        }
        QueryMsg::AllFees { start_after, limit } => {
            crate::query::query_all_fees(deps.into_empty(), start_after, limit)
        }
        QueryMsg::IsFlashSigner { signer } => {
            crate::query::query_is_flash_signer(deps.into_empty(), signer)
        }
        QueryMsg::FlashSigners { start_after, limit } => {
            crate::query::query_flash_signers(deps.into_empty(), start_after, limit)
        }
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

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    _msg: MigrateMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    // Rejects migrating from a different contract name or a newer version, and
    // bumps the stored `cw2` version to CONTRACT_VERSION. No state migration is
    // needed (see `MigrateMsg`). Only the code admin can invoke this (chain-enforced).
    let prev_version =
        cw2::ensure_from_older_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    Ok(Response::new()
        .add_attribute("action", "migrate")
        .add_attribute("from_version", prev_version.to_string())
        .add_attribute("to_version", CONTRACT_VERSION))
}
