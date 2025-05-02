// src/hedger_ws/hedge_logic/helpers.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
// Убираем debug и warn, если они больше не используются
use tracing::{error, info, trace};
use std::str::FromStr;

use crate::config::WsLimitOrderPlacementStrategy;
// Убираем лишние скобки
use crate::exchange::types::OrderSide;
use crate::storage;
use crate::hedger_ws::hedge_task::HedgerWsHedgeTask;
use crate::hedger_ws::state::{HedgerWsStatus, Leg};

// Расчет лимитной цены для ноги
pub fn calculate_limit_price_for_leg(task: &HedgerWsHedgeTask, leg: Leg) -> Result<Decimal> {
     let (market_data, side, tick_size) = match leg {
         Leg::Spot => (&task.state.spot_market_data, OrderSide::Buy, task.state.spot_tick_size),
         Leg::Futures => (&task.state.futures_market_data, OrderSide::Sell, task.state.futures_tick_size),
     };
     let reference_price = match side {
         OrderSide::Buy => market_data.best_ask_price.ok_or_else(|| anyhow!("No best ask price available for {:?}", leg)),
         OrderSide::Sell => market_data.best_bid_price.ok_or_else(|| anyhow!("No best bid price available for {:?}", leg)),
     }?;
     match task.config.ws_limit_order_placement_strategy {
         WsLimitOrderPlacementStrategy::BestAskBid => Ok(reference_price),
         WsLimitOrderPlacementStrategy::OneTickInside => {
             match side {
                 OrderSide::Buy => Ok((reference_price - tick_size).max(tick_size)),
                 OrderSide::Sell => Ok(reference_price + tick_size),
             }
         }
     }
}

// Округление ВНИЗ до указанного шага
pub fn round_down_step(task: &HedgerWsHedgeTask, value: Decimal, step: Decimal) -> Result<Decimal> {
     if step <= Decimal::ZERO {
         if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
         // Используем info или warn по необходимости
         tracing::warn!(operation_id = task.operation_id, "Rounding step zero or negative: {}. Returning original value.", step);
         return Ok(value.normalize());
     }
     if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
     let positive_step = step.abs();
     let precision = positive_step.scale() + 3;
     let value_scaled = value.round_dp(precision);
     let step_scaled = positive_step.round_dp(precision);
     Ok(((value_scaled / step_scaled).floor() * step_scaled).normalize())
}

// Получение текущей средней цены (mid-price) для ноги
pub fn get_current_price(task: &HedgerWsHedgeTask, leg: Leg) -> Option<Decimal> {
     let market_data = match leg {
         Leg::Spot => &task.state.spot_market_data,
         Leg::Futures => &task.state.futures_market_data,
     };
     match (market_data.best_bid_price, market_data.best_ask_price) {
         (Some(bid), Some(ask)) => Some((bid + ask) / dec!(2.0)),
         (Some(bid), None) => Some(bid),
         (None, Some(ask)) => Some(ask),
         (None, None) => None,
     }
}

// Проверка, завершен ли текущий чанк
pub fn check_chunk_completion(task: &HedgerWsHedgeTask) -> bool {
     task.state.active_spot_order.is_none() && task.state.active_futures_order.is_none()
}

// Отправка колбэка прогресса (заглушка)
pub async fn send_progress_update(task: &mut HedgerWsHedgeTask) -> Result<()> {
     // TODO: Сформировать HedgeProgressUpdate и вызвать task.progress_callback
     trace!(operation_id = task.operation_id, "Progress update callback not implemented yet.");
     Ok(())
}

// Обновление финального статуса операции в БД
pub async fn update_final_db_status(task: &HedgerWsHedgeTask) {
     let status_str = match &task.state.status {
         HedgerWsStatus::Completed => "Completed",
         HedgerWsStatus::Cancelled => "Cancelled",
         HedgerWsStatus::Failed(_) => "Failed",
         _ => {
             tracing::warn!(operation_id = task.operation_id, status = ?task.state.status, "update_final_db_status called with non-final status.");
             return;
         }
     };
     let error_message = match &task.state.status {
         HedgerWsStatus::Failed(msg) => Some(msg.as_str()),
         _ => None,
     };
     // Убираем неиспользуемую переменную spot_qty_f64
     // let spot_qty_f64 = task.state.cumulative_spot_filled_quantity.to_f64().unwrap_or(0.0);
     let fut_qty_f64 = task.state.cumulative_futures_filled_quantity.to_f64().unwrap_or(0.0);
     // ID последнего фьюч ордера - пока не храним его отдельно при успехе/отмене, передаем None
     let last_futures_order_id: Option<&str> = None;

     // --- ИСПРАВЛЕННЫЙ ВЫЗОВ ---
     if let Err(e) = storage::update_hedge_final_status(
         &task.database,          // 1. db
         task.operation_id,       // 2. operation_id
         status_str,              // 3. status
         last_futures_order_id,   // 4. futures_order_id (Option<&str>)
         fut_qty_f64,             // 5. futures_filled_qty (f64)
         error_message,           // 6. error_message (Option<&str>)
     ).await {
         error!(operation_id = task.operation_id, %e, "Failed to update final status in DB!");
     } else {
         info!(operation_id = task.operation_id, %status_str, "Final status updated in DB.");
     }
     // --- КОНЕЦ ИСПРАВЛЕННОГО ВЫЗОВА ---
}

// Получение количества знаков после запятой из строки шага
pub fn get_decimals_from_step(step: Option<&str>) -> Result<u32> {
     let step_str = step.ok_or_else(|| anyhow!("Missing qtyStep/basePrecision"))?;
     Ok(step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32)
}

// Получение шага как Decimal
pub fn get_step_decimal(step: Option<&str>) -> Result<Decimal> {
     let step_str = step.ok_or_else(|| anyhow!("Missing qtyStep/basePrecision"))?;
     Decimal::from_str(step_str).context("Failed to parse step string to Decimal")
}

// Проверка дисбаланса стоимостей
pub fn check_value_imbalance(task: &HedgerWsHedgeTask) -> bool {
     if let Some(ratio) = task.config.ws_max_value_imbalance_ratio {
         if ratio <= 0.0 { return false; }
         let total_value_base = task.state.initial_target_spot_value;
         if total_value_base <= Decimal::ZERO { return false; }
         let imbalance = (task.state.cumulative_spot_filled_value - task.state.cumulative_futures_filled_value.abs()).abs();
         // Добавим проверку деления на ноль для current_ratio
         let current_ratio = if total_value_base != Decimal::ZERO {
             imbalance / total_value_base
         } else {
              Decimal::ZERO // или другое значение по умолчанию/обработка ошибки
         };
         let threshold = Decimal::try_from(ratio).unwrap_or_default();
         trace!(operation_id=%task.operation_id, %imbalance, %current_ratio, %threshold, "Checking value imbalance");
         current_ratio > threshold
     } else {
         false
     }
}