use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use std::str::FromStr;
use tracing::{debug, info, warn};

use super::{HedgeParams, Hedger}; // Используем типы из родительского модуля
use crate::exchange::bybit::SPOT_CATEGORY;
use crate::exchange::Exchange;
use crate::models::HedgeRequest;

// Делаем функцию pub(super), чтобы она была доступна в mod.rs
pub(super) async fn calculate_hedge_params_impl<E>(
    exchange: &E,
    req: &HedgeRequest,
    slippage: f64,
    quote_currency: &str,
    max_allowed_leverage: f64,
) -> Result<HedgeParams>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let HedgeRequest {
        sum,
        symbol,
        volatility,
    } = req;
    debug!("Calculating hedge params for {}...", symbol);

    let spot_info = exchange
        .get_spot_instrument_info(symbol)
        .await
        .map_err(|e| anyhow!("Failed to get SPOT instrument info: {}", e))?;
    let linear_info = exchange
        .get_linear_instrument_info(symbol)
        .await
        .map_err(|e| anyhow!("Failed to get LINEAR instrument info: {}", e))?;

    let spot_fee = match exchange.get_fee_rate(symbol, SPOT_CATEGORY).await {
        Ok(fee) => {
            info!("Spot fee rate: Taker={}", fee.taker);
            fee.taker
        }
        Err(e) => {
            warn!(
                "Could not get spot fee rate: {}. Using fallback 0.001",
                e
            );
            0.001 // TODO: Сделать настраиваемым?
        }
    };

    let mmr = exchange.get_mmr(symbol).await?;
    let initial_spot_value = sum / ((1.0 + volatility) * (1.0 + mmr));

    if initial_spot_value <= 0.0 {
        return Err(anyhow!("Initial spot value is non-positive"));
    }

    let current_spot_price = exchange.get_spot_price(symbol).await?;
    if current_spot_price <= 0.0 {
        return Err(anyhow!("Invalid spot price: {}", current_spot_price));
    }

    let ideal_gross_qty = initial_spot_value / current_spot_price;
    debug!(
        "Ideal gross quantity (before fees/rounding): {}",
        ideal_gross_qty
    );

    // --- Расчет точности фьючерса ---
    let fut_qty_step_str = linear_info
        .lot_size_filter
        .qty_step
        .as_deref()
        .ok_or_else(|| anyhow!("Missing qtyStep for futures"))?;
    let fut_decimals = fut_qty_step_str
        .split('.')
        .nth(1)
        .map_or(0, |s| s.trim_end_matches('0').len()) as u32;
    let min_fut_qty_str = &linear_info.lot_size_filter.min_order_qty;
    let min_fut_qty_decimal = Decimal::from_str(min_fut_qty_str)
        .map_err(|e| anyhow!("Failed to parse min futures qty '{}': {}", min_fut_qty_str, e))?;
    debug!(
        "Futures precision: {} decimals, Min Qty: {}",
        fut_decimals, min_fut_qty_decimal
    );

    // --- Расчет точности спота ---
    let spot_precision_str = spot_info
        .lot_size_filter
        .base_precision
        .as_deref()
        .ok_or_else(|| anyhow!("Missing basePrecision for spot"))?;
    let spot_decimals = spot_precision_str
        .split('.')
        .nth(1)
        .map_or(0, |s| s.trim_end_matches('0').len()) as u32;
    let min_spot_qty_str = &spot_info.lot_size_filter.min_order_qty;
    let min_spot_qty_decimal = Decimal::from_str(min_spot_qty_str)
        .map_err(|e| anyhow!("Failed to parse min spot qty '{}': {}", min_spot_qty_str, e))?;
    debug!(
        "Spot precision: {} decimals, Min Qty: {}",
        spot_decimals, min_spot_qty_decimal
    );

    // --- Расчет количества ---
    let target_net_qty_decimal = Decimal::from_f64(ideal_gross_qty)
        .ok_or_else(|| anyhow!("Failed to convert ideal qty to Decimal"))?
        .trunc_with_scale(fut_decimals); // Округляем до точности фьючерса

    if target_net_qty_decimal < min_fut_qty_decimal {
        return Err(anyhow!(
            "Target net quantity {:.8} < min futures quantity {}",
            target_net_qty_decimal,
            min_fut_qty_decimal
        ));
    }
    let target_net_qty = target_net_qty_decimal
        .to_f64()
        .ok_or_else(|| anyhow!("Failed to convert target net qty back to f64"))?;
    debug!(
        "Target NET quantity (rounded to fut_decimals): {}",
        target_net_qty
    );

    if (1.0 - spot_fee).abs() < f64::EPSILON {
        return Err(anyhow!("Spot fee rate is 100% or invalid"));
    }
    let required_gross_qty = target_net_qty / (1.0 - spot_fee);
    debug!(
        "Required GROSS quantity (to get target net after fee): {}",
        required_gross_qty
    );

    let final_spot_gross_qty_decimal = Decimal::from_f64(required_gross_qty)
        .ok_or_else(|| anyhow!("Failed to convert required gross qty to Decimal"))?
        .trunc_with_scale(spot_decimals); // Округляем до точности спота

    if final_spot_gross_qty_decimal < min_spot_qty_decimal {
        // Если после округления стало меньше минимума, возможно, стоит увеличить до минимума?
        // Или вернуть ошибку, как сейчас. Оставим ошибку для ясности.
        return Err(anyhow!(
            "Calculated final spot quantity {:.8} < min spot quantity {}",
            final_spot_gross_qty_decimal,
            min_spot_qty_decimal
        ));
    }
    let final_spot_gross_qty = final_spot_gross_qty_decimal
        .to_f64()
        .ok_or_else(|| anyhow!("Failed to convert final gross qty back to f64"))?;
    debug!(
        "Final SPOT GROSS quantity (rounded to spot_decimals): {}",
        final_spot_gross_qty
    );

    let spot_order_qty = final_spot_gross_qty;
    let fut_order_qty = target_net_qty; // Используем уже округленное до точности фьючерса

    if spot_order_qty <= 0.0 || fut_order_qty <= 0.0 {
        return Err(anyhow!(
            "Final order quantities are non-positive: spot={}, fut={}",
            spot_order_qty,
            fut_order_qty
        ));
    }

    // --- Расчет стоимости и плеча ---
    let adjusted_spot_value = spot_order_qty * current_spot_price;
    let available_collateral = sum - adjusted_spot_value;
    let futures_position_value = fut_order_qty * current_spot_price; // Оценка по текущей спот цене

    debug!("Adjusted spot value (cost): {}", adjusted_spot_value);
    debug!(
        "Available collateral after spot buy: {}",
        available_collateral
    );
    debug!(
        "Futures position value (based on net qty): {}",
        futures_position_value
    );

    if available_collateral <= 0.0 {
        // Эта проверка может быть излишней, если sum всегда > adjusted_spot_value, но оставим для надежности
        return Err(anyhow!(
            "Available collateral non-positive after spot value calculation (Sum: {}, Spot Value: {})",
            sum, adjusted_spot_value
        ));
    }

    let required_leverage = futures_position_value / available_collateral;
    debug!("Calculated required leverage: {}", required_leverage);

    if required_leverage.is_nan() || required_leverage.is_infinite() {
        return Err(anyhow!(
            "Invalid leverage calculation (Fut Value: {}, Collateral: {})",
            futures_position_value, available_collateral
        ));
    }
    if required_leverage > max_allowed_leverage {
        return Err(anyhow!(
            "Required leverage {:.2}x > max allowed {:.2}x",
            required_leverage,
            max_allowed_leverage
        ));
    }
    info!(
        "Required leverage {:.2}x is within max allowed {:.2}x",
        required_leverage, max_allowed_leverage
    );

    // Начальная цена для лимитного ордера (для run_hedge)
    let initial_limit_price = current_spot_price * (1.0 - slippage);
    debug!("Initial limit price for spot buy: {}", initial_limit_price);

    let futures_symbol = format!("{}{}", symbol, quote_currency);

    Ok(HedgeParams {
        spot_order_qty,
        fut_order_qty,
        current_spot_price,
        initial_limit_price,
        symbol: symbol.clone(),
        spot_value: adjusted_spot_value,
        available_collateral,
        min_spot_qty_decimal, // Передаем дальше
        min_fut_qty_decimal,  // Передаем дальше
        spot_decimals,        // Передаем дальше
        fut_decimals,         // Передаем дальше
        futures_symbol,       // Передаем дальше
    })
}
