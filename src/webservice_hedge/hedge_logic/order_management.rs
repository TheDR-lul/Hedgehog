// src/webservice_hedge/hedge_logic/order_management.rs

use anyhow::{anyhow, Result, Context};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{error, info, warn, trace};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::exchange::types::{OrderSide, OrderStatusText};
use crate::webservice_hedge::hedge_task::HedgerWsHedgeTask;
use crate::webservice_hedge::state::{ChunkOrderState, HedgerWsStatus, Leg};
use crate::webservice_hedge::hedge_logic::helpers::{calculate_limit_price_for_leg, round_down_step, update_final_db_status};

pub(super) async fn check_stale_orders(task: &mut HedgerWsHedgeTask) -> Result<()> {
    if !matches!(task.state.status, HedgerWsStatus::RunningChunk(_)) { return Ok(()); }

    if let Some(stale_ratio_f64) = task.config.ws_stale_price_ratio {
        if stale_ratio_f64 <= 0.0 { return Ok(()); }
        let stale_ratio_decimal = Decimal::try_from(stale_ratio_f64)?;
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;

        let spot_order_opt = task.state.active_spot_order.clone();
        if let Some(spot_order) = spot_order_opt {
            if matches!(spot_order.status, OrderStatusText::New | OrderStatusText::PartiallyFilled) {
                if let (Some(best_ask), Some(last_update_time)) = (task.state.spot_market_data.best_ask_price, task.state.spot_market_data.last_update_time_ms) {
                    if now_ms - last_update_time < 5000 {
                        if spot_order.limit_price < best_ask * (Decimal::ONE - stale_ratio_decimal) {
                            warn!(operation_id = task.operation_id, order_id = %spot_order.order_id, limit_price = %spot_order.limit_price, best_ask = %best_ask, "Spot order price is stale! Triggering replacement.");
                            initiate_order_replacement(task, Leg::Spot, "StalePrice".to_string()).await?;
                            return Ok(());
                        }
                    } else { trace!(operation_id = task.operation_id, "Spot market data is too old for stale check ({:.1}s)", (now_ms - last_update_time) as f64 / 1000.0); }
                }
            }
        }

        let futures_order_opt = task.state.active_futures_order.clone();
        if let Some(futures_order) = futures_order_opt {
            if matches!(futures_order.status, OrderStatusText::New | OrderStatusText::PartiallyFilled) {
                if let (Some(best_bid), Some(last_update_time)) = (task.state.futures_market_data.best_bid_price, task.state.futures_market_data.last_update_time_ms) {
                    if now_ms - last_update_time < 5000 {
                        if futures_order.limit_price > best_bid * (Decimal::ONE + stale_ratio_decimal) {
                            warn!(operation_id = task.operation_id, order_id = %futures_order.order_id, limit_price = %futures_order.limit_price, best_bid = %best_bid, "Futures order price is stale! Triggering replacement.");
                            initiate_order_replacement(task, Leg::Futures, "StalePrice".to_string()).await?;
                            return Ok(());
                        }
                    } else { trace!(operation_id = task.operation_id, "Futures market data is too old for stale check ({:.1}s)", (now_ms - last_update_time) as f64 / 1000.0); }
                }
            }
        }
    }
    Ok(())
}

pub(super) async fn initiate_order_replacement(task: &mut HedgerWsHedgeTask, leg_to_cancel: Leg, reason: String) -> Result<()> {
    let (order_to_cancel_opt, symbol_for_api_call, is_spot_leg) = match leg_to_cancel {
       Leg::Spot => {
           let base_symbol = task.state.symbol_spot.trim_end_matches(&task.config.quote_currency.to_uppercase()).to_string();
           if base_symbol.is_empty() || base_symbol == task.state.symbol_spot {
               error!("op_id:{}: Could not derive base symbol from spot_symbol '{}' for cancel in initiate_order_replacement.", task.operation_id, task.state.symbol_spot);
               return Err(anyhow!("Could not derive base symbol for spot cancel"));
           }
           (task.state.active_spot_order.as_ref(), base_symbol, true)
       },
       Leg::Futures => (task.state.active_futures_order.as_ref(), task.state.symbol_futures.clone(), false),
    };

    if let Some(order_to_cancel) = order_to_cancel_opt {
       if matches!(task.state.status, HedgerWsStatus::CancellingOrder{..} | HedgerWsStatus::WaitingCancelConfirmation{..}) {
           warn!(operation_id=task.operation_id, order_id=%order_to_cancel.order_id, current_status=?task.state.status, "Replacement already in progress. Skipping.");
           return Ok(());
       }
       info!(operation_id = task.operation_id, order_id = %order_to_cancel.order_id, ?leg_to_cancel, %reason, "Initiating order replacement: sending cancel request...");
       let current_chunk = match task.state.status { HedgerWsStatus::RunningChunk(idx) => idx, _ => task.state.current_chunk_index.saturating_sub(1).max(1) };
       let order_id_to_cancel_str = order_to_cancel.order_id.clone();

       task.state.status = HedgerWsStatus::CancellingOrder { chunk_index: current_chunk, leg_to_cancel, order_id_to_cancel: order_id_to_cancel_str.clone(), reason };

       let cancel_result = if is_spot_leg {
           task.exchange_rest.cancel_spot_order(&symbol_for_api_call, &order_id_to_cancel_str).await
       } else {
           task.exchange_rest.cancel_futures_order(&symbol_for_api_call, &order_id_to_cancel_str).await
       };

       if let Err(e) = cancel_result {
            error!(operation_id = task.operation_id, order_id = %order_id_to_cancel_str, %e, "Failed to send cancel request for order replacement");
            task.state.status = HedgerWsStatus::RunningChunk(current_chunk);
            return Err(e).context("Failed to send cancel request");
       } else {
            info!(operation_id = task.operation_id, order_id = %order_id_to_cancel_str, "Cancel request sent. Waiting for WebSocket confirmation...");
            task.state.status = HedgerWsStatus::WaitingCancelConfirmation { chunk_index: current_chunk, cancelled_leg: leg_to_cancel, cancelled_order_id: order_id_to_cancel_str };
       }
    } else {
       warn!(operation_id = task.operation_id, ?leg_to_cancel, "Attempted to replace order, but no active order found for leg.");
    }
    Ok(())
}

pub(super) async fn handle_cancel_confirmation(task: &mut HedgerWsHedgeTask, cancelled_order_id: &str, leg: Leg) -> Result<()> {
    info!(operation_id = task.operation_id, %cancelled_order_id, ?leg, "Handling cancel confirmation: placing replacement order if needed...");

    // Определяем, был ли это последний чанк, из состояния перед отменой
    let (current_chunk_idx_for_replacement, is_last_chunk_for_replacement) = match task.state.status {
        HedgerWsStatus::WaitingCancelConfirmation { chunk_index, .. } => (chunk_index, chunk_index == task.state.total_chunks),
        _ => { // Фоллбек, если статус неожиданно изменился
            let chunk_idx = task.state.current_chunk_index.saturating_sub(1).max(1);
            (chunk_idx, chunk_idx == task.state.total_chunks)
        }
    };

    // *** ИСПРАВЛЕНИЕ ЛОГИКИ РАСЧЕТА КОЛИЧЕСТВА ДЛЯ ЗАМЕНЯЮЩЕГО ОРДЕРА ***
    let quantity_for_replacement_order_calc: Decimal;

    if is_last_chunk_for_replacement {
        // Для последнего чанка, заменяющий ордер должен закрыть весь оставшийся объем по этой ноге для всей операции
        let total_target_for_leg_approx = match leg {
            Leg::Spot => {
                // Приблизительное общее целевое количество спота на основе начальной стоимости и текущей цены
                // Это может быть не идеально точным, если цена сильно изменилась, но лучше чем ничего.
                let spot_price = crate::webservice_hedge::hedge_logic::helpers::get_current_price(task, Leg::Spot)
                                   .unwrap_or(task.state.spot_market_data.best_ask_price.or(task.state.spot_market_data.best_bid_price).unwrap_or(Decimal::ONE)); // Используем хоть какую-то цену
                if spot_price > Decimal::ZERO { task.state.initial_target_spot_value / spot_price } else { task.state.initial_target_spot_value }
            }
            Leg::Futures => task.state.initial_target_futures_qty,
        };
        let cumulative_filled_for_leg = match leg {
            Leg::Spot => task.state.cumulative_spot_filled_quantity,
            Leg::Futures => task.state.cumulative_futures_filled_quantity,
        };
        quantity_for_replacement_order_calc = (total_target_for_leg_approx - cumulative_filled_for_leg).max(Decimal::ZERO);
        info!(operation_id=task.operation_id, ?leg, "Last chunk replacement: total_target_approx={}, cumulative_filled={}, calculated_replacement_qty_raw={}", total_target_for_leg_approx, cumulative_filled_for_leg, quantity_for_replacement_order_calc);
    } else {
        // Для не-последних чанков, заменяющий ордер должен быть на базовое количество этого чанка для данной ноги.
        // Это предполагает, что мы "перезапускаем" попытку для этого чанка.
        quantity_for_replacement_order_calc = match leg {
            Leg::Spot => task.state.chunk_base_quantity_spot,
            Leg::Futures => task.state.chunk_base_quantity_futures,
        };
        info!(operation_id=task.operation_id, ?leg, "Intermediate chunk replacement: using chunk_base_quantity={}", quantity_for_replacement_order_calc);
    }
    // *** КОНЕЦ ИСПРАВЛЕНИЯ ЛОГИКИ РАСЧЕТА КОЛИЧЕСТВА ***

    let step = match leg {
        Leg::Spot => task.state.spot_quantity_step,
        Leg::Futures => task.state.futures_quantity_step,
    };
    let min_quantity = match leg {
        Leg::Spot => task.state.min_spot_quantity,
        Leg::Futures => task.state.min_futures_quantity,
    };

    let remaining_quantity_rounded = round_down_step(task, quantity_for_replacement_order_calc, step)?;

    match leg {
        Leg::Spot => task.state.active_spot_order = None,
        Leg::Futures => task.state.active_futures_order = None,
    }
    info!(operation_id = task.operation_id, %cancelled_order_id, ?leg, "Cleared active order state after cancel confirmation.");

    let tolerance = dec!(1e-12); // Используем Decimal для толерантности
    if remaining_quantity_rounded < min_quantity && remaining_quantity_rounded.abs() > tolerance {
        warn!(operation_id=task.operation_id, %remaining_quantity_rounded, %min_quantity, ?leg, "Remaining quantity after cancel is dust. Not placing replacement order.");
    } else if remaining_quantity_rounded.abs() > tolerance { // Убедимся, что объем не нулевой
        info!(operation_id = task.operation_id, replacement_qty = %remaining_quantity_rounded, ?leg, "Placing replacement order...");
        let new_limit_price = calculate_limit_price_for_leg(task, leg)?;

        let (symbol_for_api_call, order_side_for_replacement, qty_precision, price_precision, actual_symbol_for_state, is_spot_leg_flag) = match leg {
            Leg::Spot => {
                let base_symbol = task.state.symbol_spot.trim_end_matches(&task.config.quote_currency.to_uppercase()).to_string();
                if base_symbol.is_empty() || base_symbol == task.state.symbol_spot {
                     error!("op_id:{}: Could not derive base symbol from spot_symbol '{}' for replacement order.", task.operation_id, task.state.symbol_spot);
                     return Err(anyhow!("Could not derive base symbol for spot replacement"));
                }
                (base_symbol, OrderSide::Buy, step.scale(), task.state.spot_tick_size.scale(), task.state.symbol_spot.clone(), true)
            }
            Leg::Futures => (task.state.symbol_futures.clone(), OrderSide::Sell, step.scale(), task.state.futures_tick_size.scale(), task.state.symbol_futures.clone(), false),
        };

        let quantity_f64 = remaining_quantity_rounded.round_dp(qty_precision).to_f64().unwrap_or(0.0);
        let price_f64 = new_limit_price.round_dp(price_precision).to_f64().unwrap_or(0.0);

        if quantity_f64 <= 0.0 || price_f64 <= 0.0 {
             error!(operation_id=task.operation_id, %quantity_f64, %price_f64, ?leg, "Invalid parameters for replacement order!");
             task.state.status = HedgerWsStatus::RunningChunk(current_chunk_idx_for_replacement);
             return Err(anyhow!("Invalid parameters for replacement order"));
        }

        let place_result = if is_spot_leg_flag {
             task.exchange_rest.place_limit_order(&symbol_for_api_call, order_side_for_replacement, quantity_f64, price_f64).await
        } else {
             task.exchange_rest.place_futures_limit_order(&symbol_for_api_call, order_side_for_replacement, quantity_f64, price_f64).await
        };

        match place_result {
            Ok(new_order) => {
                info!(operation_id=task.operation_id, new_order_id=%new_order.id, ?leg, "Replacement order placed successfully.");
                let new_order_state = ChunkOrderState::new(new_order.id, actual_symbol_for_state, order_side_for_replacement, new_limit_price, remaining_quantity_rounded);
                match leg {
                    Leg::Spot => task.state.active_spot_order = Some(new_order_state),
                    Leg::Futures => task.state.active_futures_order = Some(new_order_state)
                }
            }
            Err(e) => {
                let error_msg = format!("Failed to place replacement {:?} order: {}", leg, e);
                error!(operation_id = task.operation_id, error=%error_msg, ?leg);
                task.state.status = HedgerWsStatus::Failed(error_msg.clone());
                update_final_db_status(task).await;
                return Err(anyhow!(error_msg).context("Failed to place replacement order"));
            }
        }
    } else {
        info!(operation_id=task.operation_id, ?leg, "No remaining quantity after cancel confirmation or quantity is zero. Not placing replacement.");
    }

    task.state.status = HedgerWsStatus::RunningChunk(current_chunk_idx_for_replacement);
    Ok(())
}