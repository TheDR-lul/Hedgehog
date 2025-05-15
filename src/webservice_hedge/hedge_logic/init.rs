// src/webservice_hedge/hedge_logic/init.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::{debug, info, warn, error}; // error импортирован
use std::str::FromStr;

use crate::config::Config;
use crate::exchange::Exchange;
use crate::models::HedgeRequest;
use crate::storage; // Не используем _database напрямую, но может понадобиться для других вызовов
use crate::webservice_hedge::state::{HedgerWsState};
use crate::webservice_hedge::common::calculate_auto_chunk_parameters;
use crate::webservice_hedge::hedge_logic::helpers::{get_step_decimal};
use crate::hedger::params::calculate_hedge_params_impl; // Публичная функция для расчета

pub async fn create_initial_hedger_ws_state(
    operation_id: i64,
    request: HedgeRequest,
    config: Arc<Config>,
    exchange_rest: Arc<dyn Exchange>, // Arc<dyn Exchange>
    _database: Arc<storage::Db>,      // _database пока не используется здесь напрямую
) -> Result<HedgerWsState> {
    info!(operation_id, "Creating initial HedgerWsState for operation...");

    let base_symbol = request.symbol.to_uppercase();
    let spot_symbol_name = format!("{}{}", base_symbol, config.quote_currency);
    let futures_symbol_name = format!("{}{}", base_symbol, config.quote_currency);

    // 1. Рассчитываем HedgeParams один раз, чтобы получить ОБЩИЕ целевые BTC количества
    //    и другие стабильные параметры хеджа.
    //    Используем exchange_rest.as_ref(), который имеет тип &(dyn Exchange + Send + Sync)
    let hedge_params = match calculate_hedge_params_impl(
        exchange_rest.as_ref(), // Передаем &(dyn Exchange + Send + Sync)
        &request,
        config.slippage,
        &config.quote_currency,
        config.max_allowed_leverage,
    ).await {
        Ok(params) => params,
        Err(e) => {
            error!(operation_id, "Failed to calculate initial HedgeParams: {}", e);
            return Err(e).context("Failed to calculate HedgeParams for state init (direct call)");
        }
    };
    info!(operation_id, ?hedge_params, "Initial HedgeParams calculated for WS State init (direct call)");

    let overall_target_spot_btc = Decimal::from_f64(hedge_params.spot_order_qty)
        .ok_or_else(|| anyhow!("Failed to convert hedge_params.spot_order_qty ({}) to Decimal", hedge_params.spot_order_qty))?;
    let overall_target_futures_btc = Decimal::from_f64(hedge_params.fut_order_qty)
        .ok_or_else(|| anyhow!("Failed to convert hedge_params.fut_order_qty ({}) to Decimal", hedge_params.fut_order_qty))?;

    // 2. Получаем актуальные данные по инструментам и ценам для расчета чанков
    debug!(operation_id, %spot_symbol_name, %futures_symbol_name, "Fetching instrument info and current prices for HedgerWsState chunk calculation...");
    let (
        spot_info_res,
        linear_info_res,
        spot_price_res,
        futures_price_res_opt
    ) = tokio::join!(
        exchange_rest.get_spot_instrument_info(&base_symbol),
        exchange_rest.get_linear_instrument_info(&base_symbol),
        exchange_rest.get_spot_price(&base_symbol),
        exchange_rest.get_market_price(&futures_symbol_name, false)
    );

    let spot_info = spot_info_res.context("Failed to get SPOT instrument info for state init")?;
    let linear_info = linear_info_res.context("Failed to get LINEAR instrument info for state init")?;
    let current_spot_price_f64 = spot_price_res.context("Failed to get current SPOT price for state init")?;
    if current_spot_price_f64 <= 0.0 { return Err(anyhow!("Current spot price is non-positive for state init")); }
    let current_spot_price = Decimal::try_from(current_spot_price_f64)?;

    let current_futures_price_estimate = match futures_price_res_opt {
        Ok(price) if price > 0.0 => Decimal::try_from(price)?,
        Ok(p) => {
            warn!(operation_id, futures_price_from_api=p, "Received non-positive futures price estimate, using current spot price.");
            current_spot_price
        }
        Err(e) => {
            warn!(operation_id, error=%e, "Could not get futures price estimate, using current spot price.");
            current_spot_price
        }
    };

    let target_chunk_count = config.ws_auto_chunk_target_count;
    let min_spot_quantity = Decimal::from_str(&spot_info.lot_size_filter.min_order_qty)?;
    let min_futures_quantity = Decimal::from_str(&linear_info.lot_size_filter.min_order_qty)?;
    let spot_quantity_step = get_step_decimal(spot_info.lot_size_filter.base_precision.as_deref())?;
    let futures_quantity_step = get_step_decimal(linear_info.lot_size_filter.qty_step.as_deref())?;
    let min_spot_notional = spot_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());
    let min_futures_notional = linear_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());

    debug!(
        operation_id,
        overall_target_spot_btc_param = %overall_target_spot_btc,
        overall_target_futures_btc_param = %overall_target_futures_btc,
        current_spot_price_param = %current_spot_price,
        current_futures_price_estimate_param = %current_futures_price_estimate,
        "Parameters being passed to calculate_auto_chunk_parameters"
    );

    let (final_chunk_count, chunk_spot_quantity, chunk_futures_quantity) =
        calculate_auto_chunk_parameters(
            overall_target_spot_btc,
            overall_target_futures_btc,
            current_spot_price,
            current_futures_price_estimate,
            target_chunk_count,
            min_spot_quantity,
            min_futures_quantity,
            spot_quantity_step,
            futures_quantity_step,
            min_spot_notional,
            min_futures_notional,
        )?;
    info!(operation_id, final_chunk_count, %chunk_spot_quantity, %chunk_futures_quantity, "Chunk parameters calculated for state init");

    let spot_tick_size = Decimal::from_str(&spot_info.price_filter.tick_size)?;
    let futures_tick_size = Decimal::from_str(&linear_info.price_filter.tick_size)?;

    let initial_user_sum_decimal = Decimal::try_from(request.sum)?;
    let initial_target_spot_value_for_state = Decimal::try_from(hedge_params.spot_value)?;


    let mut state = HedgerWsState::new_hedge(
        operation_id,
        spot_symbol_name.clone(),
        futures_symbol_name.clone(),
        initial_target_spot_value_for_state,
        overall_target_futures_btc,
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