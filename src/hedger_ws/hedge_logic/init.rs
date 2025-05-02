// src/hedger_ws/hedge_logic/init.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::mpsc;
// Убираем неиспользуемый warn
use tracing::{debug, info};
use std::time::Duration;
use tokio::time::sleep;
use std::str::FromStr;

use crate::config::Config;
// Убираем неиспользуемый LINEAR_CATEGORY из прямого импорта
use crate::exchange::{Exchange, bybit::SPOT_CATEGORY};
use crate::exchange::types::WebSocketMessage;
use crate::hedger::HedgeProgressCallback;
use crate::models::HedgeRequest;
use crate::storage;
use crate::hedger_ws::hedge_task::HedgerWsHedgeTask; // Доступ к структуре
use crate::hedger_ws::state::{HedgerWsState, HedgerWsStatus}; // Доступ к состояниям
use crate::hedger_ws::common::calculate_auto_chunk_parameters; // Доступ к common
use crate::hedger_ws::hedge_logic::helpers::{get_decimals_from_step, get_step_decimal}; // Доступ к хелперам

pub async fn initialize_task(
    operation_id: i64,
    request: HedgeRequest,
    config: Arc<Config>,
    database: Arc<storage::Db>,
    exchange_rest: Arc<dyn Exchange>,
    progress_callback: HedgeProgressCallback,
    ws_receiver: mpsc::Receiver<Result<WebSocketMessage>>,
) -> Result<HedgerWsHedgeTask> {
    info!(operation_id, "Initializing HedgerWsHedgeTask...");

    // --- 1. Получение информации об инструментах и начальные расчеты ---
    let base_symbol = request.symbol.to_uppercase();
    let spot_symbol_name = format!("{}{}", base_symbol, config.quote_currency);
    let futures_symbol_name = format!("{}{}", base_symbol, config.quote_currency);

    debug!(operation_id, %spot_symbol_name, %futures_symbol_name, "Fetching instrument info...");

    // --- ИСПРАВЛЕНО: Используем один tokio::join! ---
    let (
        spot_info_res,
        linear_info_res,
        fee_rate_res,
        mmr_res,
        spot_price_res
    ) = tokio::join!(
        exchange_rest.get_spot_instrument_info(&base_symbol),
        exchange_rest.get_linear_instrument_info(&base_symbol),
        exchange_rest.get_fee_rate(&spot_symbol_name, SPOT_CATEGORY),
        exchange_rest.get_mmr(&futures_symbol_name),
        exchange_rest.get_spot_price(&base_symbol)
    );
    // --- КОНЕЦ ИСПРАВЛЕНИЯ ---

    // Обработка результатов
    let spot_info = spot_info_res.context("Failed to get SPOT instrument info")?;
    let linear_info = linear_info_res.context("Failed to get LINEAR instrument info")?;
    let _fee_rate = fee_rate_res.context("Failed to get SPOT fee rate")?;
    let maintenance_margin_rate = mmr_res.context("Failed to get Futures MMR")?;
    let current_spot_price_f64 = spot_price_res.context("Failed to get current SPOT price")?;

    if current_spot_price_f64 <= 0.0 { return Err(anyhow!("Initial spot price is non-positive")); }
    let current_spot_price = Decimal::try_from(current_spot_price_f64)?;
    let total_sum_decimal = Decimal::try_from(request.sum)?;
    let volatility_decimal = Decimal::try_from(request.volatility)?;

    let denominator = (Decimal::ONE + volatility_decimal) * (Decimal::ONE + Decimal::try_from(maintenance_margin_rate)?);
    if denominator == Decimal::ZERO { return Err(anyhow!("Denominator for initial spot value calculation is zero")); }
    let initial_target_spot_value = total_sum_decimal / denominator;
    debug!(operation_id, %initial_target_spot_value, "Calculated initial target spot value");

    let ideal_gross_spot_quantity = initial_target_spot_value / current_spot_price;
    let fut_decimals = get_decimals_from_step(linear_info.lot_size_filter.qty_step.as_deref())?;
    let initial_target_futures_quantity = ideal_gross_spot_quantity.trunc_with_scale(fut_decimals);
    debug!(operation_id, %initial_target_futures_quantity, "Calculated initial target futures quantity (estimate)");

    // --- 2. Автоматический Расчет Чанков ---
    debug!(operation_id, status=?HedgerWsStatus::CalculatingChunks);
    let target_chunk_count = config.ws_auto_chunk_target_count;
    let min_spot_quantity = Decimal::from_str(&spot_info.lot_size_filter.min_order_qty)?;
    let min_futures_quantity = Decimal::from_str(&linear_info.lot_size_filter.min_order_qty)?;
    let spot_quantity_step = get_step_decimal(spot_info.lot_size_filter.base_precision.as_deref())?;
    let futures_quantity_step = get_step_decimal(linear_info.lot_size_filter.qty_step.as_deref())?;
    let min_spot_notional = spot_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());
    let min_futures_notional = linear_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());
    let current_futures_price_estimate = current_spot_price;

    let (final_chunk_count, chunk_spot_quantity, chunk_futures_quantity) =
        calculate_auto_chunk_parameters(
            initial_target_spot_value, initial_target_futures_quantity,
            current_spot_price, current_futures_price_estimate,
            target_chunk_count, min_spot_quantity, min_futures_quantity,
            spot_quantity_step, futures_quantity_step,
            min_spot_notional, min_futures_notional,
        )?;
    info!(operation_id, final_chunk_count, %chunk_spot_quantity, %chunk_futures_quantity, "Chunk parameters calculated");

    // --- 3. Расчет и Установка Плеча ---
    debug!(operation_id, status=?HedgerWsStatus::SettingLeverage);
    let estimated_total_futures_value = initial_target_futures_quantity * current_spot_price;
    let estimated_total_spot_value = initial_target_spot_value;
    let available_collateral = total_sum_decimal - estimated_total_spot_value;
    if available_collateral <= Decimal::ZERO { return Err(anyhow!("Estimated available collateral is non-positive ({})", available_collateral)); }
    let required_leverage_decimal = estimated_total_futures_value / available_collateral;
    let required_leverage = required_leverage_decimal.to_f64().unwrap_or(0.0);
    debug!(operation_id, required_leverage, "Calculated required leverage");

    if required_leverage < 1.0 { return Err(anyhow!("Calculated required leverage ({:.2}) is less than 1.0", required_leverage)); }
    if required_leverage > config.max_allowed_leverage { return Err(anyhow!("Required leverage {:.2}x exceeds max allowed {:.2}x", required_leverage, config.max_allowed_leverage)); }

    let leverage_to_set = (required_leverage * 100.0).round() / 100.0;
    info!(operation_id, leverage_to_set, %futures_symbol_name, "Setting leverage via REST...");
    exchange_rest.set_leverage(&futures_symbol_name, leverage_to_set).await.context("Failed to set leverage via REST API")?;
    info!(operation_id, "Leverage set successfully.");
    sleep(Duration::from_millis(500)).await;

    // --- 4. Создание начального состояния ---
    let spot_tick_size = Decimal::from_str(&spot_info.price_filter.tick_size)?;
    let futures_tick_size = Decimal::from_str(&linear_info.price_filter.tick_size)?;

    let mut state = HedgerWsState::new_hedge(
        operation_id, spot_symbol_name.clone(), futures_symbol_name.clone(),
        initial_target_spot_value, initial_target_futures_quantity
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
    state.status = HedgerWsStatus::StartingChunk(1);

    info!(operation_id, "HedgerWsHedgeTask initialized successfully. Ready to run.");

    // Собираем и возвращаем экземпляр задачи
    Ok(HedgerWsHedgeTask {
        operation_id,
        config,
        database,
        state,
        ws_receiver,
        exchange_rest,
        progress_callback,
    })
}