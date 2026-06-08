//! Mock CLMM flash-loan pool for the dex_aggregator integration tests.
//!
//! Faithfully mirrors the flash side of `choice_clmm_pool` (see
//! `choice_exchange/contracts/choice_clmm_pool/src/actions/flash.rs`) at the JSON
//! wire level, so the aggregator's hand-rolled `ClmmPoolFlashMsg::Flash` and
//! `ExecuteMsg::FlashCallback` are exercised against the same message shapes the
//! real pool uses:
//!   - `Flash { recipient, amount0, amount1, data }` (variant tag `flash`)
//!   - callback `{"flash_callback":{"fee0","fee1","data"}}`
//!   - `GetConfig {}` → `{ token0, token1 }`
//!
//! It lends token0/token1, calls the borrower back via a `reply_on_success`
//! submessage (`REPLY_FLASH = 100`), verifies repayment by balance delta in the
//! reply, and holds a reentrancy lock for the duration. The real pool accrues a
//! dynamic ppm fee; this mock charges a flat `fee_bps` (the aggregator only reads
//! `fee0`/`fee1` from the callback, so the exact fee source is irrelevant to the
//! integration under test).

use cosmwasm_schema::cw_serde;
use cosmwasm_std::{
    entry_point, to_json_binary, Addr, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Env,
    MessageInfo, QuerierWrapper, Reply, Response, StdError, StdResult, SubMsg, Uint128, WasmMsg,
};
use cw_storage_plus::Item;

/// Reply id for the borrower callback (mirrors `choice_clmm_pool`'s `REPLY_FLASH`).
const REPLY_FLASH: u64 = 100;

/// Wire-compatible with `choice_clmm_common::types::AssetInfo`.
#[cw_serde]
pub enum AssetInfo {
    NativeToken { denom: String },
    Token { contract_addr: String },
}

impl AssetInfo {
    /// Direct transfer of `amount` to `to` — bank `Send` for native, CW20
    /// `Transfer` for tokens. Mirrors the real pool's `AssetInfo::transfer_msg`.
    fn transfer_msg(&self, to: &str, amount: Uint128) -> StdResult<CosmosMsg> {
        Ok(match self {
            AssetInfo::NativeToken { denom } => CosmosMsg::Bank(BankMsg::Send {
                to_address: to.to_string(),
                amount: vec![Coin {
                    denom: denom.clone(),
                    amount: amount.into(),
                }],
            }),
            AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: to.to_string(),
                    amount,
                })?,
                funds: vec![],
            }),
        })
    }
}

#[cw_serde]
enum Cw20ExecuteMsg {
    Transfer { recipient: String, amount: Uint128 },
}

#[cw_serde]
enum Cw20QueryMsg {
    Balance { address: String },
}

#[cw_serde]
struct Cw20BalanceResponse {
    balance: Uint128,
}

#[cw_serde]
pub struct InstantiateMsg {
    pub token0: AssetInfo,
    pub token1: AssetInfo,
    /// Flat flash fee in basis points (e.g. 30 = 0.30%).
    pub fee_bps: u16,
}

/// Wire-compatible with `choice_clmm_common::pool::ExecuteMsg::Flash`.
#[cw_serde]
pub enum ExecuteMsg {
    Flash {
        recipient: String,
        amount0: Uint128,
        amount1: Uint128,
        data: Binary,
    },
}

/// Wire-compatible with `choice_clmm_common::pool::FlashCallbackMsg`.
#[cw_serde]
pub enum FlashCallbackMsg {
    FlashCallback {
        fee0: Uint128,
        fee1: Uint128,
        data: Binary,
    },
}

#[cw_serde]
pub enum QueryMsg {
    /// Mirrors the CLMM pool's `GetConfig {}` (the aggregator queries this to map
    /// the borrowed asset onto token0/token1).
    GetConfig {},
}

#[cw_serde]
pub struct ConfigResponse {
    pub token0: AssetInfo,
    pub token1: AssetInfo,
}

#[cw_serde]
struct Config {
    token0: AssetInfo,
    token1: AssetInfo,
    fee_bps: u16,
}

#[cw_serde]
struct PendingFlash {
    amount0: Uint128,
    amount1: Uint128,
    fee0: Uint128,
    fee1: Uint128,
    snapshot0: Uint128,
    snapshot1: Uint128,
}

const CONFIG: Item<Config> = Item::new("config");
const PENDING: Item<PendingFlash> = Item::new("pending_flash");
const LOCK: Item<bool> = Item::new("reentrancy_lock");

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    CONFIG.save(
        deps.storage,
        &Config {
            token0: msg.token0,
            token1: msg.token1,
            fee_bps: msg.fee_bps,
        },
    )?;
    Ok(Response::new())
}

fn pool_balance(q: &QuerierWrapper, pool: &Addr, asset: &AssetInfo) -> StdResult<Uint128> {
    Ok(match asset {
        AssetInfo::NativeToken { denom } => {
            Uint128::try_from(q.query_balance(pool, denom)?.amount).map_err(StdError::from)?
        }
        AssetInfo::Token { contract_addr } => {
            let r: Cw20BalanceResponse = q.query_wasm_smart(
                contract_addr,
                &Cw20QueryMsg::Balance {
                    address: pool.to_string(),
                },
            )?;
            r.balance
        }
    })
}

/// Flash fee = ceil(amount * fee_bps / 10_000), rounded up in the pool's favor.
fn flash_fee(amount: Uint128, fee_bps: u16) -> Uint128 {
    if amount.is_zero() {
        return Uint128::zero();
    }
    Uint128::new((amount.u128() * fee_bps as u128).div_ceil(10_000))
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response> {
    match msg {
        ExecuteMsg::Flash {
            recipient,
            amount0,
            amount1,
            data,
        } => {
            if LOCK.may_load(deps.storage)?.unwrap_or(false) {
                return Err(StdError::msg("reentrancy: flash already in progress"));
            }
            if amount0.is_zero() && amount1.is_zero() {
                return Err(StdError::msg("flash: zero amount"));
            }
            let cfg = CONFIG.load(deps.storage)?;
            let recipient = deps.api.addr_validate(&recipient)?;
            let fee0 = flash_fee(amount0, cfg.fee_bps);
            let fee1 = flash_fee(amount1, cfg.fee_bps);

            let pool = &env.contract.address;
            let snapshot0 = pool_balance(&deps.querier, pool, &cfg.token0)?;
            let snapshot1 = pool_balance(&deps.querier, pool, &cfg.token1)?;
            if amount0 > snapshot0 || amount1 > snapshot1 {
                return Err(StdError::msg("flash: amount exceeds pool balance"));
            }

            LOCK.save(deps.storage, &true)?;
            PENDING.save(
                deps.storage,
                &PendingFlash {
                    amount0,
                    amount1,
                    fee0,
                    fee1,
                    snapshot0,
                    snapshot1,
                },
            )?;

            let mut msgs: Vec<CosmosMsg> = vec![];
            if !amount0.is_zero() {
                msgs.push(cfg.token0.transfer_msg(recipient.as_str(), amount0)?);
            }
            if !amount1.is_zero() {
                msgs.push(cfg.token1.transfer_msg(recipient.as_str(), amount1)?);
            }

            let callback = SubMsg::reply_on_success(
                WasmMsg::Execute {
                    contract_addr: recipient.to_string(),
                    msg: to_json_binary(&FlashCallbackMsg::FlashCallback { fee0, fee1, data })?,
                    funds: vec![],
                },
                REPLY_FLASH,
            );

            Ok(Response::new()
                .add_messages(msgs)
                .add_submessage(callback)
                .add_attribute("action", "flash")
                .add_attribute("fee0", fee0)
                .add_attribute("fee1", fee1))
        }
    }
}

#[entry_point]
pub fn reply(deps: DepsMut, env: Env, msg: Reply) -> StdResult<Response> {
    if msg.id != REPLY_FLASH {
        return Err(StdError::msg(format!("unknown reply id {}", msg.id)));
    }
    let p = PENDING.load(deps.storage)?;
    PENDING.remove(deps.storage);
    let cfg = CONFIG.load(deps.storage)?;
    let pool = &env.contract.address;

    let required0 = p.snapshot0 + p.fee0;
    let required1 = p.snapshot1 + p.fee1;
    if !p.amount0.is_zero() || !p.fee0.is_zero() {
        let bal = pool_balance(&deps.querier, pool, &cfg.token0)?;
        if bal < required0 {
            return Err(StdError::msg(format!(
                "flash not repaid (token0): required {required0}, have {bal}"
            )));
        }
    }
    if !p.amount1.is_zero() || !p.fee1.is_zero() {
        let bal = pool_balance(&deps.querier, pool, &cfg.token1)?;
        if bal < required1 {
            return Err(StdError::msg(format!(
                "flash not repaid (token1): required {required1}, have {bal}"
            )));
        }
    }

    LOCK.save(deps.storage, &false)?;
    Ok(Response::new()
        .add_attribute("action", "flash_repaid")
        .add_attribute("fee0", p.fee0)
        .add_attribute("fee1", p.fee1))
}

#[entry_point]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::GetConfig {} => {
            let c = CONFIG.load(deps.storage)?;
            to_json_binary(&ConfigResponse {
                token0: c.token0,
                token1: c.token1,
            })
        }
    }
}
