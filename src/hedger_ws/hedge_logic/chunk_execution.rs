// src/hedger_ws/hedge_logic/chunk_execution.rs

use anyhow::{anyhow, Result, Context}; // Добавили Context
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{debug, error, info, warn};

use crate::exchange::types::OrderSide;
use crate::hedger_ws::hedge_task::HedgerWsHedgeTask;
use crate::hedger_ws::state::{ChunkOrderState, HedgerWsStatus, Leg};
use crate::hedger_ws::hedge_logic::helpers::{calculate_limit_price_for_leg, round_down_step, get_current_price, send_progress_update}; // Используем хелперы

// Возвращает Ok(true), если чанк был пропущен, Ok(false) если запущен, Err при ошибке.
pub async fn start_next_chunk(task: &mut HedgerWsHedgeTask) -> Result<bool> {
    let chunk_index = match task.state.status {
         HedgerWsStatus::StartingChunk(index) => index,
         HedgerWsStatus::RunningChunk(index) => {
            warn!(operation_id = task.operation_id, chunk_index=index, "start_next_chunk called while chunk already running.");
            return Ok(false); // Чанк уже запущен, не пропускаем
        }
        _ => return Err(anyhow!("start_next_chunk (hedge) called in unexpected state: {:?}", task.state.status)),
    };
    info!(operation_id = task.operation_id, chunk_index, "Starting hedge chunk placement...");
    task.state.active_spot_order = None;
    task.state.active_futures_order = None;

    // --- Расчет объемов для текущего чанка (Hedge) ---
    let spot_quantity_chunk: Decimal;
    let futures_quantity_chunk: Decimal;
    let is_last_chunk = chunk_index == task.state.total_chunks;

    let current_spot_price_estimate = get_current_price(task, Leg::Spot)
        .ok_or_else(|| anyhow!("Cannot get current spot price estimate for chunk calculation"))?;

    let current_total_spot_target_qty = if current_spot_price_estimate > Decimal::ZERO {
        task.state.initial_target_spot_value / current_spot_price_estimate
    } else {
        warn!(operation_id = task.operation_id, "Current spot price estimate is zero, cannot calculate spot target quantity.");
        return Err(anyhow!("Current spot price estimate is zero"));
    };

    if is_last_chunk {
        spot_quantity_chunk = (current_total_spot_target_qty - task.state.cumulative_spot_filled_quantity).max(Decimal::ZERO);
        futures_quantity_chunk = (task.state.initial_target_futures_qty - task.state.cumulative_futures_filled_quantity).max(Decimal::ZERO);
        info!(operation_id=task.operation_id, chunk_index, "Calculating quantities for last hedge chunk.");
    } else {
        spot_quantity_chunk = task.state.chunk_base_quantity_spot;
        futures_quantity_chunk = task.state.chunk_base_quantity_futures;
    }
    debug!(operation_id=task.operation_id, chunk_index, %spot_quantity_chunk, %futures_quantity_chunk, "Calculated hedge chunk quantities.");

    let spot_quantity_rounded = round_down_step(task, spot_quantity_chunk, task.state.spot_quantity_step)?;
    let futures_quantity_rounded = round_down_step(task, futures_quantity_chunk, task.state.futures_quantity_step)?;

    let tolerance = dec!(1e-12);
    let place_spot = spot_quantity_rounded >= task.state.min_spot_quantity || spot_quantity_rounded < tolerance;
    let place_futures = futures_quantity_rounded >= task.state.min_futures_quantity || futures_quantity_rounded < tolerance;

    let spot_notional_ok = task.state.min_spot_notional.map_or(true, |min_val| {
         (spot_quantity_rounded * current_spot_price_estimate) >= min_val || spot_quantity_rounded < tolerance
    });
    let futures_price_estimate = get_current_price(task, Leg::Futures).unwrap_or(current_spot_price_estimate);
    let futures_notional_ok = task.state.min_futures_notional.map_or(true, |min_val| {
         (futures_quantity_rounded * futures_price_estimate) >= min_val || futures_quantity_rounded < tolerance
    });

    // --- Проверка на пропуск чанка ---
    if (!place_spot || !spot_notional_ok) && (!place_futures || !futures_notional_ok) {
        warn!(operation_id=task.operation_id, chunk_index, %spot_quantity_rounded, %futures_quantity_rounded,
              place_spot, spot_notional_ok, place_futures, futures_notional_ok,
              "Both legs quantities for chunk are below minimums or notionals. Skipping chunk.");
         if is_last_chunk {
            info!(operation_id=task.operation_id, chunk_index, "Skipping last chunk due to minimums, moving to reconcile.");
            task.state.status = HedgerWsStatus::Reconciling;
         } else {
             let next_chunk_index = chunk_index + 1;
             task.state.current_chunk_index = next_chunk_index;
             task.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
             info!(operation_id=task.operation_id, chunk_index, "Chunk skipped, setting status to StartChunk({}).", next_chunk_index);
         }
         return Ok(true); // Возвращаем true, указывая, что чанк пропущен
    }
    if !place_spot || !spot_notional_ok { warn!(operation_id=task.operation_id, chunk_index, %spot_quantity_rounded, "Spot leg quantity too small or below notional. Skipping spot placement."); }
    if !place_futures || !futures_notional_ok { warn!(operation_id=task.operation_id, chunk_index, %futures_quantity_rounded, "Futures leg quantity too small or below notional. Skipping futures placement."); }


    // --- Определение лимитных цен и выставление ордеров ---
    let spot_limit_price = if place_spot && spot_notional_ok { Some(calculate_limit_price_for_leg(task, Leg::Spot)?) } else { None };
    let futures_limit_price = if place_futures && futures_notional_ok { Some(calculate_limit_price_for_leg(task, Leg::Futures)?) } else { None };

    let mut placed_spot_order: Option<ChunkOrderState> = None;
    let mut placed_futures_order: Option<ChunkOrderState> = None;
    let mut spot_place_error: Option<anyhow::Error> = None;
    let mut futures_place_error: Option<anyhow::Error> = None;

    // 1. Выставляем ордер на СПОТ (Покупка)
    if let (true, true, Some(limit_price)) = (place_spot, spot_notional_ok, spot_limit_price) {
        task.state.status = HedgerWsStatus::PlacingSpotOrder(chunk_index);
        let qty_f64 = spot_quantity_rounded.round_dp(task.state.spot_quantity_step.scale()).to_f64().unwrap_or(0.0);
        let price_f64 = limit_price.round_dp(task.state.spot_tick_size.scale()).to_f64().unwrap_or(0.0);

        if qty_f64 > 0.0 && price_f64 > 0.0 {
            info!(operation_id=task.operation_id, chunk_index, %qty_f64, %price_f64, "Placing SPOT BUY order via REST");
            let spot_order_result = task.exchange_rest.place_limit_order(
                &task.state.symbol_spot, OrderSide::Buy, qty_f64, price_f64
            ).await;

            match spot_order_result {
                Ok(order) => {
                    info!(operation_id=task.operation_id, chunk_index, order_id=%order.id, "SPOT BUY order placed.");
                    // Создаем ChunkOrderState с реальными данными
                    placed_spot_order = Some(ChunkOrderState::new(
                        order.id, // ID из ответа биржи
                        task.state.symbol_spot.clone(),
                        OrderSide::Buy,
                        limit_price, // Рассчитанная цена
                        spot_quantity_rounded // Рассчитанный объем
                    ));
                }
                Err(e) => {
                    error!(operation_id=task.operation_id, chunk_index, %e, "Failed to place SPOT BUY order!");
                    spot_place_error = Some(e.context("Failed to place SPOT BUY order"));
                }
            };
        } else {
            spot_place_error = Some(anyhow!("Invalid SPOT order parameters (qty={}, price={})", qty_f64, price_f64));
        }
    }

    // 2. Выставляем ордер на ФЬЮЧЕРС (Продажа)
    if let (true, true, Some(limit_price)) = (place_futures, futures_notional_ok, futures_limit_price) {
        if spot_place_error.is_none() {
            task.state.status = HedgerWsStatus::PlacingFuturesOrder(chunk_index);
            let qty_f64 = futures_quantity_rounded.round_dp(task.state.futures_quantity_step.scale()).to_f64().unwrap_or(0.0);
            let price_f64 = limit_price.round_dp(task.state.futures_tick_size.scale()).to_f64().unwrap_or(0.0);

            if qty_f64 > 0.0 && price_f64 > 0.0 {
                info!(operation_id=task.operation_id, chunk_index, %qty_f64, %price_f64, "Placing FUTURES SELL order via REST");
                let futures_order_result = task.exchange_rest.place_futures_limit_order(
                    &task.state.symbol_futures, OrderSide::Sell, qty_f64, price_f64
                ).await;

                match futures_order_result {
                    Ok(order) => {
                        info!(operation_id=task.operation_id, chunk_index, order_id=%order.id, "FUTURES SELL order placed.");
                        // Создаем ChunkOrderState с реальными данными
                        placed_futures_order = Some(ChunkOrderState::new(
                            order.id, // ID из ответа биржи
                            task.state.symbol_futures.clone(),
                            OrderSide::Sell,
                            limit_price, // Рассчитанная цена
                            futures_quantity_rounded // Рассчитанный объем
                        ));
                    }
                    Err(e) => {
                        error!(operation_id=task.operation_id, chunk_index, %e, "Failed to place FUTURES SELL order!");
                        futures_place_error = Some(e.context("Failed to place FUTURES SELL order"));
                        // --- Откат спотового ордера ---
                        let mut spot_cancelled_in_rollback = false;
                        if let Some(ref spot_order_state) = placed_spot_order {
                            warn!(operation_id=task.operation_id, chunk_index, order_id=%spot_order_state.order_id, "Attempting to cancel SPOT order due to FUTURES placement failure.");
                            if let Err(cancel_err) = task.exchange_rest.cancel_spot_order(&task.state.symbol_spot, &spot_order_state.order_id).await {
                                error!(operation_id=task.operation_id, chunk_index, order_id=%spot_order_state.order_id, %cancel_err, "Failed to cancel SPOT order after FUTURES failure!");
                                futures_place_error = futures_place_error.map(|err| err.context(format!("Also failed to cancel spot: {}", cancel_err)));
                            } else {
                                spot_cancelled_in_rollback = true;
                                info!(operation_id=task.operation_id, chunk_index, order_id=%spot_order_state.order_id, "SPOT order cancelled successfully after FUTURES failure.");
                            }
                        }
                        if spot_cancelled_in_rollback {
                            placed_spot_order = None; // Обнуляем, если отменили
                        }
                        // --- Конец отката ---
                    }
                };
            } else {
                futures_place_error = Some(anyhow!("Invalid FUTURES order parameters (qty={}, price={})", qty_f64, price_f64));
                // --- Откат спотового ордера ---
                let mut spot_cancelled_in_rollback = false;
                if let Some(ref spot_order_state) = placed_spot_order {
                     warn!(operation_id=task.operation_id, chunk_index, order_id=%spot_order_state.order_id, "Attempting to cancel SPOT order due to FUTURES invalid params.");
                     if let Err(cancel_err) = task.exchange_rest.cancel_spot_order(&task.state.symbol_spot, &spot_order_state.order_id).await {
                          error!(operation_id=task.operation_id, chunk_index, order_id=%spot_order_state.order_id, %cancel_err, "Failed to cancel SPOT order!");
                     } else {
                         spot_cancelled_in_rollback = true;
                         info!(operation_id=task.operation_id, chunk_index, order_id=%spot_order_state.order_id, "SPOT order cancelled successfully after FUTURES invalid params.");
                    }
                }
                 if spot_cancelled_in_rollback { placed_spot_order = None; }
                 // --- Конец отката ---
            }
        } else {
             warn!(operation_id=task.operation_id, chunk_index, "Skipping FUTURES placement because SPOT placement failed.");
        }
    }

    // --- Обработка ошибок размещения ---
    if let Some(error) = spot_place_error.or(futures_place_error) {
        task.state.status = HedgerWsStatus::Failed(format!("Chunk {} placement error: {}", chunk_index, error));
        crate::hedger_ws::hedge_logic::helpers::update_final_db_status(task).await;
        return Err(error);
    }

    // --- Успешное размещение ---
    task.state.active_spot_order = placed_spot_order;
    task.state.active_futures_order = placed_futures_order;

    if task.state.active_spot_order.is_some() || task.state.active_futures_order.is_some() {
        task.state.current_chunk_index = chunk_index + 1; // Переходим к следующему
        task.state.status = HedgerWsStatus::RunningChunk(chunk_index);
        info!(operation_id = task.operation_id, chunk_index, "Hedge chunk placement finished. Status: RunningChunk");
        send_progress_update(task).await?; // Отправляем прогресс
        Ok(false) // Чанк запущен, не пропущен
    } else {
        // Сюда попадем, только если обе ноги были пропущены ИЗНАЧАЛЬНО (не из-за ошибки)
        warn!(operation_id=task.operation_id, chunk_index, "No orders were placed for this chunk (both legs skipped due to size/notional).");
        // Статус уже должен быть StartingChunk(next_chunk_index) после проверки на пропуск выше
        Ok(true) // Чанк пропущен
    }
}