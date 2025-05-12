// src/webservice_hedge/unhedge_logic/order_management.rs

use anyhow::{anyhow, Result, Context};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{error, info, warn, trace};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::exchange::types::{OrderSide, OrderStatusText};
use crate::webservice_hedge::unhedge_task::HedgerWsUnhedgeTask; // Изменено на UnhedgeTask
use crate::webservice_hedge::state::{ChunkOrderState, HedgerWsStatus, Leg};
// Используем хелперы из unhedge_logic
use crate::webservice_hedge::unhedge_logic::helpers::{calculate_limit_price_for_leg, round_down_step, update_final_db_status};

// Проверка активных ордеров на "устаревание" цены для РАСХЕДЖИРОВАНИЯ
pub async fn check_stale_orders(task: &mut HedgerWsUnhedgeTask) -> Result<()> {
    if !matches!(task.state.status, HedgerWsStatus::RunningChunk(_)) { return Ok(()); }

    if let Some(stale_ratio_f64) = task.config.ws_stale_price_ratio {
        if stale_ratio_f64 <= 0.0 { return Ok(()); }
        let stale_ratio_decimal = Decimal::try_from(stale_ratio_f64)?;
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;

        // Проверка ордера на СПОТ (Расхеджирование: Продажа)
        let spot_order_opt = task.state.active_spot_order.clone();
        if let Some(spot_order) = spot_order_opt {
            if matches!(spot_order.status, OrderStatusText::New | OrderStatusText::PartiallyFilled) {
                // Для продажи спота сравниваем нашу цену продажи (limit_price) с лучшей ценой покупки на рынке (best_bid)
                if let (Some(best_bid), Some(last_update_time)) = (task.state.spot_market_data.best_bid_price, task.state.spot_market_data.last_update_time_ms) {
                    if now_ms - last_update_time < 5000 { // Проверка актуальности рыночных данных
                        // "Устарел", если наша цена продажи значительно НИЖЕ текущей лучшей цены покупки
                        if spot_order.limit_price < best_bid * (Decimal::ONE - stale_ratio_decimal) {
                            warn!(operation_id = task.operation_id, order_id = %spot_order.order_id, limit_price = %spot_order.limit_price, best_bid = %best_bid, "РАСХЕДЖ: Спот ордер (продажа) устарел! Запускаем замену.");
                            initiate_order_replacement(task, Leg::Spot, "StalePrice".to_string()).await?;
                            return Ok(());
                        }
                    } else {
                        trace!(operation_id = task.operation_id, "РАСХЕДЖ: Рыночные данные для спота слишком старые для проверки устаревания ({:.1}с)", (now_ms - last_update_time) as f64 / 1000.0);
                    }
                }
            }
        }

        // Проверка ордера на ФЬЮЧЕРС (Расхеджирование: Покупка)
        let futures_order_opt = task.state.active_futures_order.clone();
        if let Some(futures_order) = futures_order_opt {
            if matches!(futures_order.status, OrderStatusText::New | OrderStatusText::PartiallyFilled) {
                // Для покупки фьючерса сравниваем нашу цену покупки (limit_price) с лучшей ценой продажи на рынке (best_ask)
                if let (Some(best_ask), Some(last_update_time)) = (task.state.futures_market_data.best_ask_price, task.state.futures_market_data.last_update_time_ms) {
                    if now_ms - last_update_time < 5000 { // Проверка актуальности рыночных данных
                        // "Устарел", если наша цена покупки значительно ВЫШЕ текущей лучшей цены продажи
                        if futures_order.limit_price > best_ask * (Decimal::ONE + stale_ratio_decimal) {
                            warn!(operation_id = task.operation_id, order_id = %futures_order.order_id, limit_price = %futures_order.limit_price, best_ask = %best_ask, "РАСХЕДЖ: Фьючерсный ордер (покупка) устарел! Запускаем замену.");
                            initiate_order_replacement(task, Leg::Futures, "StalePrice".to_string()).await?;
                            return Ok(());
                        }
                    } else {
                        trace!(operation_id = task.operation_id, "РАСХЕДЖ: Рыночные данные для фьючерса слишком старые для проверки устаревания ({:.1}с)", (now_ms - last_update_time) as f64 / 1000.0);
                    }
                }
            }
        }
    }
    Ok(())
}

// Инициирование замены ордера (отправка команды cancel) для РАСХЕДЖИРОВАНИЯ
pub async fn initiate_order_replacement(task: &mut HedgerWsUnhedgeTask, leg_to_cancel: Leg, reason: String) -> Result<()> {
    let (order_to_cancel_opt, symbol_str, is_spot) = match leg_to_cancel {
       Leg::Spot => (task.state.active_spot_order.as_ref(), &task.state.symbol_spot, true),
       Leg::Futures => (task.state.active_futures_order.as_ref(), &task.state.symbol_futures, false),
    };

    if let Some(order_to_cancel) = order_to_cancel_opt {
       if matches!(task.state.status, HedgerWsStatus::CancellingOrder{..} | HedgerWsStatus::WaitingCancelConfirmation{..}) {
           warn!(operation_id=task.operation_id, order_id=%order_to_cancel.order_id, current_status=?task.state.status, "Замена (расхедж) уже в процессе. Пропускаем.");
           return Ok(());
       }
       info!(operation_id = task.operation_id, order_id = %order_to_cancel.order_id, ?leg_to_cancel, %reason, "Инициируем замену ордера РАСХЕДЖИРОВАНИЯ: отправка запроса на отмену...");
       let current_chunk = match task.state.status { HedgerWsStatus::RunningChunk(idx) => idx, _ => task.state.current_chunk_index.saturating_sub(1).max(1) };
       let order_id_to_cancel_str = order_to_cancel.order_id.clone();

       task.state.status = HedgerWsStatus::CancellingOrder { chunk_index: current_chunk, leg_to_cancel, order_id_to_cancel: order_id_to_cancel_str.clone(), reason };

       let cancel_result = if is_spot { task.exchange_rest.cancel_spot_order(symbol_str, &order_id_to_cancel_str).await }
                         else { task.exchange_rest.cancel_futures_order(symbol_str, &order_id_to_cancel_str).await };

       if let Err(e) = cancel_result {
            error!(operation_id = task.operation_id, order_id = %order_id_to_cancel_str, %e, "Не удалось отправить запрос на отмену для замены ордера РАСХЕДЖИРОВАНИЯ");
            task.state.status = HedgerWsStatus::RunningChunk(current_chunk); // Возвращаем статус
            return Err(e).context("Не удалось отправить запрос на отмену (расхедж)");
       } else {
            info!(operation_id = task.operation_id, order_id = %order_id_to_cancel_str, "Запрос на отмену (расхедж) отправлен. Ожидаем подтверждения по WebSocket...");
            task.state.status = HedgerWsStatus::WaitingCancelConfirmation { chunk_index: current_chunk, cancelled_leg: leg_to_cancel, cancelled_order_id: order_id_to_cancel_str };
       }
    } else {
       warn!(operation_id = task.operation_id, ?leg_to_cancel, "Попытка заменить ордер РАСХЕДЖИРОВАНИЯ, но активный ордер для ноги не найден.");
    }
    Ok(())
}

// Обработка подтверждения отмены и выставление нового ордера для РАСХЕДЖИРОВАНИЯ
pub async fn handle_cancel_confirmation(task: &mut HedgerWsUnhedgeTask, cancelled_order_id: &str, leg: Leg) -> Result<()> {
     info!(operation_id = task.operation_id, %cancelled_order_id, ?leg, "Обработка подтверждения отмены РАСХЕДЖИРОВАНИЯ: выставляем заменяющий ордер при необходимости...");

    // Определяем общий оставшийся объем для данной ноги на основе общих целей расхеджирования
    let (total_target_qty, filled_qty, min_quantity, step, is_spot) = match leg {
        Leg::Spot => (
            task.actual_spot_sell_target, // Общая цель по продаже спота
            task.state.cumulative_spot_filled_quantity,
            task.state.min_spot_quantity,
            task.state.spot_quantity_step,
            true
        ),
        Leg::Futures => (
            task.original_futures_target.abs(), // Общая цель по покупке фьючерса (абсолютное значение)
            task.state.cumulative_futures_filled_quantity,
            task.state.min_futures_quantity,
            task.state.futures_quantity_step,
            false
        ),
    };

    // Общий остаток по ноге
    let _remaining_quantity_overall = (total_target_qty - filled_qty).max(Decimal::ZERO);

    // Определяем объем для заменяющего ордера.
    // Логика должна быть такой: если отменен ордер, который был частью текущего чанка,
    // то новый ордер должен закрыть потребность этого чанка по данной ноге.
    let current_chunk_idx = match task.state.status {
        HedgerWsStatus::WaitingCancelConfirmation { chunk_index, .. } => chunk_index,
        // Если статус неожиданно другой, берем текущий индекс чанка из задачи
        _ => task.state.current_chunk_index.saturating_sub(1).max(1),
    };
    let is_last_chunk = current_chunk_idx == task.state.total_chunks;

    let quantity_for_replacement_calc = if is_last_chunk {
        // Для последнего чанка - это весь оставшийся объем по ноге
        (total_target_qty - filled_qty).max(Decimal::ZERO)
    } else {
        // Для промежуточных чанков - это базовый объем чанка для данной ноги
        // (предполагаем, что отмененный ордер должен был покрыть этот объем)
        match leg {
            Leg::Spot => task.state.chunk_base_quantity_spot,
            Leg::Futures => task.state.chunk_base_quantity_futures,
        }
    };
    // Учитываем уже исполненное в рамках *этого* чанка по этой ноге, если такая информация есть.
    // Пока упрощенно: выставляем на базовый объем чанка или остаток.
    // Более точная логика потребовала бы хранить исполнение по каждой ноге внутри каждого чанка.

    let remaining_qty_for_replacement_order = round_down_step(task, quantity_for_replacement_calc, step)?;

    // Очищаем состояние активного ордера для отмененной ноги
    match leg {
        Leg::Spot => task.state.active_spot_order = None,
        Leg::Futures => task.state.active_futures_order = None,
    }
    info!(operation_id = task.operation_id, %cancelled_order_id, ?leg, "Очищено состояние активного ордера РАСХЕДЖИРОВАНИЯ после подтверждения отмены.");

    let tolerance = dec!(1e-12);
    if remaining_qty_for_replacement_order < min_quantity && remaining_qty_for_replacement_order.abs() > tolerance {
        warn!(operation_id=task.operation_id, %remaining_qty_for_replacement_order, %min_quantity, ?leg, "РАСХЕДЖ: Оставшийся объем для замены слишком мал (пыль). Ордер не выставляется.");
    } else if remaining_qty_for_replacement_order.abs() > tolerance { // Убедимся, что объем не нулевой
        info!(operation_id = task.operation_id, replacement_qty = %remaining_qty_for_replacement_order, ?leg, "РАСХЕДЖ: Выставляем заменяющий ордер...");
        let new_limit_price = calculate_limit_price_for_leg(task, leg)?; // хелпер использует задачу расхеджирования

        let (symbol_str, order_side_for_replacement, qty_precision, price_precision) = match leg {
            // При расхеджировании: СПОТ - ПРОДАЖА, ФЬЮЧЕРС - ПОКУПКА
            Leg::Spot => (&task.state.symbol_spot, OrderSide::Sell, step.scale(), task.state.spot_tick_size.scale()),
            Leg::Futures => (&task.state.symbol_futures, OrderSide::Buy, step.scale(), task.state.futures_tick_size.scale()),
        };

        let quantity_f64 = remaining_qty_for_replacement_order.round_dp(qty_precision).to_f64().unwrap_or(0.0);
        let price_f64 = new_limit_price.round_dp(price_precision).to_f64().unwrap_or(0.0);

        if quantity_f64 <= 0.0 || price_f64 <= 0.0 {
            error!(operation_id=task.operation_id, %quantity_f64, %price_f64, ?leg, "РАСХЕДЖ: Неверные параметры для заменяющего ордера!");
            task.state.status = HedgerWsStatus::RunningChunk(current_chunk_idx); // Возвращаем статус перед ошибкой
            return Err(anyhow!("Неверные параметры для заменяющего ордера РАСХЕДЖИРОВАНИЯ"));
        }

        let place_result = if is_spot {
            task.exchange_rest.place_limit_order(symbol_str, order_side_for_replacement, quantity_f64, price_f64).await
        } else {
            task.exchange_rest.place_futures_limit_order(symbol_str, order_side_for_replacement, quantity_f64, price_f64).await
        };

        match place_result {
            Ok(new_order) => {
                info!(operation_id=task.operation_id, new_order_id=%new_order.id, ?leg, "Заменяющий ордер РАСХЕДЖИРОВАНИЯ успешно выставлен.");
                let new_order_state = ChunkOrderState::new(new_order.id, symbol_str.to_string(), order_side_for_replacement, new_limit_price, remaining_qty_for_replacement_order);
                match leg {
                    Leg::Spot => task.state.active_spot_order = Some(new_order_state),
                    Leg::Futures => task.state.active_futures_order = Some(new_order_state),
                }
            }
            Err(e) => {
                let error_msg = format!("РАСХЕДЖ: Не удалось выставить заменяющий ордер для {:?}: {}", leg, e);
                error!(operation_id = task.operation_id, error=%error_msg, ?leg);
                task.state.status = HedgerWsStatus::Failed(error_msg.clone());
                update_final_db_status(task).await; // хелпер из unhedge_logic
                return Err(anyhow!(error_msg)); 
            }
        }
    } else {
        info!(operation_id=task.operation_id, ?leg, "РАСХЕДЖ: Нет оставшегося объема после подтверждения отмены или объем нулевой. Заменяющий ордер не выставляется.");
    }

    // Возвращаем статус RunningChunk, используя chunk_index из WaitingCancelConfirmation
    task.state.status = HedgerWsStatus::RunningChunk(current_chunk_idx);
    Ok(())
}