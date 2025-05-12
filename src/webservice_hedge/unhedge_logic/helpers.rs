// src/webservice_hedge/unhedge_logic/helpers.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{info, trace, warn};
use std::str::FromStr;

use crate::config::WsLimitOrderPlacementStrategy;
use crate::exchange::types::OrderSide;
use crate::webservice_hedge::unhedge_task::HedgerWsUnhedgeTask;
use crate::webservice_hedge::state::{HedgerWsStatus, Leg, ChunkOrderState}; // Добавили ChunkOrderState
use crate::hedger::HedgeProgressUpdate; // Импортируем структуру для колбэка

// Расчет лимитной цены для ноги (Unhedge)
pub fn calculate_limit_price_for_leg(task: &HedgerWsUnhedgeTask, leg: Leg) -> Result<Decimal> {
     let (market_data, side, tick_size) = match leg {
         Leg::Spot => (&task.state.spot_market_data, OrderSide::Sell, task.state.spot_tick_size),
         Leg::Futures => (&task.state.futures_market_data, OrderSide::Buy, task.state.futures_tick_size),
     };
     let reference_price = match side {
         OrderSide::Buy => market_data.best_ask_price.ok_or_else(|| anyhow!("Нет доступной лучшей цены ASK для {:?}", leg)),
         OrderSide::Sell => market_data.best_bid_price.ok_or_else(|| anyhow!("Нет доступной лучшей цены BID для {:?}", leg)),
     }?;
     match task.config.ws_limit_order_placement_strategy {
         WsLimitOrderPlacementStrategy::BestAskBid => Ok(reference_price),
         WsLimitOrderPlacementStrategy::OneTickInside => {
             match side {
                 OrderSide::Buy => Ok((reference_price - tick_size).max(tick_size)), // Покупаем дешевле
                 OrderSide::Sell => Ok(reference_price + tick_size), // Продаем дороже
             }
         }
     }
}

// Округление ВНИЗ до указанного шага (Unhedge)
pub fn round_down_step(task: &HedgerWsUnhedgeTask, value: Decimal, step: Decimal) -> Result<Decimal> {
     if step <= Decimal::ZERO {
         if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
         warn!(operation_id = task.operation_id, "Шаг округления нулевой или отрицательный: {}. Возвращается исходное значение.", step);
         return Ok(value.normalize());
     }
     if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
     let positive_step = step.abs();
     // Увеличиваем точность для промежуточных вычислений, чтобы избежать ошибок округления Decimal
     let precision = positive_step.scale() + 5; // Немного больше запас
     let value_scaled = value.round_dp_with_strategy(precision, rust_decimal::RoundingStrategy::ToZero); // Явно округляем к нулю
     let step_scaled = positive_step.round_dp(precision); // Шаг можно округлять стандартно

     if step_scaled == Decimal::ZERO { // Дополнительная проверка, если шаг стал нулем после округления (маловероятно)
        warn!(operation_id = task.operation_id, "Масштабированный шаг округления стал нулевым. Возвращается исходное значение.");
        return Ok(value.normalize());
     }
     Ok(((value_scaled / step_scaled).floor() * step_scaled).normalize())
}

// Получение текущей средней цены (mid-price) для ноги (Unhedge)
pub fn get_current_price(task: &HedgerWsUnhedgeTask, leg: Leg) -> Option<Decimal> {
     let market_data = match leg {
         Leg::Spot => &task.state.spot_market_data,
         Leg::Futures => &task.state.futures_market_data,
     };
     match (market_data.best_bid_price, market_data.best_ask_price) {
         (Some(bid), Some(ask)) if bid > Decimal::ZERO && ask > Decimal::ZERO => Some((bid + ask) / dec!(2.0)),
         (Some(bid), None) if bid > Decimal::ZERO => Some(bid),
         (None, Some(ask)) if ask > Decimal::ZERO => Some(ask),
         _ => None,
     }
}

// Проверка, завершен ли текущий чанк (Unhedge)
pub fn check_chunk_completion(task: &HedgerWsUnhedgeTask) -> bool {
     task.state.active_spot_order.is_none() && task.state.active_futures_order.is_none()
}

// Отправка колбэка прогресса (Unhedge)
pub async fn send_progress_update(task: &mut HedgerWsUnhedgeTask) -> Result<()> {
    // Собираем информацию для HedgeProgressUpdate.
    // Это обновление будет отражать общий прогресс операции расхеджирования,
    // а не только одной ноги или одного ордера.

    // Определяем "ведущую" ногу для отображения рыночной цены и деталей ордера.
    // Обычно это та нога, по которой есть активный ордер, или спот по умолчанию.
    let (active_order_leg, active_order_state): (Option<Leg>, Option<&ChunkOrderState>) =
        if task.state.active_spot_order.is_some() {
            (Some(Leg::Spot), task.state.active_spot_order.as_ref())
        } else if task.state.active_futures_order.is_some() {
            (Some(Leg::Futures), task.state.active_futures_order.as_ref())
        } else {
            (None, None) // Нет активных ордеров
        };

    let stage = active_order_leg.unwrap_or(Leg::Spot); // По умолчанию показываем прогресс по споту, если нет активных

    let market_price_for_cb = get_current_price(task, stage)
        .unwrap_or_else(|| {
            warn!(operation_id = task.operation_id, "Не удалось получить текущую цену для колбэка прогресса ({:?}), используем 0.0", stage);
            Decimal::ZERO
        })
        .to_f64().unwrap_or(0.0);

    let (limit_price_for_cb, filled_qty_current_order, target_qty_current_order, is_replacement_cb) =
        if let Some(order_state) = active_order_state {
            (
                order_state.limit_price.to_f64().unwrap_or(0.0),
                order_state.filled_quantity.to_f64().unwrap_or(0.0),
                order_state.target_quantity.to_f64().unwrap_or(0.0),
                matches!(task.state.status, HedgerWsStatus::CancellingOrder { .. } | HedgerWsStatus::WaitingCancelConfirmation { .. })
            )
        } else {
            (0.0, 0.0, 0.0, false) // Нет активного ордера
        };
    
    // Общий прогресс по ноге
    let (cumulative_filled_for_stage, total_target_for_stage) = match stage {
        Leg::Spot => (
            task.state.cumulative_spot_filled_quantity.to_f64().unwrap_or(0.0),
            task.actual_spot_sell_target.to_f64().unwrap_or(0.0) // Общая цель по продаже спота
        ),
        Leg::Futures => (
            task.state.cumulative_futures_filled_quantity.to_f64().unwrap_or(0.0),
            task.original_futures_target.abs().to_f64().unwrap_or(0.0) // Общая цель по покупке фьюча
        ),
    };

    let update_data = HedgeProgressUpdate {
        stage: match stage { // Преобразуем Leg в hedger::HedgeStage
            Leg::Spot => crate::hedger::HedgeStage::Spot,
            Leg::Futures => crate::hedger::HedgeStage::Futures,
        },
        current_spot_price: market_price_for_cb, // Название поля общее, но это рыночная цена для stage
        new_limit_price: limit_price_for_cb,
        is_replacement: is_replacement_cb,
        filled_qty: filled_qty_current_order,
        target_qty: target_qty_current_order,
        cumulative_filled_qty: cumulative_filled_for_stage,
        total_target_qty: total_target_for_stage,
    };

    if let Err(e) = (task.progress_callback)(update_data).await {
        if !e.to_string().contains("message is not modified") { // Игнорируем эту специфическую ошибку Telegram
            warn!(operation_id = task.operation_id, "Ошибка при отправке колбэка прогресса расхеджирования: {}", e);
        }
    }
    Ok(())
}


// Обновление финального статуса операции в БД (Unhedge)
pub async fn update_final_db_status(task: &HedgerWsUnhedgeTask) {
     let status_str = match &task.state.status {
         HedgerWsStatus::Completed => "Completed",
         HedgerWsStatus::Cancelled => "Cancelled", // Если будет механизм отмены пользователем
         HedgerWsStatus::Failed(_) => "Failed",
         _ => {
             warn!(operation_id = task.operation_id, status = ?task.state.status, "update_final_db_status (unhedge) вызвана с нефинальным статусом.");
             return;
         }
     };
     let error_message = match &task.state.status {
         HedgerWsStatus::Failed(msg) => Some(msg.as_str()),
         _ => None,
     };

     // Для расхеджирования мы в основном помечаем исходную операцию хеджирования.
     // Если бы была отдельная таблица для операций расхеджирования, здесь было бы обновление ее статуса.
     // Сейчас `mark_hedge_as_unhedged` вызывается в `reconciliation.rs` при успехе.
     // При ошибках до реконсиляции, исходная операция хеджирования остается "Completed", но не "Unhedged".
     // Это может потребовать доработки, если нужно явно отмечать неудачные попытки расхеджирования.
     info!(operation_id = task.operation_id, final_status = %status_str, error=?error_message, "Задача расхеджирования достигла финального состояния.");

     // Если задача расхеджирования провалилась или была отменена ДО того, как была помечена исходная операция,
     // возможно, стоит как-то это отразить в БД исходной операции (например, в error_message или спец. поле).
     // Пока что, если reconciliation не был достигнут, запись в unhedged_op_id не произойдет.
}

// Получение количества знаков после запятой из строки шага (Общий)
pub fn get_decimals_from_step(step: Option<&str>) -> Result<u32> {
     let step_str = step.ok_or_else(|| anyhow!("Отсутствует qtyStep/basePrecision"))?;
     Ok(step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32)
}

// Получение шага как Decimal (Общий)
pub fn get_step_decimal(step: Option<&str>) -> Result<Decimal> {
     let step_str = step.ok_or_else(|| anyhow!("Отсутствует qtyStep/basePrecision"))?;
     Decimal::from_str(step_str).context("Не удалось преобразовать строку шага в Decimal")
}

// Проверка дисбаланса стоимостей (Unhedge)
pub fn check_value_imbalance(task: &HedgerWsUnhedgeTask) -> bool {
     if let Some(ratio) = task.config.ws_max_value_imbalance_ratio {
         if ratio <= 0.0 { return false; } // Дисбаланс не проверяется, если ratio некорректный
         
         // Для расхеджирования, "базовая" стоимость - это стоимость спота, который мы изначально продаем.
         // Или, если более точно, стоимость фьючерсов, которые мы должны откупить,
         // так как именно они являются "противовесом" проданному споту.
         // Используем initial_target_spot_value как оценку стоимости проданного спота.
         let total_value_base = task.state.initial_target_spot_value; 
         if total_value_base <= Decimal::ZERO { 
             trace!(operation_id=%task.operation_id, "Базовая стоимость для проверки дисбаланса нулевая или отрицательная, дисбаланс не проверяется.");
             return false; 
         }

         // Дисбаланс - это разница между стоимостью исполненного спота (проданного)
         // и стоимостью исполненных фьючерсов (купленных). Обе величины должны быть положительными.
         let imbalance = (task.state.cumulative_spot_filled_value - task.state.cumulative_futures_filled_value).abs();
         
         let current_ratio = if total_value_base != Decimal::ZERO { imbalance / total_value_base } else { Decimal::MAX }; // Если база 0, любой дисбаланс будет > порога
         
         let threshold = Decimal::try_from(ratio).unwrap_or_else(|_| {
            warn!(operation_id=%task.operation_id, "Не удалось преобразовать ws_max_value_imbalance_ratio в Decimal, используем 0.05");
            dec!(0.05) // Значение по умолчанию, если конвертация не удалась
         });

         trace!(operation_id=%task.operation_id, %imbalance, spot_val=%task.state.cumulative_spot_filled_value, fut_val=%task.state.cumulative_futures_filled_value, base_val=%total_value_base, %current_ratio, %threshold, "Проверка дисбаланса стоимостей при расхеджировании");
         current_ratio > threshold
     } else {
         false // Конфигурация для проверки дисбаланса отсутствует
     }
}