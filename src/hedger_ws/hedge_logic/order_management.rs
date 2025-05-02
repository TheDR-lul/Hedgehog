// src/hedger_ws/hedge_logic/order_management.rs

use anyhow::{anyhow, Result, Context};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
// --- ИСПРАВЛЕНО: Добавляем trace ---
use tracing::{error, info, warn, trace}; // Добавили trace
use std::time::{SystemTime, UNIX_EPOCH};

use crate::exchange::types::{OrderSide, OrderStatusText};
use crate::hedger_ws::hedge_task::HedgerWsHedgeTask;
use crate::hedger_ws::state::{ChunkOrderState, HedgerWsStatus, Leg};
use crate::hedger_ws::hedge_logic::helpers::{calculate_limit_price_for_leg, round_down_step};

// Проверка активных ордеров на "устаревание" цены
pub async fn check_stale_orders(task: &mut HedgerWsHedgeTask) -> Result<()> {
    if !matches!(task.state.status, HedgerWsStatus::RunningChunk(_)) { return Ok(()); }

    if let Some(stale_ratio_f64) = task.config.ws_stale_price_ratio {
        if stale_ratio_f64 <= 0.0 { return Ok(()); }
        let stale_ratio_decimal = Decimal::try_from(stale_ratio_f64)?;
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;

        // Проверка ордера на СПОТ (Buy)
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
                    // --- ИСПРАВЛЕНО: Убираем плейсхолдер ---
                    } else { trace!(operation_id = task.operation_id, "Spot market data is too old for stale check ({:.1}s)", (now_ms - last_update_time) as f64 / 1000.0); }
                }
            }
        }

        // Проверка ордера на ФЬЮЧЕРС (Sell)
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
                    // --- ИСПРАВЛЕНО: Убираем плейсхолдер ---
                    } else { trace!(operation_id = task.operation_id, "Futures market data is too old for stale check ({:.1}s)", (now_ms - last_update_time) as f64 / 1000.0); }
                }
            }
        }
    }
    Ok(())
}

// Инициирование замены ордера (отправка команды cancel)
pub async fn initiate_order_replacement(task: &mut HedgerWsHedgeTask, leg_to_cancel: Leg, reason: String) -> Result<()> {
    // ... (код функции без изменений) ...
    let (order_to_cancel_opt, symbol, is_spot) = match leg_to_cancel {
       Leg::Spot => (task.state.active_spot_order.as_ref(), &task.state.symbol_spot, true),
       Leg::Futures => (task.state.active_futures_order.as_ref(), &task.state.symbol_futures, false),
    };

    if let Some(order_to_cancel) = order_to_cancel_opt {
       if matches!(task.state.status, HedgerWsStatus::CancellingOrder{..} | HedgerWsStatus::WaitingCancelConfirmation{..}) {
           warn!(operation_id=task.operation_id, order_id=%order_to_cancel.order_id, current_status=?task.state.status, "Replacement already in progress. Skipping.");
           return Ok(());
       }
       info!(operation_id = task.operation_id, order_id = %order_to_cancel.order_id, ?leg_to_cancel, %reason, "Initiating order replacement: sending cancel request...");
       let current_chunk = match task.state.status { HedgerWsStatus::RunningChunk(idx) => idx, _ => task.state.current_chunk_index };
       let order_id_to_cancel_str = order_to_cancel.order_id.clone();

       task.state.status = HedgerWsStatus::CancellingOrder { chunk_index: current_chunk, leg_to_cancel, order_id_to_cancel: order_id_to_cancel_str.clone(), reason };

       let cancel_result = if is_spot { task.exchange_rest.cancel_spot_order(symbol, &order_id_to_cancel_str).await }
                         else { task.exchange_rest.cancel_futures_order(symbol, &order_id_to_cancel_str).await };

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

// Обработка подтверждения отмены и выставление нового ордера
pub async fn handle_cancel_confirmation(task: &mut HedgerWsHedgeTask, cancelled_order_id: &str, leg: Leg) -> Result<()> {
     info!(operation_id = task.operation_id, %cancelled_order_id, ?leg, "Handling cancel confirmation: placing replacement order if needed...");

     let (total_target_qty_approx, filled_qty, min_quantity, step, is_spot) = match leg {
         Leg::Spot => {
             let price = super::helpers::get_current_price(task, Leg::Spot).unwrap_or(Decimal::ONE);
             let target_approx = if price > Decimal::ZERO { task.state.initial_target_spot_value / price } else { Decimal::MAX };
             (target_approx, task.state.cumulative_spot_filled_quantity, task.state.min_spot_quantity, task.state.spot_quantity_step, true)
         }
         Leg::Futures => (task.state.initial_target_futures_qty, task.state.cumulative_futures_filled_quantity, task.state.min_futures_quantity, task.state.futures_quantity_step, false),
     };

      let remaining_quantity = (total_target_qty_approx - filled_qty).max(Decimal::ZERO);
      let remaining_quantity_rounded = round_down_step(task, remaining_quantity, step)?;

      match leg {
          Leg::Spot => task.state.active_spot_order = None,
          Leg::Futures => task.state.active_futures_order = None,
      }
      info!(operation_id = task.operation_id, %cancelled_order_id, ?leg, "Cleared active order state after cancel confirmation.");

      let tolerance = dec!(1e-12);
      if remaining_quantity_rounded < min_quantity && remaining_quantity_rounded.abs() > tolerance {
            // --- ИСПРАВЛЕНО: Убираем плейсхолдер ---
           warn!(operation_id=task.operation_id, %remaining_quantity_rounded, %min_quantity, ?leg, "Remaining quantity after cancel is dust. Not placing replacement order.");
      }
      else if remaining_quantity_rounded > tolerance {
          info!(operation_id = task.operation_id, %remaining_quantity_rounded, ?leg, "Placing replacement order...");
          let new_limit_price = calculate_limit_price_for_leg(task, leg)?;

          let (symbol, side, qty_precision, price_precision) = match leg {
              Leg::Spot => (&task.state.symbol_spot, OrderSide::Buy, step.scale(), task.state.spot_tick_size.scale()),
              Leg::Futures => (&task.state.symbol_futures, OrderSide::Sell, step.scale(), task.state.futures_tick_size.scale()),
          };

          let quantity_f64 = remaining_quantity_rounded.round_dp(qty_precision).to_f64().unwrap_or(0.0);
          let price_f64 = new_limit_price.round_dp(price_precision).to_f64().unwrap_or(0.0);

          if quantity_f64 <= 0.0 || price_f64 <= 0.0 {
               // --- ИСПРАВЛЕНО: Убираем плейсхолдер ---
               error!(operation_id=task.operation_id, %quantity_f64, %price_f64, ?leg, "Invalid parameters for replacement order!");
               let current_chunk = match task.state.status { HedgerWsStatus::WaitingCancelConfirmation { chunk_index, .. } => chunk_index, _ => task.state.current_chunk_index };
               task.state.status = HedgerWsStatus::RunningChunk(current_chunk);
               return Err(anyhow!("Invalid parameters for replacement order"));
          }

           let place_result = if is_spot { task.exchange_rest.place_limit_order(symbol, side, quantity_f64, price_f64).await }
                             else { task.exchange_rest.place_futures_limit_order(symbol, side, quantity_f64, price_f64).await };

           match place_result {
               Ok(new_order) => {
                   // --- ИСПРАВЛЕНО: Убираем плейсхолдер ---
                   info!(operation_id=task.operation_id, new_order_id=%new_order.id, ?leg, "Replacement order placed successfully.");
                   let new_order_state = ChunkOrderState::new(new_order.id, symbol.to_string(), side, new_limit_price, remaining_quantity_rounded);
                   match leg {
                       Leg::Spot => task.state.active_spot_order = Some(new_order_state),
                       Leg::Futures => task.state.active_futures_order = Some(new_order_state)
                   }
               }
               Err(e) => {
                    // --- ИСПРАВЛЕНО: Убираем плейсхолдеры и добавляем аргумент в Failed ---
                    let error_msg = format!("Failed to place replacement {:?} order: {}", leg, e);
                    error!(operation_id = task.operation_id, error=%error_msg, ?leg);
                    task.state.status = HedgerWsStatus::Failed(error_msg); // <-- Добавляем строку ошибки
                    super::helpers::update_final_db_status(task).await;
                    return Err(e).context("Failed to place replacement order");
               }
           }
      } else {
           // --- ИСПРАВЛЕНО: Убираем плейсхолдер ---
           info!(operation_id=task.operation_id, ?leg, "No remaining quantity after cancel confirmation. Not placing replacement.");
      }

      let current_chunk = match task.state.status { HedgerWsStatus::WaitingCancelConfirmation { chunk_index, .. } => chunk_index, _ => task.state.current_chunk_index };
      task.state.status = HedgerWsStatus::RunningChunk(current_chunk);
     Ok(())
}