// src/webservice_hedge/unhedge_logic/helpers.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{info, trace, warn}; // Добавляем нужные макросы
use std::str::FromStr;

use crate::config::WsLimitOrderPlacementStrategy;
use crate::exchange::types::OrderSide;
// --- ИЗМЕНЕНО: Ссылка на HedgerWsUnhedgeTask ---
use crate::webservice_hedge::unhedge_task::HedgerWsUnhedgeTask;
use crate::webservice_hedge::state::{HedgerWsStatus, Leg};

// Расчет лимитной цены для ноги (Unhedge)
// Логика та же, но тип task другой
pub fn calculate_limit_price_for_leg(task: &HedgerWsUnhedgeTask, leg: Leg) -> Result<Decimal> {
     let (market_data, side, tick_size) = match leg {
         // При расхедже: ПРОДАЕМ спот, ПОКУПАЕМ фьюч
         Leg::Spot => (&task.state.spot_market_data, OrderSide::Sell, task.state.spot_tick_size),
         Leg::Futures => (&task.state.futures_market_data, OrderSide::Buy, task.state.futures_tick_size),
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

// Округление ВНИЗ до указанного шага (Unhedge)
// Логика та же, но тип task другой
pub fn round_down_step(task: &HedgerWsUnhedgeTask, value: Decimal, step: Decimal) -> Result<Decimal> {
     if step <= Decimal::ZERO {
         if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
         warn!(operation_id = task.operation_id, "Rounding step zero or negative: {}. Returning original value.", step);
         return Ok(value.normalize());
     }
     if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
     let positive_step = step.abs();
     let precision = positive_step.scale() + 3;
     let value_scaled = value.round_dp(precision);
     let step_scaled = positive_step.round_dp(precision);
     Ok(((value_scaled / step_scaled).floor() * step_scaled).normalize())
}

// Получение текущей средней цены (mid-price) для ноги (Unhedge)
// Логика та же, но тип task другой
pub fn get_current_price(task: &HedgerWsUnhedgeTask, leg: Leg) -> Option<Decimal> {
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

// Проверка, завершен ли текущий чанк (Unhedge)
// Логика та же, но тип task другой
pub fn check_chunk_completion(task: &HedgerWsUnhedgeTask) -> bool {
     task.state.active_spot_order.is_none() && task.state.active_futures_order.is_none()
}

// Отправка колбэка прогресса (Unhedge - ЗАГЛУШКА)
// Тип task изменен
pub async fn send_progress_update(task: &mut HedgerWsUnhedgeTask) -> Result<()> {
     // TODO: Сформировать HedgeProgressUpdate для Unhedge и вызвать task.progress_callback
     trace!(operation_id = task.operation_id, "Unhedge progress update callback not implemented yet.");
     Ok(())
}

// Обновление финального статуса операции в БД (Unhedge)
// Тип task изменен
pub async fn update_final_db_status(task: &HedgerWsUnhedgeTask) {
     let status_str = match &task.state.status {
         HedgerWsStatus::Completed => "Completed", // Статус самой задачи
         HedgerWsStatus::Cancelled => "Cancelled",
         HedgerWsStatus::Failed(_) => "Failed",
         _ => {
             warn!(operation_id = task.operation_id, status = ?task.state.status, "update_final_db_status (unhedge) called with non-final status.");
             return;
         }
     };
     let error_message = match &task.state.status {
         HedgerWsStatus::Failed(msg) => Some(msg.as_str()),
         _ => None,
     };
     // При расхеджировании в базе обновляется только статус Completed/Failed/Cancelled
     // самой ОРИГИНАЛЬНОЙ операции хеджирования (через mark_hedge_as_unhedged или при ошибке).
     // Поэтому здесь просто логируем финальный статус задачи.
     // Если бы была отдельная таблица Unhedge Operations, здесь был бы вызов к ней.
     info!(operation_id = task.operation_id, final_status = %status_str, error=?error_message, "Unhedge task reached final state.");

     // Вызов mark_hedge_as_unhedged происходит в reconciliation.rs
}

// Получение количества знаков после запятой из строки шага (Общий)
pub fn get_decimals_from_step(step: Option<&str>) -> Result<u32> {
     let step_str = step.ok_or_else(|| anyhow!("Missing qtyStep/basePrecision"))?;
     Ok(step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32)
}

// Получение шага как Decimal (Общий)
pub fn get_step_decimal(step: Option<&str>) -> Result<Decimal> {
     let step_str = step.ok_or_else(|| anyhow!("Missing qtyStep/basePrecision"))?;
     Decimal::from_str(step_str).context("Failed to parse step string to Decimal")
}

// Проверка дисбаланса стоимостей (Unhedge)
// Логика та же, но тип task другой
pub fn check_value_imbalance(task: &HedgerWsUnhedgeTask) -> bool {
     if let Some(ratio) = task.config.ws_max_value_imbalance_ratio {
         if ratio <= 0.0 { return false; }
         let total_value_base = task.state.initial_target_spot_value; // Используем оценку стоимости спота
         if total_value_base <= Decimal::ZERO { return false; }
         let imbalance = (task.state.cumulative_spot_filled_value - task.state.cumulative_futures_filled_value.abs()).abs();
         let current_ratio = if total_value_base != Decimal::ZERO { imbalance / total_value_base } else { Decimal::ZERO };
         let threshold = Decimal::try_from(ratio).unwrap_or_default();
         trace!(operation_id=%task.operation_id, %imbalance, %current_ratio, %threshold, "Checking unhedge value imbalance");
         current_ratio > threshold
     } else {
         false
     }
}