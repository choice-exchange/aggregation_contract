//! Native Injective spot-orderbook execution, ported from the standalone
//! `inj-orderbook-swap-contract` (`queries.rs` / `helpers.rs` / `types.rs`).
//!
//! Scope (see docs/orderbook_merge_plan.md): only the **single-market, from-source
//! input-quantity** estimation path is kept. The standalone contract's multi-market
//! routes (`SwapRoute`/`steps_from`), exact-output / `*_from_target` estimators,
//! `SwapQuantityMode`, and cross-step `SWAP_*` state are all dropped — each merged
//! `OrderbookSwapOp` is exactly one market = one order = one reply.
//!
//! Adaptations from the original:
//! - The aggregator is always its own fee recipient (self-relayer), so the fee
//!   discount is always applied — no `Config`/`fee_recipient` lookup.
//! - cosmwasm-std 3.0: `generic_err` -> `msg`; `Coin.amount` is `Uint256`
//!   (`FPDecimal: From<Uint256>` exists, so balance reads still `.into()` cleanly).

use cosmwasm_std::{Addr, CosmosMsg, Deps, StdError, StdResult, SubMsgResponse, Uint128};
use injective_cosmwasm::{
    create_spot_market_order_msg, get_default_subaccount_id_for_checked_address,
    InjectiveMsgWrapper, InjectiveQuerier, InjectiveQueryWrapper, MarketId, OrderSide, OrderType,
    PriceLevel, SpotMarket, SpotOrder,
};
use injective_math::utils::round_to_min_tick;
use injective_math::FPDecimal;
use injective_std::types::injective::exchange::v1beta1::MsgCreateSpotMarketOrderResponse;
use prost::Message as _;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// An `FPDecimal`-denominated coin. Orderbook quantities/prices are `FPDecimal`,
/// distinct from the `Uint128` used everywhere else in the aggregator.
#[derive(Clone, Debug, PartialEq)]
pub struct FPCoin {
    pub amount: FPDecimal,
    pub denom: String,
}

/// Result of estimating (or sizing, for execution) a single-market orderbook hop.
#[derive(Clone, Debug, PartialEq)]
pub struct StepExecutionEstimate {
    /// Worst acceptable price across the consumed levels — used as the atomic
    /// order's price bound. Conservative; the route-level `minimum_receive` is the
    /// real net.
    pub worst_price: FPDecimal,
    /// Denom produced by this hop.
    pub result_denom: String,
    /// Expected output quantity (base for a buy, quote-minus-fee for a sell).
    pub result_quantity: FPDecimal,
    pub is_buy_order: bool,
    /// Trading fee taken by the exchange, always in the quote denom.
    pub fee_estimate: Option<FPCoin>,
}

// ---------------------------------------------------------------------------
// Helpers (ported from helpers.rs)
// ---------------------------------------------------------------------------

/// Scale an `FPDecimal` by `10^digits` (negative to descale).
pub trait Scaled {
    fn scaled(self, digits: i32) -> Self;
}

impl Scaled for FPDecimal {
    fn scaled(self, digits: i32) -> Self {
        self * FPDecimal::from(10i128)
            .pow(FPDecimal::from(digits as i128))
            .unwrap()
    }
}

/// `10^18`, the exchange-module scale factor.
pub fn dec_scale_factor() -> FPDecimal {
    FPDecimal::ONE.scaled(18)
}

/// Round `num` UP to the nearest multiple of `min_tick` (never below `min_tick`).
pub fn round_up_to_min_tick(num: FPDecimal, min_tick: FPDecimal) -> FPDecimal {
    if num < min_tick {
        return min_tick;
    }

    let remainder = FPDecimal::from(num.num % min_tick.num);

    if remainder.num.is_zero() {
        return num;
    }

    FPDecimal::from(num.num - remainder.num + min_tick.num)
}

// ---------------------------------------------------------------------------
// Orderbook walk / pricing (ported from queries.rs, from-source only)
// ---------------------------------------------------------------------------

/// Walk price levels until `total` (in the unit produced by `calc`) is covered,
/// taking a partial slice of the last level. Errors if the book lacks liquidity.
pub fn get_minimum_liquidity_levels(
    levels: &[PriceLevel],
    total: FPDecimal,
    calc: fn(&PriceLevel) -> FPDecimal,
    min_quantity_tick_size: FPDecimal,
) -> StdResult<Vec<PriceLevel>> {
    let mut sum = FPDecimal::ZERO;
    let mut orders: Vec<PriceLevel> = Vec::new();

    for level in levels {
        let value = calc(level);
        if value.is_zero() {
            return Err(StdError::msg("price level with zero value"));
        }

        let order_to_add = if sum + value > total {
            let excess = value + sum - total;

            // we only take a part of this price level
            let raw_quantity = ((value - excess) / value) * level.q;
            let rounded_quantity = round_up_to_min_tick(raw_quantity, min_quantity_tick_size);

            PriceLevel {
                p: level.p,
                q: rounded_quantity,
            }
        } else {
            level.clone() // take fully
        };

        sum += value;
        orders.push(order_to_add);

        if sum >= total {
            break;
        }
    }

    if sum < total {
        return Err(StdError::msg("Not enough liquidity to fulfill order"));
    }

    Ok(orders)
}

/// Quantity-weighted average price across the consumed levels. `is_rounding_up`
/// biases the estimate to the worse side for the trader (up for buys, down for sells).
fn get_average_price_from_orders(
    levels: &[PriceLevel],
    min_price_tick_size: FPDecimal,
    is_rounding_up: bool,
) -> StdResult<FPDecimal> {
    let (total_quantity, total_notional) = levels
        .iter()
        .fold((FPDecimal::ZERO, FPDecimal::ZERO), |acc, pl| {
            (acc.0 + pl.q, acc.1 + pl.p * pl.q)
        });

    if total_quantity.is_zero() {
        return Err(StdError::msg("no quantity in consumed levels"));
    }
    let average_price = total_notional / total_quantity;

    Ok(if is_rounding_up {
        round_up_to_min_tick(average_price, min_price_tick_size)
    } else {
        round_to_min_tick(average_price, min_price_tick_size)
    })
}

/// The worst (last consumed) price level. Used as the atomic order's price bound.
fn get_worst_price_from_orders(levels: &[PriceLevel]) -> StdResult<FPDecimal> {
    levels
        .last()
        .map(|l| l.p)
        .ok_or_else(|| StdError::msg("no consumed price levels"))
}

/// Fee discount applied because the aggregator self-relays. The standalone
/// contract gated this on `fee_recipient == contract`; the merged aggregator is
/// always its own fee recipient, so this is always the full relayer-fee share.
fn get_effective_fee_discount_rate(market: &SpotMarket, is_self_relayer: bool) -> FPDecimal {
    if !is_self_relayer {
        FPDecimal::ZERO
    } else {
        market.relayer_fee_share_rate
    }
}

// ---------------------------------------------------------------------------
// Single-market estimators (from-source only)
// ---------------------------------------------------------------------------

/// Estimate / size a single-market orderbook hop where the caller supplies the
/// **input** quantity (`input`, in either the market's base or quote denom).
///
/// `is_simulation = true` for the on-chain `SimulateRoute` quote; `false` during
/// execution, where the contract already holds the input funds (the buy-side
/// margin check accounts for that). The aggregator self-relays, so the fee
/// discount is always applied.
pub fn estimate_single_swap_execution(
    deps: &Deps<InjectiveQueryWrapper>,
    contract_address: &Addr,
    market: &SpotMarket,
    input: FPCoin,
    is_simulation: bool,
) -> StdResult<StepExecutionEstimate> {
    let querier = InjectiveQuerier::new(&deps.querier);

    let has_invalid_denom = input.denom != market.quote_denom && input.denom != market.base_denom;
    if has_invalid_denom {
        return Err(StdError::msg("Invalid swap denom - neither base nor quote"));
    }

    // Merged aggregator is always its own fee recipient.
    let is_self_relayer = true;

    let fee_multiplier = querier
        .query_market_atomic_execution_fee_multiplier(&market.market_id)?
        .multiplier;

    // The chain reserves the GROSS atomic taker fee as order margin; the relayer-
    // fee-share discount is only rebated *after* the trade (to the fee recipient,
    // i.e. this contract). So size BUY orders with the gross fee — sizing with the
    // discounted net fee over-commits the held quote and the order is rejected
    // ("insufficient funds"). SELL output uses the net fee (what the self-relaying
    // contract actually nets, discount included).
    let gross_fee_fraction = market.taker_fee_rate * fee_multiplier;
    let net_fee_fraction = gross_fee_fraction
        * (FPDecimal::ONE - get_effective_fee_discount_rate(market, is_self_relayer));

    // from-source: paying quote => buying base; paying base => selling.
    let is_buy = input.denom != market.base_denom;

    if is_buy {
        estimate_execution_buy_from_source(
            deps,
            &querier,
            contract_address,
            market,
            input.amount,
            gross_fee_fraction,
            is_simulation,
        )
    } else {
        estimate_execution_sell_from_source(&querier, market, input.amount, net_fee_fraction)
    }
}

/// Buy base with a known quote input. Overestimates price (rounds avg up) so the
/// sizing is conservative. Verifies the contract holds enough quote margin.
fn estimate_execution_buy_from_source(
    deps: &Deps<InjectiveQueryWrapper>,
    querier: &InjectiveQuerier,
    contract_address: &Addr,
    market: &SpotMarket,
    input_quote_quantity: FPDecimal,
    fee_fraction: FPDecimal,
    is_simulation: bool,
) -> StdResult<StepExecutionEstimate> {
    let available_swap_quote_funds = input_quote_quantity / (FPDecimal::ONE + fee_fraction);

    let orders = querier.query_spot_market_orderbook(
        &market.market_id,
        OrderSide::Sell,
        None,
        Some(available_swap_quote_funds),
    )?;
    let top_orders = get_minimum_liquidity_levels(
        &orders.sells_price_level,
        available_swap_quote_funds,
        |l| l.q * l.p,
        market.min_quantity_tick_size,
    )?;

    let worst_price = get_worst_price_from_orders(&top_orders)?;

    // Size the quantity from `worst_price`, NOT the (better) average price. The
    // atomic BUY order is placed at `worst_price` and the chain reserves margin at
    // that price: `worst_price * qty * (1+fee)`. Sizing from the average price would
    // need `(worst/avg) * input` margin — more than the `input` the contract holds —
    // so any fill crossing more than one price level would be rejected for
    // insufficient funds. With worst-price sizing the margin is exactly
    // `available * (1+fee) == input`; the order fills this quantity out of the
    // cheaper levels and the chain refunds the price-improvement quote to the
    // contract. That refund is not credited back into the route, so it lingers as
    // (admin-recoverable) dust — negligible on a liquid book where worst≈avg, larger
    // only when levels are far apart.
    let expected_base_quantity = available_swap_quote_funds / worst_price;
    let result_quantity = round_to_min_tick(expected_base_quantity, market.min_quantity_tick_size);
    let fee_estimate = input_quote_quantity - available_swap_quote_funds;

    // The funds check only matters for real execution: the atomic order debits the
    // contract's subaccount, so it must already hold enough quote (the user's input,
    // which it's holding during the route) to back the order. In simulation
    // (`SimulateRoute`) no funds are sent — skip the check so a read-only quote does
    // NOT require the aggregator to be pre-seeded with the quote denom.
    if !is_simulation {
        // Check against the rounded quantity actually placed (margin basis).
        let required_funds = worst_price * result_quantity * (FPDecimal::ONE + fee_fraction);
        let funds_in_contract: FPDecimal = deps
            .querier
            .query_balance(contract_address, &market.quote_denom)?
            .amount
            .into();
        if required_funds > funds_in_contract {
            return Err(StdError::msg(format!(
                "Swap amount too high, required funds: {required_funds}, available funds: {funds_in_contract}",
            )));
        }
    }

    Ok(StepExecutionEstimate {
        worst_price,
        result_quantity,
        result_denom: market.base_denom.to_string(),
        is_buy_order: true,
        fee_estimate: Some(FPCoin {
            denom: market.quote_denom.clone(),
            amount: fee_estimate,
        }),
    })
}

/// Sell a known base input for quote. Underestimates price (rounds avg down) so
/// the sizing is conservative. No margin check (the base is already in hand).
fn estimate_execution_sell_from_source(
    querier: &InjectiveQuerier,
    market: &SpotMarket,
    input_base_quantity: FPDecimal,
    fee_fraction: FPDecimal,
) -> StdResult<StepExecutionEstimate> {
    let orders = querier.query_spot_market_orderbook(
        &market.market_id,
        OrderSide::Buy,
        Some(input_base_quantity),
        None,
    )?;

    let top_orders = get_minimum_liquidity_levels(
        &orders.buys_price_level,
        input_base_quantity,
        |l| l.q,
        market.min_quantity_tick_size,
    )?;

    // overestimate for sells => round average price down => lower (worse) sell price
    let average_price =
        get_average_price_from_orders(&top_orders, market.min_price_tick_size, false)?;
    let worst_price = get_worst_price_from_orders(&top_orders)?;

    let expected_exchange_quantity = input_base_quantity * average_price;
    let fee_estimate = expected_exchange_quantity * fee_fraction;
    let expected_quantity = expected_exchange_quantity - fee_estimate;

    Ok(StepExecutionEstimate {
        worst_price,
        result_quantity: expected_quantity,
        result_denom: market.quote_denom.to_string(),
        is_buy_order: false,
        fee_estimate: Some(FPCoin {
            denom: market.quote_denom.clone(),
            amount: fee_estimate,
        }),
    })
}

// ---------------------------------------------------------------------------
// Market resolution + native order construction / reply decoding
// ---------------------------------------------------------------------------

/// Load a spot market by id (errors if the market does not exist).
pub fn load_market(
    deps: Deps<InjectiveQueryWrapper>,
    market_id: &MarketId,
) -> StdResult<SpotMarket> {
    InjectiveQuerier::new(&deps.querier)
        .query_spot_market(market_id)?
        .market
        .ok_or_else(|| StdError::msg(format!("spot market {} not found", market_id.as_str())))
}

/// The denom paid *into* a hop that produces `target_denom` on `market`.
/// Buying base (target = base) pays quote; selling base (target = quote) pays base.
pub fn offer_denom_for(market: &SpotMarket, target_denom: &str) -> StdResult<String> {
    if target_denom == market.base_denom {
        Ok(market.quote_denom.clone())
    } else if target_denom == market.quote_denom {
        Ok(market.base_denom.clone())
    } else {
        Err(StdError::msg(format!(
            "target denom {target_denom} is not in market {}",
            market.market_id.as_str()
        )))
    }
}

/// `true` if a hop producing `target_denom` is a BUY (the aggregator receives base).
pub fn is_buy_for_target(market: &SpotMarket, target_denom: &str) -> bool {
    target_denom == market.base_denom
}

/// Build the atomic spot-market-order message for a single orderbook hop.
///
/// `quantity`/`worst_price` both `Some` => **direct mode**: the order is placed
/// with the caller's exact base quantity and price bound, with no orderbook-walk
/// queries. Otherwise => **estimation mode**: the book is walked to size the order
/// (buy => estimated base quantity; sell => the base input rounded down to tick).
///
/// `input_amount` is the funds (chain scale, `offer_denom`) the contract holds for
/// this hop; the order debits the contract's default subaccount, and the aggregator
/// is its own fee recipient (self-relayer). Returns `Ok(None)` when the order
/// quantity rounds to zero, so the caller can surface `AmountTooSmall`.
pub fn build_swap_order_msg(
    deps: Deps<InjectiveQueryWrapper>,
    contract: &Addr,
    market: &SpotMarket,
    offer_denom: &str,
    input_amount: Uint128,
    quantity: Option<FPDecimal>,
    worst_price: Option<FPDecimal>,
) -> StdResult<Option<CosmosMsg<InjectiveMsgWrapper>>> {
    // Paying quote => buying base; paying base => selling.
    let is_buy = offer_denom != market.base_denom;

    let (price, order_qty) = match (quantity, worst_price) {
        (Some(q), Some(p)) => (p, q),
        _ => {
            let input = FPCoin {
                amount: FPDecimal::from(input_amount),
                denom: offer_denom.to_string(),
            };
            let est = estimate_single_swap_execution(&deps, contract, market, input, false)?;
            let qty = if est.is_buy_order {
                // estimator already rounds the base quantity to tick
                est.result_quantity
            } else {
                // sells trade the base input directly; round it down to the tick
                round_to_min_tick(FPDecimal::from(input_amount), market.min_quantity_tick_size)
            };
            (est.worst_price, qty)
        }
    };

    if order_qty.is_negative() || order_qty.is_zero() {
        return Ok(None);
    }

    let order = SpotOrder::new(
        price,
        order_qty,
        if is_buy {
            OrderType::BuyAtomic
        } else {
            OrderType::SellAtomic
        },
        &market.market_id,
        get_default_subaccount_id_for_checked_address(contract),
        Some(contract.clone()), // self-relayer => keeps the fee-share discount
        None,
    );

    Ok(Some(create_spot_market_order_msg(contract.clone(), order)))
}

/// Decode the filled output (in `target_denom`, chain scale) from an atomic spot
/// market order reply. Returns zero when nothing filled (IOC orders may fill
/// partially or not at all — the route-level `minimum_receive` is the net).
pub fn parse_order_output(
    market: &SpotMarket,
    target_denom: &str,
    response: &SubMsgResponse,
) -> StdResult<Uint128> {
    let first = match response.msg_responses.first() {
        Some(r) => r,
        None => return Ok(Uint128::zero()),
    };
    let decoded = MsgCreateSpotMarketOrderResponse::decode(first.value.as_slice())
        .map_err(|e| StdError::msg(format!("decode failed (type_url={}): {e}", first.type_url)))?;
    let trade = match decoded.results {
        Some(t) => t,
        None => return Ok(Uint128::zero()),
    };

    // protobuf serializes Dec values with an extra 10^18 factor; descale to chain units.
    let scale = dec_scale_factor();
    let price = FPDecimal::from_str(&trade.price).map_err(|_| StdError::msg("bad price"))? / scale;
    let quantity =
        FPDecimal::from_str(&trade.quantity).map_err(|_| StdError::msg("bad quantity"))? / scale;
    let fee = FPDecimal::from_str(&trade.fee).map_err(|_| StdError::msg("bad fee"))? / scale;

    // buy => base received; sell => quote received net of the trading fee.
    let out = if is_buy_for_target(market, target_denom) {
        quantity
    } else {
        quantity * price - fee
    };

    if out.is_negative() || out.is_zero() {
        Ok(Uint128::zero())
    } else {
        Ok(Uint128::from(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_price_level(p: u128, q: u128) -> PriceLevel {
        PriceLevel {
            p: FPDecimal::from(p),
            q: FPDecimal::from(q),
        }
    }

    #[test]
    fn test_average_price_simple() {
        let levels = vec![
            create_price_level(1, 200),
            create_price_level(2, 200),
            create_price_level(3, 200),
        ];

        let avg = get_average_price_from_orders(&levels, FPDecimal::must_from_str("0.01"), false)
            .unwrap();
        assert_eq!(avg, FPDecimal::from(2u128));
    }

    #[test]
    fn test_average_price_round_down() {
        let levels = vec![
            create_price_level(1, 300),
            create_price_level(2, 200),
            create_price_level(3, 100),
        ];

        let avg = get_average_price_from_orders(&levels, FPDecimal::must_from_str("0.01"), false)
            .unwrap();
        assert_eq!(avg, FPDecimal::must_from_str("1.66")); // round down
    }

    #[test]
    fn test_average_price_round_up() {
        let levels = vec![
            create_price_level(1, 300),
            create_price_level(2, 200),
            create_price_level(3, 100),
        ];

        let avg =
            get_average_price_from_orders(&levels, FPDecimal::must_from_str("0.01"), true).unwrap();
        assert_eq!(avg, FPDecimal::must_from_str("1.67")); // round up
    }

    #[test]
    fn test_worst_price() {
        let levels = vec![
            create_price_level(1, 100),
            create_price_level(2, 200),
            create_price_level(3, 300),
        ];

        assert_eq!(
            get_worst_price_from_orders(&levels).unwrap(),
            FPDecimal::from(3u128)
        );
    }

    #[test]
    fn test_min_liquidity_not_enough() {
        let levels = vec![create_price_level(1, 100), create_price_level(2, 200)];

        let result = get_minimum_liquidity_levels(
            &levels,
            FPDecimal::from(1000u128),
            |l| l.q,
            FPDecimal::must_from_str("0.01"),
        );
        assert!(result.is_err());
        // StdError is opaque in cosmwasm-std 3.0 (not PartialEq); match the message.
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Not enough liquidity"));
    }

    #[test]
    fn test_min_liquidity_with_gaps() {
        let levels = vec![
            create_price_level(1, 100),
            create_price_level(3, 300),
            create_price_level(5, 500),
        ];

        let min_orders = get_minimum_liquidity_levels(
            &levels,
            FPDecimal::from(800u128),
            |l| l.q,
            FPDecimal::must_from_str("0.01"),
        )
        .unwrap();
        assert_eq!(min_orders.len(), 3);
        assert_eq!(min_orders[0].p, FPDecimal::from(1u128));
        assert_eq!(min_orders[1].p, FPDecimal::from(3u128));
        assert_eq!(min_orders[2].p, FPDecimal::from(5u128));
    }

    #[test]
    fn test_min_liquidity_partial_last_level() {
        let levels = vec![
            create_price_level(1, 100),
            create_price_level(3, 300),
            create_price_level(5, 500),
        ];

        let min_orders = get_minimum_liquidity_levels(
            &levels,
            FPDecimal::from(450u128),
            |l| l.q,
            FPDecimal::must_from_str("0.01"),
        )
        .unwrap();
        assert_eq!(min_orders.len(), 3);
        assert_eq!(min_orders[2].q, FPDecimal::from(50u128)); // partial slice
    }

    #[test]
    fn test_round_up_to_min_tick() {
        assert_eq!(
            round_up_to_min_tick(FPDecimal::from(37u128), FPDecimal::from(10u128)),
            FPDecimal::from(40u128)
        );
        assert_eq!(
            round_up_to_min_tick(
                FPDecimal::must_from_str("0.00000153"),
                FPDecimal::must_from_str("0.000001")
            ),
            FPDecimal::must_from_str("0.000002")
        );
        // below one tick rounds up to exactly one tick
        assert_eq!(
            round_up_to_min_tick(
                FPDecimal::must_from_str("0.0000001"),
                FPDecimal::must_from_str("0.000001")
            ),
            FPDecimal::must_from_str("0.000001")
        );
    }

    #[test]
    fn test_dec_scale_factor_roundtrip() {
        let val = FPDecimal::must_from_str("1000000000000000000");
        assert_eq!(val.scaled(-18), FPDecimal::from(1u128));
        assert_eq!(dec_scale_factor(), val);
    }
}
