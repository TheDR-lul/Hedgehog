// src/webservice_hedge/hedge_logic/chunk_execution.rs

use anyhow::{anyhow, Result, Context};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{debug, error, info, warn};

use crate::exchange::types::OrderSide;
use crate::webservice_hedge::hedge_task::HedgerWsHedgeTask;
use crate::webservice_hedge::state::{ChunkOrderState, HedgerWsStatus, Leg};
use crate::webservice_hedge::hedge_logic::helpers::{calculate_limit_price_for_leg, round_down_step, get_current_price, send_progress_update};
use crate::hedger::ORDER_FILL_TOLERANCE; // Для сравнения с нулем

pub(crate) async fn start_next_chunk(task: &mut HedgerWsHedgeTask) -> Result<bool> { // bool: true если чанк был пропущен
    let chunk_index = match task.state.status {
         HedgerWsStatus::StartingChunk(index) => index,
         HedgerWsStatus::RunningChunk(index) => {
            warn!(operation_id = task.operation_id, chunk_index=index, "start_next_chunk called while chunk already running.");
            return Ok(false);
        }
        _ => return Err(anyhow!("start_next_chunk (hedge) called in unexpected state: {:?}", task.state.status)),
    };
    info!(operation_id = task.operation_id, chunk_index, "Starting hedge chunk placement...");
    task.state.active_spot_order = None;
    task.state.active_futures_order = None;

    let spot_quantity_chunk: Decimal;
    let futures_quantity_chunk: Decimal;
    let is_last_chunk = chunk_index == task.state.total_chunks;

    // --- ИСПРАВЛЕНО: Расчет оставшегося количества до общей цели ---
    // overall_target_spot_btc_qty теперь есть в стейте
    let remaining_spot_to_target = (task.state.overall_target_spot_btc_qty - task.state.cumulative_spot_filled_quantity).max(Decimal::ZERO);
    // initial_target_futures_qty в стейте УЖЕ является общим количеством BTC для фьючерсов
    let remaining_futures_to_target = (task.state.initial_target_futures_qty.abs() - task.state.cumulative_futures_filled_quantity.abs()).max(Decimal::ZERO);

    if is_last_chunk {
        spot_quantity_chunk = remaining_spot_to_target;
        futures_quantity_chunk = remaining_futures_to_target;
        info!(operation_id=task.operation_id, chunk_index, "Calculating quantities for LAST hedge chunk. Spot rem: {:.8}, Fut rem: {:.8}", spot_quantity_chunk, futures_quantity_chunk);
    } else {
        // Для не-последних чанков, берем базовый размер чанка, НО не больше, чем осталось до общей цели
        spot_quantity_chunk = task.state.chunk_base_quantity_spot.min(remaining_spot_to_target);
        futures_quantity_chunk = task.state.chunk_base_quantity_futures.min(remaining_futures_to_target);
        info!(operation_id=task.operation_id, chunk_index,
            "Calculating quantities for INTERMEDIATE hedge chunk. BaseSpot: {:.8}, RemSpot: {:.8}, ChosenSpot: {:.8}. BaseFut: {:.8}, RemFut: {:.8}, ChosenFut: {:.8}",
            task.state.chunk_base_quantity_spot, remaining_spot_to_target, spot_quantity_chunk,
            task.state.chunk_base_quantity_futures, remaining_futures_to_target, futures_quantity_chunk
        );
    }
    // --- КОНЕЦ ИСПРАВЛЕНИЯ ---

    debug!(operation_id=task.operation_id, chunk_index, %spot_quantity_chunk, %futures_quantity_chunk, "Calculated hedge chunk quantities to place.");

    let spot_quantity_rounded = round_down_step(task, spot_quantity_chunk, task.state.spot_quantity_step)?;
    let futures_quantity_rounded = round_down_step(task, futures_quantity_chunk, task.state.futures_quantity_step)?;

    debug!(operation_id=task.operation_id, chunk_index, %spot_quantity_rounded, %futures_quantity_rounded, "Rounded hedge chunk quantities.");

    // Используем Decimal для толерантности
    let order_fill_tolerance_decimal = Decimal::from_f64(ORDER_FILL_TOLERANCE).unwrap_or_else(|| dec!(1e-8));

    let place_spot = spot_quantity_rounded >= task.state.min_spot_quantity || (spot_quantity_rounded.abs() < order_fill_tolerance_decimal && spot_quantity_chunk.abs() < order_fill_tolerance_decimal);
    let place_futures = futures_quantity_rounded >= task.state.min_futures_quantity || (futures_quantity_rounded.abs() < order_fill_tolerance_decimal && futures_quantity_chunk.abs() < order_fill_tolerance_decimal);

    let current_spot_price_estimate = get_current_price(task, Leg::Spot)
        .ok_or_else(|| anyhow!("Cannot get current spot price estimate for chunk notional calculation"))?;

    let spot_notional_ok = if place_spot && spot_quantity_rounded.abs() > order_fill_tolerance_decimal {
        task.state.min_spot_notional.map_or(true, |min_val| {
            (spot_quantity_rounded * current_spot_price_estimate) >= min_val
        })
    } else {
        true // Если не размещаем или количество нулевое, то нотионал ОК
    };

    let futures_price_estimate = get_current_price(task, Leg::Futures).unwrap_or(current_spot_price_estimate);
    let futures_notional_ok = if place_futures && futures_quantity_rounded.abs() > order_fill_tolerance_decimal {
         task.state.min_futures_notional.map_or(true, |min_val| {
            (futures_quantity_rounded * futures_price_estimate) >= min_val
        })
    } else {
        true // Если не размещаем или количество нулевое, то нотионал ОК
    };


    if (!place_spot || !spot_notional_ok) && (!place_futures || !futures_notional_ok) {
        warn!(operation_id=task.operation_id, chunk_index, %spot_quantity_rounded, %futures_quantity_rounded,
              place_spot, spot_notional_ok, place_futures, futures_notional_ok,
              "Both legs quantities for chunk are effectively zero or below minimums/notionals. Skipping chunk.");
         if is_last_chunk {
            info!(operation_id=task.operation_id, chunk_index, "Skipping LAST chunk due to minimums, moving to reconcile.");
            task.state.status = HedgerWsStatus::Reconciling;
         } else {
             let next_chunk_index = chunk_index + 1;
             // Проверяем, не превысили ли мы уже общее количество чанков
             if next_chunk_index <= task.state.total_chunks {
                task.state.current_chunk_index = next_chunk_index;
                task.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                info!(operation_id=task.operation_id, chunk_index, "Chunk skipped, setting status to StartChunk({}).", next_chunk_index);
             } else {
                 info!(operation_id=task.operation_id, chunk_index, "All planned chunks processed or skipped, moving to reconcile.");
                 task.state.status = HedgerWsStatus::Reconciling;
             }
         }
         return Ok(true); // Чанк пропущен
    }

    if !place_spot || !spot_notional_ok { warn!(operation_id=task.operation_id, chunk_index, %spot_quantity_rounded, spot_qty_target_for_chunk=%spot_quantity_chunk, "Spot leg quantity too small or below notional. Skipping spot placement."); }
    if !place_futures || !futures_notional_ok { warn!(operation_id=task.operation_id, chunk_index, %futures_quantity_rounded, fut_qty_target_for_chunk=%futures_quantity_chunk, "Futures leg quantity too small or below notional. Skipping futures placement."); }

    // ... остальная часть функции start_next_chunk остается такой же, как в предыдущей версии ...
    // (размещение ордеров, обработка ошибок и т.д.)
    // Важно, что теперь spot_quantity_rounded и futures_quantity_rounded будут корректными
    // для текущего чанка с учетом общего прогресса.

    let spot_limit_price = if place_spot && spot_notional_ok && spot_quantity_rounded.abs() > order_fill_tolerance_decimal {
        Some(calculate_limit_price_for_leg(task, Leg::Spot)?)
    } else { None };

    let futures_limit_price = if place_futures && futures_notional_ok && futures_quantity_rounded.abs() > order_fill_tolerance_decimal {
        Some(calculate_limit_price_for_leg(task, Leg::Futures)?)
    } else { None };

    let mut placed_spot_order: Option<ChunkOrderState> = None;
    let mut placed_futures_order: Option<ChunkOrderState> = None;
    let mut spot_place_error: Option<anyhow::Error> = None;
    let mut futures_place_error: Option<anyhow::Error> = None;

    // Логика размещения ордеров (сначала спот, потом фьючерс)
    if place_spot && spot_notional_ok && spot_quantity_rounded.abs() > order_fill_tolerance_decimal {
        if let Some(limit_price) = spot_limit_price {
            task.state.status = HedgerWsStatus::PlacingSpotOrder(chunk_index);
            let qty_f64 = spot_quantity_rounded.round_dp(task.state.spot_quantity_step.scale()).to_f64().unwrap_or(0.0);
            let price_f64 = limit_price.round_dp(task.state.spot_tick_size.scale()).to_f64().unwrap_or(0.0);

            if qty_f64 > 0.0 && price_f64 > 0.0 {
                let base_spot_symbol_str = task.state.symbol_spot.trim_end_matches(&task.config.quote_currency.to_uppercase());
                if base_spot_symbol_str.is_empty() || base_spot_symbol_str == task.state.symbol_spot {
                    spot_place_error = Some(anyhow!("Could not derive base symbol from spot_symbol: {}", task.state.symbol_spot));
                } else {
                    info!(operation_id=task.operation_id, chunk_index, base_symbol=%base_spot_symbol_str, %qty_f64, %price_f64, "Placing SPOT BUY order via REST");
                    let spot_order_result = task.exchange_rest.place_limit_order(
                        base_spot_symbol_str,
                        OrderSide::Buy,
                        qty_f64,
                        price_f64
                    ).await;
                    match spot_order_result {
                        Ok(order) => {
                            info!(operation_id=task.operation_id, chunk_index, order_id=%order.id, "SPOT BUY order placed.");
                            placed_spot_order = Some(ChunkOrderState::new(
                                order.id,
                                task.state.symbol_spot.clone(),
                                OrderSide::Buy,
                                limit_price, // Используем Decimal limit_price
                                spot_quantity_rounded // Используем Decimal quantity
                            ));
                        }
                        Err(e) => {
                            error!(operation_id=task.operation_id, chunk_index, error_message=%e, "Failed to place SPOT BUY order!");
                            spot_place_error = Some(e.context("Failed to place SPOT BUY order"));
                        }
                    };
                }
            } else {
                spot_place_error = Some(anyhow!("Invalid SPOT order parameters (qty={}, price={}) for chunk {}", qty_f64, price_f64, chunk_index));
            }
        }
    }


    if place_futures && futures_notional_ok && futures_quantity_rounded.abs() > order_fill_tolerance_decimal {
        if spot_place_error.is_none() { // Продолжаем с фьючерсом, только если спот ОК (или не размещался)
            if let Some(limit_price) = futures_limit_price {
                task.state.status = HedgerWsStatus::PlacingFuturesOrder(chunk_index);
                let qty_f64 = futures_quantity_rounded.round_dp(task.state.futures_quantity_step.scale()).to_f64().unwrap_or(0.0);
                let price_f64 = limit_price.round_dp(task.state.futures_tick_size.scale()).to_f64().unwrap_or(0.0);

                if qty_f64 > 0.0 && price_f64 > 0.0 {
                    info!(operation_id=task.operation_id, chunk_index, %qty_f64, %price_f64, "Placing FUTURES SELL order via REST");
                    let futures_order_result = task.exchange_rest.place_futures_limit_order(
                        &task.state.symbol_futures,
                        OrderSide::Sell,
                        qty_f64,
                        price_f64
                    ).await;

                    match futures_order_result {
                        Ok(order) => {
                            info!(operation_id=task.operation_id, chunk_index, order_id=%order.id, "FUTURES SELL order placed.");
                            placed_futures_order = Some(ChunkOrderState::new(
                                order.id,
                                task.state.symbol_futures.clone(),
                                OrderSide::Sell,
                                limit_price, // Decimal
                                futures_quantity_rounded // Decimal
                            ));
                        }
                        Err(e) => {
                            error!(operation_id=task.operation_id, chunk_index, error_message=%e, "Failed to place FUTURES SELL order!");
                            futures_place_error = Some(e.context("Failed to place FUTURES SELL order"));
                            // Если спот был успешно размещен, а фьючерс нет - откатываем спот
                            if let Some(ref spot_order_state) = placed_spot_order {
                                warn!(operation_id=task.operation_id, chunk_index, order_id=%spot_order_state.order_id, "Attempting to cancel SPOT order due to FUTURES placement failure.");
                                let base_spot_symbol_for_cancel = task.state.symbol_spot.trim_end_matches(&task.config.quote_currency.to_uppercase());
                                if !base_spot_symbol_for_cancel.is_empty() && base_spot_symbol_for_cancel != task.state.symbol_spot {
                                    if let Err(cancel_err) = task.exchange_rest.cancel_spot_order(base_spot_symbol_for_cancel, &spot_order_state.order_id).await {
                                        error!(operation_id=task.operation_id, chunk_index, order_id=%spot_order_state.order_id, %cancel_err, "Failed to cancel SPOT order after FUTURES failure!");
                                        futures_place_error = futures_place_error.map(|err| err.context(format!("Also failed to cancel spot: {}", cancel_err)));
                                    } else {
                                        info!(operation_id=task.operation_id, chunk_index, order_id=%spot_order_state.order_id, "SPOT order cancelled successfully after FUTURES failure.");
                                        // Обнуляем, т.к. он отменен
                                        placed_spot_order = None;
                                    }
                                } else {
                                     error!("op_id:{}: Could not derive base symbol from spot_symbol '{}' for cancel during rollback.", task.operation_id, task.state.symbol_spot);
                                }
                            }
                        }
                    };
                } else {
                    futures_place_error = Some(anyhow!("Invalid FUTURES order parameters (qty={}, price={}) for chunk {}", qty_f64, price_f64, chunk_index));
                    // По аналогии, если спот был размещен, а параметры фьюча невалидны
                    if let Some(ref spot_order_state) = placed_spot_order {
                         warn!(operation_id=task.operation_id, chunk_index, order_id=%spot_order_state.order_id, "Attempting to cancel SPOT order due to FUTURES invalid params.");
                         let base_spot_symbol_for_cancel = task.state.symbol_spot.trim_end_matches(&task.config.quote_currency.to_uppercase());
                         if !base_spot_symbol_for_cancel.is_empty() && base_spot_symbol_for_cancel != task.state.symbol_spot {
                            if let Err(cancel_err) = task.exchange_rest.cancel_spot_order(base_spot_symbol_for_cancel, &spot_order_state.order_id).await {
                                 error!("op_id:{}: Failed to cancel SPOT order after FUTURES invalid params: {}", task.operation_id, cancel_err);
                            } else {
                                 info!("op_id:{}: SPOT order cancelled successfully after FUTURES invalid params.", task.operation_id);
                                 placed_spot_order = None;
                            }
                        }  else {
                             error!("op_id:{}: Could not derive base symbol from spot_symbol '{}' for cancel during rollback (invalid fut params).", task.operation_id, task.state.symbol_spot);
                        }
                    }
                }
            }
        } else if place_futures && futures_notional_ok && futures_quantity_rounded.abs() > order_fill_tolerance_decimal {
             // Эта ветка означает, что фьючерс нужно было размещать, но не стали из-за ошибки спота.
             warn!(operation_id=task.operation_id, chunk_index, "Skipping FUTURES placement because SPOT placement failed or was skipped.");
        }
    }


    if let Some(error) = spot_place_error.or(futures_place_error) {
        task.state.status = HedgerWsStatus::Failed(format!("Chunk {} placement error: {}", chunk_index, error));
        crate::webservice_hedge::hedge_logic::helpers::update_final_db_status(task).await;
        return Err(error);
    }

    task.state.active_spot_order = placed_spot_order;
    task.state.active_futures_order = placed_futures_order;

    if task.state.active_spot_order.is_some() || task.state.active_futures_order.is_some() {
        // current_chunk_index инкрементируется после успешного завершения чанка,
        // а здесь мы устанавливаем RunningChunk для ТЕКУЩЕГО chunk_index.
        // Инкремент current_chunk_index происходит в основном цикле HedgerWsHedgeTask::run
        // после того, как check_chunk_completion вернет true.
        task.state.status = HedgerWsStatus::RunningChunk(chunk_index);
        info!(operation_id = task.operation_id, chunk_index, "Hedge chunk placement finished. Status: RunningChunk");
        send_progress_update(task).await?;
        Ok(false) // Чанк не был пропущен, ордера размещены (или один из них)
    } else {
        warn!(operation_id=task.operation_id, chunk_index, "No orders were placed for this chunk (both legs skipped or resulted in zero quantity).");
        // Если нет активных ордеров, значит, чанк по сути пропущен.
        // Логика перехода к следующему чанку или реконсиляции обработается в основном цикле HedgerWsHedgeTask::run
        // на основе того, что check_chunk_completion вернет true (т.к. оба active_order None).
        // Здесь важно вернуть true, чтобы основной цикл понял, что чанк был "пропущен" без размещения ордеров.
        Ok(true)
    }
}