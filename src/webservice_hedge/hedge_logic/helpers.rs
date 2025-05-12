// src/webservice_hedge/hedge_logic/helpers.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{error, info, trace, warn};
use std::str::FromStr;

use crate::config::WsLimitOrderPlacementStrategy;
use crate::exchange::types::OrderSide;
use crate::storage;
use crate::webservice_hedge::hedge_task::HedgerWsHedgeTask;
use crate::webservice_hedge::state::{HedgerWsStatus, Leg, ChunkOrderState}; // Добавили ChunkOrderState
use crate::hedger::HedgeProgressUpdate;


pub fn calculate_limit_price_for_leg(task: &HedgerWsHedgeTask, leg: Leg) -> Result<Decimal> {
    let (market_data, side, tick_size, leg_name_str) = match leg {
        Leg::Spot => (&task.state.spot_market_data, OrderSide::Buy, task.state.spot_tick_size, "Spot"),
        Leg::Futures => (&task.state.futures_market_data, OrderSide::Sell, task.state.futures_tick_size, "Futures"),
    };

    let reference_price = match side {
        OrderSide::Buy => { // Покупка (нужна цена Ask)
            if let Some(ask_price) = market_data.best_ask_price {
                ask_price
            } else if let Some(bid_price) = market_data.best_bid_price {
                warn!(operation_id = task.operation_id, leg = %leg_name_str,
                      "No best ask price for BUY leg. Using best bid ({}) + 3 ticks as fallback.", bid_price);
                // Если нет ask, берем лучший bid и делаем цену чуть выше (агрессивнее для покупки)
                // Убедимся, что tick_size не нулевой, чтобы избежать паники при умножении
                if tick_size > Decimal::ZERO {
                    bid_price + (tick_size * Decimal::from(3)) // Пример: 3 тика выше лучшего бида
                } else {
                    warn!(operation_id = task.operation_id, leg = %leg_name_str, "Tick size is zero for BUY leg fallback. Using bid_price directly.");
                    bid_price
                }
            } else {
                error!(operation_id = task.operation_id, leg = %leg_name_str, "No best ask or bid price available for BUY leg.");
                return Err(anyhow!("No best ask or bid price available for {} BUY leg", leg_name_str));
            }
        }
        OrderSide::Sell => { // Продажа (нужна цена Bid)
            if let Some(bid_price) = market_data.best_bid_price {
                bid_price
            } else if let Some(ask_price) = market_data.best_ask_price {
                warn!(operation_id = task.operation_id, leg = %leg_name_str,
                      "No best bid price for SELL leg. Using best ask ({}) - 3 ticks as fallback.", ask_price);
                // Если нет bid, берем лучший ask и делаем цену чуть ниже (агрессивнее для продажи)
                // Убедимся, что tick_size не нулевой и что результат не станет отрицательным
                if tick_size > Decimal::ZERO {
                    (ask_price - (tick_size * Decimal::from(3))).max(tick_size) // .max(tick_size) чтобы цена не стала слишком низкой или отрицательной
                } else {
                    warn!(operation_id = task.operation_id, leg = %leg_name_str, "Tick size is zero for SELL leg fallback. Using ask_price directly.");
                    ask_price
                }
            } else {
                error!(operation_id = task.operation_id, leg = %leg_name_str, "No best bid or ask price available for SELL leg.");
                return Err(anyhow!("No best bid or ask price available for {} SELL leg", leg_name_str));
            }
        }
    };

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

pub fn round_down_step(task: &HedgerWsHedgeTask, value: Decimal, step: Decimal) -> Result<Decimal> {
     if step <= Decimal::ZERO {
         if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
         warn!(operation_id = task.operation_id, value=%value, step=%step, "Rounding step zero or negative. Returning original value normalized.");
         return Ok(value.normalize());
     }
     if value == Decimal::ZERO { return Ok(Decimal::ZERO); }

     let positive_step = step.abs();
     // Увеличиваем точность для промежуточных вычислений, чтобы избежать ошибок округления Decimal
     // rust_decimal не всегда идеально обрабатывает деление без достаточной точности
     let internal_precision = value.scale() + positive_step.scale() + 5; // Добавим запас

     let value_scaled = value.round_dp_with_strategy(internal_precision, rust_decimal::RoundingStrategy::ToZero);
     let step_scaled = positive_step.round_dp_with_strategy(internal_precision, rust_decimal::RoundingStrategy::ToZero);

     if step_scaled == Decimal::ZERO {
        warn!(operation_id = task.operation_id, value=%value, step=%step, "Scaled rounding step became zero. Returning original value normalized.");
        return Ok(value.normalize());
     }
     Ok(((value_scaled / step_scaled).floor() * step_scaled).normalize())
}

pub fn get_current_price(task: &HedgerWsHedgeTask, leg: Leg) -> Option<Decimal> {
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

pub fn check_chunk_completion(task: &HedgerWsHedgeTask) -> bool {
     task.state.active_spot_order.is_none() && task.state.active_futures_order.is_none()
}

pub async fn send_progress_update(task: &mut HedgerWsHedgeTask) -> Result<()> {
    let (active_order_leg, active_order_state_opt) =
        if task.state.active_spot_order.is_some() {
            (Some(Leg::Spot), task.state.active_spot_order.as_ref())
        } else if task.state.active_futures_order.is_some() {
            (Some(Leg::Futures), task.state.active_futures_order.as_ref())
        } else {
            match task.state.status {
                HedgerWsStatus::PlacingSpotOrder(_) | HedgerWsStatus::RunningChunk(_)
                    if task.state.active_spot_order.is_none() && task.state.cumulative_spot_filled_value < task.state.initial_target_spot_value * dec!(0.999) => (Some(Leg::Spot), None),
                HedgerWsStatus::PlacingFuturesOrder(_) | HedgerWsStatus::RunningChunk(_)
                    if task.state.active_futures_order.is_none() => (Some(Leg::Futures), None),
                _ => (None, None)
            }
        };

    let stage_for_cb = match active_order_leg {
        Some(Leg::Spot) => crate::hedger::HedgeStage::Spot,
        Some(Leg::Futures) => crate::hedger::HedgeStage::Futures,
        None => {
            if task.state.cumulative_spot_filled_value < task.state.initial_target_spot_value * dec!(0.9999) && task.state.active_spot_order.is_none() {
                crate::hedger::HedgeStage::Spot
            } else if task.state.active_futures_order.is_none() {
                 crate::hedger::HedgeStage::Futures
            } else {
                crate::hedger::HedgeStage::Spot
            }
        }
    };

    let market_price_for_cb = get_current_price(task, active_order_leg.unwrap_or(Leg::Spot))
        .unwrap_or_else(|| {
            warn!(operation_id = task.operation_id, "Не удалось получить текущую цену для колбэка прогресса, используем 0.0");
            Decimal::ZERO
        })
        .to_f64().unwrap_or(0.0);

    let (limit_price_for_cb, filled_qty_current_order, target_qty_current_order, is_replacement_cb) =
        if let Some(order_state) = active_order_state_opt {
            (
                order_state.limit_price.to_f64().unwrap_or(0.0),
                order_state.filled_quantity.to_f64().unwrap_or(0.0),
                order_state.target_quantity.to_f64().unwrap_or(0.0),
                matches!(task.state.status, HedgerWsStatus::CancellingOrder { .. } | HedgerWsStatus::WaitingCancelConfirmation { .. })
            )
        } else {
             let target_for_stage_if_no_order = match stage_for_cb {
                 crate::hedger::HedgeStage::Spot => task.state.chunk_base_quantity_spot.to_f64().unwrap_or(0.0),
                 crate::hedger::HedgeStage::Futures => task.state.chunk_base_quantity_futures.to_f64().unwrap_or(0.0),
             };
            (0.0, 0.0, target_for_stage_if_no_order, false)
        };

    let (cumulative_filled_for_stage, total_target_for_stage_qty) = match stage_for_cb {
        crate::hedger::HedgeStage::Spot => {
            let spot_price_for_total_qty = get_current_price(task, Leg::Spot).unwrap_or(Decimal::ONE);
            let approx_total_spot_qty = if spot_price_for_total_qty > Decimal::ZERO {
                (task.state.initial_target_spot_value / spot_price_for_total_qty)
            } else {
                task.state.initial_target_spot_value // Fallback, если цена 0, хотя это маловероятно
            };
            (
                task.state.cumulative_spot_filled_quantity.to_f64().unwrap_or(0.0),
                approx_total_spot_qty.to_f64().unwrap_or(0.0)
            )
        }
        crate::hedger::HedgeStage::Futures => (
            task.state.cumulative_futures_filled_quantity.to_f64().unwrap_or(0.0),
            task.state.initial_target_futures_qty.to_f64().unwrap_or(0.0)
        ),
    };

    let update_data = HedgeProgressUpdate {
        stage: stage_for_cb,
        current_spot_price: market_price_for_cb,
        new_limit_price: limit_price_for_cb,
        is_replacement: is_replacement_cb,
        filled_qty: filled_qty_current_order,
        target_qty: target_qty_current_order,
        cumulative_filled_qty: cumulative_filled_for_stage,
        total_target_qty: total_target_for_stage_qty,
    };

    if let Err(e) = (task.progress_callback)(update_data).await {
        if !e.to_string().contains("message is not modified") {
            warn!(operation_id = task.operation_id, "Ошибка при отправке колбэка прогресса хеджирования: {}", e);
        }
    }
     Ok(())
}

pub async fn update_final_db_status(task: &HedgerWsHedgeTask) {
     let status_str = match &task.state.status {
         HedgerWsStatus::Completed => "Completed",
         HedgerWsStatus::Cancelled => "Cancelled",
         HedgerWsStatus::Failed(_) => "Failed",
         _ => {
             warn!(operation_id = task.operation_id, status = ?task.state.status, "update_final_db_status called with non-final status.");
             return;
         }
     };
     let error_message = match &task.state.status {
         HedgerWsStatus::Failed(msg) => Some(msg.as_str()),
         _ => None,
     };
     let fut_qty_f64 = task.state.cumulative_futures_filled_quantity.to_f64().unwrap_or(0.0);
     let spot_qty_f64 = task.state.cumulative_spot_filled_quantity.to_f64().unwrap_or(0.0);


    // Для WS хеджа, ID ордеров могут меняться. Возможно, лучше не сохранять ID конкретного ордера как "финальный"
    // а ориентироваться на исполненные объемы. Или хранить ID самого последнего успешного ордера каждого этапа, если это нужно.
    // Пока передаем None для ID ордеров в финальном статусе, т.к. они могли быть переставлены.
     if let Err(e) = storage::update_hedge_final_status(
         &task.database,
         task.operation_id,
         status_str,
         None, // last_futures_order_id, для WS это может быть нерелевантно или сложно отследить один "финальный"
         fut_qty_f64, // Передаем исполненный объем фьючерсов
         error_message,
         // TODO: Если нужно хранить исполненный спот в той же функции, нужно изменить update_hedge_final_status
         // Пока что spot_filled_qty обновляется через update_hedge_spot_order
     ).await {
         error!(operation_id = task.operation_id, %e, "Failed to update final status in DB!");
     } else {
         info!(operation_id = task.operation_id, %status_str, spot_filled_ws = %spot_qty_f64, fut_filled_ws = %fut_qty_f64, "Final status updated in DB.");
     }
}

pub fn get_decimals_from_step(step: Option<&str>) -> Result<u32> {
     let step_str = step.ok_or_else(|| anyhow!("Missing qtyStep/basePrecision"))?;
     Ok(step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32)
}

pub fn get_step_decimal(step: Option<&str>) -> Result<Decimal> {
     let step_str = step.ok_or_else(|| anyhow!("Missing qtyStep/basePrecision"))?;
     Decimal::from_str(step_str).context("Failed to parse step string to Decimal")
}

pub fn check_value_imbalance(task: &HedgerWsHedgeTask) -> bool {
     if let Some(ratio) = task.config.ws_max_value_imbalance_ratio {
         if ratio <= 0.0 { return false; }
         let total_value_base = task.state.initial_target_spot_value;
         if total_value_base <= Decimal::ZERO { return false; }
         // Для хеджа: сравниваем стоимость купленного спота со стоимостью проданного фьючерса
         // Обе величины должны быть положительными для корректного сравнения их абсолютного дисбаланса.
         // cumulative_futures_filled_value уже хранится как положительное число (abs() берется при обновлении).
         let imbalance = (task.state.cumulative_spot_filled_value - task.state.cumulative_futures_filled_value).abs();
         let current_ratio = if total_value_base != Decimal::ZERO {
             imbalance / total_value_base
         } else {
              Decimal::MAX
         };
         let threshold = Decimal::try_from(ratio).unwrap_or_else(|_| dec!(0.05)); // 5% по умолчанию
         trace!(operation_id=%task.operation_id, %imbalance, spot_val=%task.state.cumulative_spot_filled_value, fut_val=%task.state.cumulative_futures_filled_value, base_val=%total_value_base, %current_ratio, %threshold, "Checking value imbalance");
         current_ratio > threshold
     } else {
         false
     }
}