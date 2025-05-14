// src/webservice_hedge/hedge_logic/init.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::{debug, info, warn}; // Убрал error, если он не используется напрямую здесь
// use std::time::Duration; // Закомментировано, если не используется
// use tokio::time::sleep; // Закомментировано, если не используется
use std::str::FromStr;

use crate::config::Config;
use crate::exchange::{Exchange, bybit::SPOT_CATEGORY}; // Убедимся, что SPOT_CATEGORY импортирован, если используется
use crate::models::HedgeRequest;
use crate::storage;
use crate::webservice_hedge::state::{HedgerWsState}; // Убрал HedgerWsStatus, если он не используется для установки начального статуса здесь
use crate::webservice_hedge::common::calculate_auto_chunk_parameters;
use crate::webservice_hedge::hedge_logic::helpers::{get_step_decimal}; // Убрал get_decimals_from_step, если он не используется

pub async fn create_initial_hedger_ws_state(
    operation_id: i64,
    request: HedgeRequest,
    config: Arc<Config>,
    exchange_rest: Arc<dyn Exchange>,
    _database: Arc<storage::Db>,
) -> Result<HedgerWsState> {
    info!(operation_id, "Creating initial HedgerWsState for operation...");

    let base_symbol = request.symbol.to_uppercase();
    let spot_symbol_name = format!("{}{}", base_symbol, config.quote_currency);
    let futures_symbol_name = format!("{}{}", base_symbol, config.quote_currency);

    debug!(operation_id, %spot_symbol_name, %futures_symbol_name, "Fetching instrument info for HedgerWsState...");

    let (
        spot_info_res,
        linear_info_res,
        _fee_rate_res, // Пометил как неиспользуемый, если это так
        mmr_res,
        spot_price_res
    ) = tokio::join!(
        exchange_rest.get_spot_instrument_info(&base_symbol),
        exchange_rest.get_linear_instrument_info(&base_symbol),
        exchange_rest.get_fee_rate(&spot_symbol_name, SPOT_CATEGORY), // Используем SPOT_CATEGORY
        exchange_rest.get_mmr(&futures_symbol_name),
        exchange_rest.get_spot_price(&base_symbol)
    );

    let spot_info = spot_info_res.context("Failed to get SPOT instrument info for state init")?;
    let linear_info = linear_info_res.context("Failed to get LINEAR instrument info for state init")?;
    let _fee_rate = _fee_rate_res.context("Failed to get SPOT fee rate for state init")?; // Сохраняем, если используется где-то ниже
    let maintenance_margin_rate = mmr_res.context("Failed to get Futures MMR for state init")?;
    let current_spot_price_f64 = spot_price_res.context("Failed to get current SPOT price for state init")?;

    if current_spot_price_f64 <= 0.0 { return Err(anyhow!("Initial spot price is non-positive for state init")); }
    let current_spot_price = Decimal::try_from(current_spot_price_f64)?;

    let initial_user_sum_decimal = Decimal::try_from(request.sum)
        .map_err(|e| anyhow!("Failed to convert request.sum to Decimal: {}", e))?;

    let volatility_decimal = Decimal::try_from(request.volatility)?;

    let maintenance_margin_rate_decimal = Decimal::try_from(maintenance_margin_rate)
        .map_err(|e| anyhow!("Failed to convert maintenance_margin_rate to Decimal: {}", e))?;

    let denominator = (Decimal::ONE + volatility_decimal) * (Decimal::ONE + maintenance_margin_rate_decimal);
    if denominator == Decimal::ZERO { return Err(anyhow!("Denominator for initial spot value calculation is zero for state init")); }

    let initial_target_spot_value = initial_user_sum_decimal / denominator;
    debug!(operation_id, %initial_target_spot_value, %initial_user_sum_decimal, %volatility_decimal, %maintenance_margin_rate_decimal, "Calculated initial target spot value for state init");

    let ideal_gross_spot_quantity = if current_spot_price > Decimal::ZERO {
        initial_target_spot_value / current_spot_price
    } else {
        return Err(anyhow!("Current spot price is zero, cannot calculate ideal gross spot quantity for state init"));
    };

    let fut_qty_step_str = linear_info.lot_size_filter.qty_step.as_deref()
        .ok_or_else(|| anyhow!("Missing qty_step for futures instrument {}", futures_symbol_name))?;
    let fut_decimals = fut_qty_step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;

    let initial_target_futures_quantity = ideal_gross_spot_quantity.trunc_with_scale(fut_decimals);
    debug!(operation_id, %initial_target_futures_quantity, %ideal_gross_spot_quantity, fut_decimals, "Calculated initial target futures quantity (estimate) for state init");

    let target_chunk_count = config.ws_auto_chunk_target_count;
    let min_spot_quantity = Decimal::from_str(&spot_info.lot_size_filter.min_order_qty)?;
    let min_futures_quantity = Decimal::from_str(&linear_info.lot_size_filter.min_order_qty)?;
    let spot_quantity_step = get_step_decimal(spot_info.lot_size_filter.base_precision.as_deref())?;
    let futures_quantity_step = get_step_decimal(linear_info.lot_size_filter.qty_step.as_deref())?;
    let min_spot_notional = spot_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());
    let min_futures_notional = linear_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());

    let current_futures_price_estimate = match exchange_rest.get_market_price(&futures_symbol_name, false).await {
        Ok(price) if price > 0.0 => Decimal::try_from(price)?,
        _ => {
            warn!(operation_id, "Could not get futures price for chunk calculation, using current spot price as estimate.");
            current_spot_price
        }
    };

    // --- ДОБАВЛЕНО ЛОГИРОВАНИЕ ПЕРЕД ВЫЗОВОМ calculate_auto_chunk_parameters ---
    debug!(
        operation_id,
        initial_target_spot_value_param = %initial_target_spot_value,
        initial_target_futures_quantity_param = %initial_target_futures_quantity,
        current_spot_price_param = %current_spot_price,
        current_futures_price_estimate_param = %current_futures_price_estimate,
        target_chunk_count_param = target_chunk_count,
        "Parameters prepared for calculate_auto_chunk_parameters"
    );
    // --- КОНЕЦ ЛОГИРОВАНИЯ ---

    let (final_chunk_count, chunk_spot_quantity, chunk_futures_quantity) =
        calculate_auto_chunk_parameters(
            initial_target_spot_value, initial_target_futures_quantity,
            current_spot_price, current_futures_price_estimate,
            target_chunk_count, min_spot_quantity, min_futures_quantity,
            spot_quantity_step, futures_quantity_step,
            min_spot_notional, min_futures_notional,
        )?;
    info!(operation_id, final_chunk_count, %chunk_spot_quantity, %chunk_futures_quantity, "Chunk parameters calculated for state init");

    let spot_tick_size = Decimal::from_str(&spot_info.price_filter.tick_size)?;
    let futures_tick_size = Decimal::from_str(&linear_info.price_filter.tick_size)?;

    let mut state = HedgerWsState::new_hedge(
        operation_id,
        spot_symbol_name.clone(),
        futures_symbol_name.clone(),
        initial_target_spot_value,
        initial_target_futures_quantity,
        initial_user_sum_decimal,
    );
    state.spot_tick_size = spot_tick_size;
    state.spot_quantity_step = spot_quantity_step;
    state.min_spot_quantity = min_spot_quantity;
    state.min_spot_notional = min_spot_notional;
    state.futures_tick_size = futures_tick_size;
    state.futures_quantity_step = futures_quantity_step;
    state.min_futures_quantity = min_futures_quantity;
    state.min_futures_notional = min_futures_notional;
    state.total_chunks = final_chunk_count;
    state.chunk_base_quantity_spot = chunk_spot_quantity;
    state.chunk_base_quantity_futures = chunk_futures_quantity;

    info!(operation_id, "HedgerWsState initialized successfully.");
    Ok(state)
}