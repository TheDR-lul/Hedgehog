// src/hedger/params.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use rust_decimal::Decimal; // Явный импорт Decimal
use std::str::FromStr;
use tracing::{debug, info, warn}; // Добавил warn, если понадобится

use crate::hedger::{HedgeParams, ORDER_FILL_TOLERANCE}; // Используем ORDER_FILL_TOLERANCE из родительского модуля
use crate::exchange::bybit::SPOT_CATEGORY;
use crate::exchange::Exchange;
use crate::models::HedgeRequest;

// ИЗМЕНЕНО: делаем функцию pub и адаптируем для &dyn Exchange
pub async fn calculate_hedge_params_impl<E>(
    exchange: &E, // Принимаем &E, где E: Exchange + ?Sized
    req: &HedgeRequest,
    slippage: f64,
    quote_currency: &str,
    max_allowed_leverage: f64,
) -> Result<HedgeParams>
where
    E: Exchange + ?Sized + Send + Sync, // Добавляем Send + Sync, т.к. exchange используется в await
{
    let HedgeRequest {
        sum,
        symbol,
        volatility,
    } = req;
    debug!("Calculating hedge params for {}...", symbol);

    // Убедимся, что quote_currency используется для формирования пар
    let base_symbol_upper = symbol.to_uppercase();
    let quote_currency_upper = quote_currency.to_uppercase();

    let spot_pair_for_info = base_symbol_upper.as_str(); // get_spot_instrument_info ожидает базовый символ
    let linear_pair_for_info = base_symbol_upper.as_str(); // get_linear_instrument_info ожидает базовый символ

    let spot_info = exchange
        .get_spot_instrument_info(spot_pair_for_info)
        .await
        .map_err(|e| anyhow!("Failed to get SPOT instrument info for {}: {}", spot_pair_for_info, e))?;

    let linear_info = exchange
        .get_linear_instrument_info(linear_pair_for_info)
        .await
        .map_err(|e| anyhow!("Failed to get LINEAR instrument info for {}: {}", linear_pair_for_info, e))?;

    let spot_pair_for_fee = format!("{}{}", base_symbol_upper, quote_currency_upper);
    let spot_fee = match exchange.get_fee_rate(&spot_pair_for_fee, SPOT_CATEGORY).await {
        Ok(fee) => {
            info!("Spot fee rate for {}: Taker={}", spot_pair_for_fee, fee.taker);
            fee.taker
        }
        Err(e) => {
            warn!(
                "Could not get spot fee rate for {}: {}. Using fallback 0.001",
                spot_pair_for_fee, e
            );
            0.001
        }
    };

    let futures_symbol = format!("{}{}", base_symbol_upper, quote_currency_upper);
    debug!("Using futures symbol {} for MMR lookup", futures_symbol);

    let mmr = exchange.get_mmr(&futures_symbol).await?;
    let initial_spot_value = sum / ((1.0 + volatility) * (1.0 + mmr));

    if initial_spot_value <= 0.0 {
        return Err(anyhow!("Initial spot value ({}) is non-positive. Sum: {}, Vol: {}, MMR: {}", initial_spot_value, sum, volatility, mmr));
    }

    let current_spot_price = exchange.get_spot_price(&base_symbol_upper).await?;
    if current_spot_price <= 0.0 {
        return Err(anyhow!("Invalid spot price: {}", current_spot_price));
    }

    let ideal_gross_qty = initial_spot_value / current_spot_price;
    debug!(
        "Ideal gross quantity (before fees/rounding): {}",
        ideal_gross_qty
    );

    let fut_qty_step_str = linear_info
        .lot_size_filter
        .qty_step
        .as_deref()
        .ok_or_else(|| anyhow!("Missing qtyStep for futures {}", futures_symbol))?;
    let fut_decimals = fut_qty_step_str
        .split('.')
        .nth(1)
        .map_or(0, |s| s.trim_end_matches('0').len()) as u32;
    let min_fut_qty_str = &linear_info.lot_size_filter.min_order_qty;
    let min_fut_qty_decimal = Decimal::from_str(min_fut_qty_str)
        .map_err(|e| anyhow!("Failed to parse min futures qty '{}' for {}: {}", min_fut_qty_str, futures_symbol, e))?;
    debug!(
        "Futures {}: precision: {} decimals, Min Qty: {}",
        futures_symbol, fut_decimals, min_fut_qty_decimal
    );

    let spot_precision_str = spot_info
        .lot_size_filter
        .base_precision
        .as_deref()
        .ok_or_else(|| anyhow!("Missing basePrecision for spot {}", spot_pair_for_fee))?;
    let spot_decimals = spot_precision_str
        .split('.')
        .nth(1)
        .map_or(0, |s| s.trim_end_matches('0').len()) as u32;
    let min_spot_qty_str = &spot_info.lot_size_filter.min_order_qty;
    let min_spot_qty_decimal = Decimal::from_str(min_spot_qty_str)
        .map_err(|e| anyhow!("Failed to parse min spot qty '{}' for {}: {}", min_spot_qty_str, spot_pair_for_fee, e))?;
    debug!(
        "Spot {}: precision: {} decimals, Min Qty: {}",
        spot_pair_for_fee, spot_decimals, min_spot_qty_decimal
    );

    let target_net_qty_decimal = Decimal::from_f64(ideal_gross_qty)
        .ok_or_else(|| anyhow!("Failed to convert ideal qty {} to Decimal", ideal_gross_qty))?
        .trunc_with_scale(fut_decimals);

    if target_net_qty_decimal < min_fut_qty_decimal && target_net_qty_decimal.abs() > dec!(1e-12) {
        return Err(anyhow!(
            "Target net quantity {:.8} for {} < min futures quantity {} (and not zero)",
            target_net_qty_decimal, futures_symbol, min_fut_qty_decimal
        ));
    }
    let target_net_qty = target_net_qty_decimal
        .to_f64()
        .ok_or_else(|| anyhow!("Failed to convert target net qty {} back to f64", target_net_qty_decimal))?;
    debug!(
        "Target NET quantity for {} (rounded to fut_decimals): {}",
        futures_symbol, target_net_qty
    );

    if (1.0 - spot_fee).abs() < f64::EPSILON {
        return Err(anyhow!("Spot fee rate is 100% or invalid (spot_fee: {})", spot_fee));
    }
    let required_gross_qty = target_net_qty / (1.0 - spot_fee);
    debug!(
        "Required GROSS quantity for {} (to get target net after fee {}): {}",
        spot_pair_for_fee, spot_fee, required_gross_qty
    );

    let final_spot_gross_qty_decimal = Decimal::from_f64(required_gross_qty)
        .ok_or_else(|| anyhow!("Failed to convert required gross qty {} to Decimal", required_gross_qty))?
        .trunc_with_scale(spot_decimals);

    if final_spot_gross_qty_decimal < min_spot_qty_decimal && final_spot_gross_qty_decimal.abs() > dec!(1e-12) {
        return Err(anyhow!(
            "Calculated final spot quantity {:.8} for {} < min spot quantity {} (and not zero)",
            final_spot_gross_qty_decimal, spot_pair_for_fee, min_spot_qty_decimal
        ));
    }
    let final_spot_gross_qty = final_spot_gross_qty_decimal
        .to_f64()
        .ok_or_else(|| anyhow!("Failed to convert final gross qty {} back to f64", final_spot_gross_qty_decimal))?;
    debug!(
        "Final SPOT GROSS quantity for {} (rounded to spot_decimals): {}",
        spot_pair_for_fee, final_spot_gross_qty
    );

    let spot_order_qty = final_spot_gross_qty;
    let fut_order_qty = target_net_qty;

    if spot_order_qty < 0.0 || fut_order_qty < 0.0 {
        return Err(anyhow!(
            "Final order quantities are negative: spot={}, fut={}",
            spot_order_qty, fut_order_qty
        ));
    }

    let min_spot_qty_f64 = min_spot_qty_decimal.to_f64().unwrap_or(0.0);
    if spot_order_qty < min_spot_qty_f64 && spot_order_qty.abs() > ORDER_FILL_TOLERANCE {
          return Err(anyhow!("Final spot_order_qty {:.8} is less than min_spot_qty {:.8}", spot_order_qty, min_spot_qty_f64));
    }
    let min_fut_qty_f64 = min_fut_qty_decimal.to_f64().unwrap_or(0.0);
    if fut_order_qty < min_fut_qty_f64 && fut_order_qty.abs() > ORDER_FILL_TOLERANCE {
         return Err(anyhow!("Final fut_order_qty {:.8} is less than min_fut_qty {:.8}", fut_order_qty, min_fut_qty_f64));
    }

    let adjusted_spot_value = spot_order_qty * current_spot_price;
    let available_collateral = sum - adjusted_spot_value;
    let futures_position_value = fut_order_qty * current_spot_price;

    debug!("Adjusted spot value (cost): {:.8}", adjusted_spot_value);
    debug!("Available collateral after spot buy: {:.8}", available_collateral);
    debug!("Futures position value (based on net qty and current spot price): {:.8}", futures_position_value);

    if available_collateral <= 0.0 && fut_order_qty > ORDER_FILL_TOLERANCE {
        return Err(anyhow!(
            "Available collateral ({:.8}) non-positive after spot value calculation (Sum: {}, Spot Value: {:.8}) with non-zero futures quantity ({:.8}) required.",
            available_collateral, sum, adjusted_spot_value, fut_order_qty
        ));
    }

    let required_leverage = if available_collateral.abs() < f64::EPSILON {
        if futures_position_value.abs() < f64::EPSILON { 1.0 } else { f64::INFINITY }
    } else {
        (futures_position_value / available_collateral).abs()
    };
    debug!("Calculated required leverage: {:.4}x", required_leverage);

    if fut_order_qty > ORDER_FILL_TOLERANCE { // Проверяем плечо, только если есть фьючерсы
        if required_leverage.is_nan() || required_leverage.is_infinite() {
            return Err(anyhow!(
                "Invalid leverage calculation (Infinity/NaN) with non-zero futures. Fut Value: {:.8}, Collateral: {:.8}",
                futures_position_value, available_collateral
            ));
        }
        if required_leverage > max_allowed_leverage {
            return Err(anyhow!(
                "Required leverage {:.2}x > max allowed {:.2}x",
                required_leverage, max_allowed_leverage
            ));
        }
        info!(
            "Required leverage {:.2}x is within max allowed {:.2}x",
            required_leverage, max_allowed_leverage
        );
    }

    let initial_limit_price = current_spot_price * (1.0 - slippage);
    debug!("Initial limit price for spot buy: {:.8}", initial_limit_price);

    Ok(HedgeParams {
        spot_order_qty,
        fut_order_qty,
        current_spot_price,
        initial_limit_price,
        symbol: base_symbol_upper.clone(), // Используем базовый символ
        spot_value: adjusted_spot_value,
        available_collateral,
        min_spot_qty_decimal,
        min_fut_qty_decimal,
        spot_decimals,
        fut_decimals,
        futures_symbol, // Уже содержит отформатированный символ фьючерса
    })
}