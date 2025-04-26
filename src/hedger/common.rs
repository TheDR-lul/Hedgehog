// src/hedger/common.rs

use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use rust_decimal::prelude::FromPrimitive;


use super::{HedgeProgressCallback, HedgeProgressUpdate, HedgeStage, Hedger, ORDER_FILL_TOLERANCE};
use crate::exchange::types::{OrderSide, OrderStatus as ExchangeOrderStatus};
use crate::exchange::Exchange;
use crate::storage::{update_hedge_spot_order, Db}; // Добавим Db и нужные функции

// Структура для передачи параметров в цикл управления ордером
pub(super) struct OrderLoopParams<'a, E: Exchange> {
    pub hedger: &'a Hedger<E>, // Доступ к exchange, slippage, max_wait
    pub db: &'a Db,
    pub operation_id: i64,
    pub symbol: &'a str, // Символ для API (spot или futures)
    pub side: OrderSide,
    pub initial_target_qty: f64, // Общая цель для этого этапа
    pub initial_limit_price: f64,
    pub progress_callback: &'a mut HedgeProgressCallback,
    pub stage: HedgeStage, // Spot или Futures
    pub is_spot: bool,     // Флаг для выбора API методов
    pub min_order_qty_decimal: Option<Decimal>, // Для проверки на пыль (только для unhedge spot)
    pub total_filled_qty_storage: Arc<TokioMutex<f64>>, // Общее исполненное кол-во на этом этапе
}

// Общая функция цикла управления ордером
pub(super) async fn manage_order_loop<'a, E>(
    params: OrderLoopParams<'a, E>,
) -> Result<(f64, Option<String>)> // Возвращает (финальное исполненное количество, ID последнего ордера)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let OrderLoopParams {
        hedger,
        db,
        operation_id,
        symbol, // Это уже готовый символ для API (spot или futures)
        side,
        initial_target_qty,
        initial_limit_price,
        progress_callback,
        stage,
        is_spot,
        min_order_qty_decimal,
        total_filled_qty_storage,
    } = params;

    let mut cumulative_filled_qty = *total_filled_qty_storage.lock().await; // Начинаем с того, что уже есть
    let mut current_order_target_qty = initial_target_qty - cumulative_filled_qty; // Сколько осталось для первого ордера
    let mut qty_filled_in_current_order = 0.0;
    let mut current_order_id: Option<String> = None;
    let mut limit_price = initial_limit_price; // Цена для текущего ордера
    let mut last_placed_order_id: Option<String> = None; // Храним ID последнего *успешно размещенного* ордера
    // Используем config для доступа к slippage
    let mut current_market_price = initial_limit_price / (1.0 - hedger.config.slippage * side.sign()); // Примерная рыночная цена

    // --- Размещение начального ордера ---
    if current_order_target_qty <= ORDER_FILL_TOLERANCE {
        info!(
            "op_id:{}: Stage {:?} target already reached ({:.8}/{:.8}). Skipping placement.",
            operation_id, stage, cumulative_filled_qty, initial_target_qty
        );
        return Ok((cumulative_filled_qty, None)); // Возвращаем None, т.к. ордер не размещался
     }

    info!(
        "op_id:{}: Placing initial {} {} order at {:.8} for qty {:.8} (Stage: {:?})",
        operation_id,
        if is_spot { "spot" } else { "futures" },
        side,
        limit_price,
        current_order_target_qty,
        stage
    );

    let order_result = place_order(
        hedger.exchange.clone(), // Клонируем для передачи в функцию
        symbol,
        side,
        current_order_target_qty,
        limit_price,
        is_spot,
    )
    .await;

    let order_id = match order_result {
        Ok(id) => id,
        Err(e) => {
            error!(
                "op_id:{}: Failed place initial {} order (Stage: {:?}): {}",
                operation_id,
                if is_spot { "spot" } else { "futures" },
                stage,
                e
            );
            return Err(e);
        }
    };
    info!(
        "op_id:{}: Placed initial {} order: id={} (Stage: {:?})",
        operation_id,
        if is_spot { "spot" } else { "futures" },
        order_id,
        stage
    );
    current_order_id = Some(order_id.clone());
    last_placed_order_id = current_order_id.clone();
    // Обновляем БД, если это hedge spot
    if is_spot {
        if let Err(e) = update_hedge_spot_order(db, operation_id, current_order_id.as_deref(), cumulative_filled_qty).await {
             error!("op_id:{}: Failed update initial spot order info in DB: {}", operation_id, e);
        }
     }


    let mut start_of_current_order = Instant::now(); // Таймер для текущего ордера
    let mut last_update_sent = Instant::now();
    let update_interval = Duration::from_secs(5);
    // --- ДОБАВЛЕНО: Интервал для проверки "свежести" цены ---
    let price_check_interval = Duration::from_secs(5); // Проверяем цену каждые 5 секунд
    let mut last_price_check = Instant::now();
    // --- КОНЕЦ ДОБАВЛЕНИЯ ---

    // --- Основной цикл управления ордером ---
    loop {
        sleep(Duration::from_millis(500)).await; // Пауза между проверками
        let now = Instant::now();
        let id_to_check_opt = current_order_id.clone();

        // Получаем ID для проверки
        let order_id_to_check = match id_to_check_opt {
            Some(id) => id,
            None => {
                // Если ID нет, проверяем, достигнута ли цель
                if cumulative_filled_qty >= initial_target_qty - ORDER_FILL_TOLERANCE {
                    info!(
                        "op_id:{}: No active {} order and target reached. Exiting loop. (Stage: {:?})",
                        operation_id, if is_spot { "spot" } else { "futures" }, stage
                    );
                    break Ok((cumulative_filled_qty, last_placed_order_id)); // Возвращаем последнее ID
                } else {
                    // Цель не достигнута, а ордера нет - это проблема, если прошло время
                    if now.duration_since(start_of_current_order) > Duration::from_secs(2) { // Используем start_of_current_order
                        warn!(
                            "op_id:{}: No active {} order ID, but target not reached ({:.8}/{:.8}). Aborting stage. (Stage: {:?})",
                            operation_id, if is_spot { "spot" } else { "futures" }, cumulative_filled_qty, initial_target_qty, stage
                        );
                        return Err(anyhow!(
                            "No active {} order, but target not reached after fill/cancellation (Stage: {:?})",
                             if is_spot { "spot" } else { "futures" }, stage
                        ));
                    } else {
                        debug!("op_id:{}: {} Order ID is None shortly after placement/cancel? Waiting. (Stage: {:?})", operation_id, if is_spot { "spot" } else { "futures" }, stage);
                        continue; // Ждем появления ID или выхода
                    }
                }
            }
        };

        // --- Получение статуса ордера ---
        let status_result = get_order_status(
            hedger.exchange.clone(),
            symbol,
            &order_id_to_check,
            is_spot,
        )
        .await;

        let status: ExchangeOrderStatus = match status_result {
            Ok(s) => s,
            Err(e) => {
                // Обработка "Order not found"
                if e.to_string().contains("Order not found")
                    && now.duration_since(start_of_current_order) > Duration::from_secs(5) // Используем start_of_current_order
                {
                    warn!(
                        "op_id:{}: {} Order {} not found after delay, assuming it filled for its target qty {:.8}. Continuing... (Stage: {:?})",
                        operation_id, if is_spot { "spot" } else { "futures" }, order_id_to_check, current_order_target_qty, stage
                    );
                    let assumed_filled_in_this_order = current_order_target_qty;
                    let filled_before = cumulative_filled_qty;
                    cumulative_filled_qty += assumed_filled_in_this_order;
                    cumulative_filled_qty = cumulative_filled_qty.min(initial_target_qty);
                    let filled_diff = cumulative_filled_qty - filled_before;

                    if filled_diff.abs() > ORDER_FILL_TOLERANCE {
                         *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                         if is_spot {
                             if let Err(db_err) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                 error!("op_id:{}: Failed update DB after {} order not found: {}", operation_id, if is_spot { "spot" } else { "futures" }, db_err);
                             }
                         }
                    }
                    current_order_id = None;
                    qty_filled_in_current_order = 0.0;

                    if cumulative_filled_qty >= initial_target_qty - ORDER_FILL_TOLERANCE {
                        info!(
                            "op_id:{}: {} target reached after order not found assumption. Exiting loop. (Stage: {:?})",
                            operation_id, if is_spot { "spot" } else { "futures" }, stage
                        );
                        break Ok((cumulative_filled_qty, last_placed_order_id));
                    } else {
                        warn!(
                            "op_id:{}: {} target not reached after assumption. Triggering replacement. (Stage: {:?})",
                            operation_id, if is_spot { "spot" } else { "futures" }, stage
                        );
                        // Используем config для доступа к max_wait
                        start_of_current_order = now - Duration::from_secs(hedger.config.max_wait_secs) - Duration::from_secs(1); // Форсируем замену
                        last_price_check = start_of_current_order; // Сбрасываем и проверку цены
                        continue;
                    }
                } else {
                    warn!(
                        "op_id:{}: Failed to get {} order status for {}: {}. Aborting stage. (Stage: {:?})",
                        operation_id, if is_spot { "spot" } else { "futures" }, order_id_to_check, e, stage
                    );
                    return Err(anyhow!(
                        "Failed during {} status check for {}: {} (Stage: {:?})",
                        if is_spot { "spot" } else { "futures" }, order_id_to_check, e, stage
                    ));
                }
            }
        };

        // --- Обновление исполненного количества ---
        let previously_filled_in_current = qty_filled_in_current_order;
        qty_filled_in_current_order = status.filled_qty;
        let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current;

        if filled_since_last_check.abs() > ORDER_FILL_TOLERANCE {
            let filled_before = cumulative_filled_qty;
            cumulative_filled_qty += filled_since_last_check;
            cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_target_qty * 1.00001);
            let filled_diff = cumulative_filled_qty - filled_before;

            if filled_diff.abs() > ORDER_FILL_TOLERANCE {
                *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                debug!(
                    "op_id:{}: {} fill update. Filled in current: {:.8}, Cum: {:.8}/{:.8} (Stage: {:?})",
                    operation_id, if is_spot { "spot" } else { "futures" }, qty_filled_in_current_order, cumulative_filled_qty, initial_target_qty, stage
                );
                if is_spot {
                    if let Err(e) = update_hedge_spot_order(
                        db,
                        operation_id,
                        current_order_id.as_deref(),
                        cumulative_filled_qty,
                    )
                    .await
                    {
                        error!(
                            "op_id:{}: Failed to update {} filled qty in DB: {}",
                            operation_id, if is_spot { "spot" } else { "futures" }, e
                        );
                    }
                }
            }
        }

        // --- Проверка полного исполнения ордера ---
        if status.remaining_qty <= ORDER_FILL_TOLERANCE {
            info!(
                "op_id:{}: {} order {} considered filled (remaining: {:.8}). (Stage: {:?})",
                operation_id, if is_spot { "spot" } else { "futures" }, order_id_to_check, status.remaining_qty, stage
            );
            if (cumulative_filled_qty - initial_target_qty).abs() > ORDER_FILL_TOLERANCE && cumulative_filled_qty < initial_target_qty {
                 warn!(
                     "op_id:{}: {} final fill correction after order fill: {:.8} -> {:.8}. (Stage: {:?})",
                     operation_id, if is_spot { "spot" } else { "futures" }, cumulative_filled_qty, initial_target_qty, stage
                 );
                 cumulative_filled_qty = initial_target_qty;
                 *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                 if is_spot {
                     // Используем as_deref() для Option<String>
                     if let Err(e) = update_hedge_spot_order(db, operation_id, current_order_id.as_deref(), cumulative_filled_qty).await {
                         error!("op_id:{}: Failed update DB after final fill correction: {}", operation_id, e);
                     }
                 }
            }
            current_order_id = None;
            qty_filled_in_current_order = 0.0;
            if cumulative_filled_qty >= initial_target_qty - ORDER_FILL_TOLERANCE {
                 info!("op_id:{}: Target reached after order fill. Exiting loop. (Stage: {:?})", operation_id, stage);
                 break Ok((cumulative_filled_qty, last_placed_order_id));
            } else {
                 warn!("op_id:{}: Order filled but target not reached? Triggering replacement check. (Stage: {:?})", operation_id, stage);
                 // Используем config для доступа к max_wait
                 start_of_current_order = now - Duration::from_secs(hedger.config.max_wait_secs) - Duration::from_secs(1);
                 last_price_check = start_of_current_order; // Сбрасываем и проверку цены
                 continue;
            }
        }

        // --- Логика перестановки ордера ---
        let elapsed_since_order_start = now.duration_since(start_of_current_order);
        let mut should_replace = false; // Флаг для решения о замене
        let mut is_replacement = false; // Флаг для колбэка

        // 1. Проверка по таймауту max_wait
        // Используем config для доступа к max_wait
        if elapsed_since_order_start > Duration::from_secs(hedger.config.max_wait_secs) {
            warn!(
                "op_id:{}: {} order {} MAX_WAIT timeout (elapsed: {:?}). Triggering replacement. (Stage: {:?})",
                operation_id, if is_spot { "spot" } else { "futures" }, order_id_to_check, elapsed_since_order_start, stage
            );
            should_replace = true;
        }
        // 2. Проверка по интервалу price_check_interval и "свежести" цены
        else if now.duration_since(last_price_check) > price_check_interval {
            debug!("op_id:{}: Checking price relevance for {} order {} (elapsed: {:?})...", operation_id, if is_spot { "spot" } else { "futures" }, order_id_to_check, elapsed_since_order_start);
            last_price_check = now; // Сбрасываем таймер проверки цены

            match get_market_price(hedger.exchange.clone(), symbol, is_spot).await {
                Ok(market_price) => {
                    current_market_price = market_price; // Обновляем текущую рыночную цену
                    // Используем config для доступа к slippage
                    let price_diff_threshold = hedger.config.slippage * 2.0; // Порог в 2 раза больше slippage
                    let is_stale = match side {
                        OrderSide::Buy => limit_price < market_price * (1.0 - price_diff_threshold),
                        OrderSide::Sell => limit_price > market_price * (1.0 + price_diff_threshold),
                    };

                    if is_stale {
                        warn!(
                            "op_id:{}: {} order {} price {:.8} is stale vs market {:.8}. Triggering replacement. (Stage: {:?})",
                            operation_id, if is_spot { "spot" } else { "futures" }, order_id_to_check, limit_price, market_price, stage
                        );
                        should_replace = true;
                    } else {
                         debug!("op_id:{}: Price {:.8} is still relevant vs market {:.8}.", operation_id, limit_price, market_price);
                    }
                }
                Err(e) => {
                    warn!("op_id:{}: Failed to get market price for relevance check: {}. Skipping check.", operation_id, e);
                }
            }
        }

        // --- Выполнение замены, если флаг установлен ---
        if should_replace && status.remaining_qty > ORDER_FILL_TOLERANCE {
            is_replacement = true; // Устанавливаем флаг для колбэка

            let remaining_total_qty = initial_target_qty - cumulative_filled_qty;
            let remaining_total_qty_decimal = Decimal::from_f64(remaining_total_qty).unwrap_or_default();

            // --- Проверка на пыль (только для unhedge spot) ---
            if is_spot && side == OrderSide::Sell && min_order_qty_decimal.is_some() {
                if remaining_total_qty > ORDER_FILL_TOLERANCE
                    && remaining_total_qty_decimal < min_order_qty_decimal.unwrap()
                {
                    warn!(
                        "op_id:{}: Unhedge spot remaining qty {:.8} is dust (min: {}). Ignoring. (Stage: {:?})",
                        operation_id, remaining_total_qty, min_order_qty_decimal.unwrap(), stage
                    );
                    if let Err(e) = cancel_order(hedger.exchange.clone(), symbol, &order_id_to_check, is_spot).await {
                        warn!("op_id:{}: Failed cancel dust {} order {}: {}", operation_id, if is_spot { "spot" } else { "futures" }, order_id_to_check, e);
                    } else {
                        info!("op_id:{}: Cancel request sent for dust {} order {}", operation_id, if is_spot { "spot" } else { "futures" }, order_id_to_check);
                        sleep(Duration::from_millis(500)).await;
                    }
                    current_order_id = None;
                    qty_filled_in_current_order = 0.0;
                    // Перепроверка исполнения после отмены на всякий случай
                    match get_order_status(hedger.exchange.clone(), symbol, &order_id_to_check, is_spot).await {
                        Ok(fs) => {
                            let filled_after_cancel = fs.filled_qty - previously_filled_in_current;
                            if filled_after_cancel > ORDER_FILL_TOLERANCE {
                                let filled_before = cumulative_filled_qty;
                                cumulative_filled_qty += filled_after_cancel;
                                cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_target_qty * 1.00001);
                                if (cumulative_filled_qty - filled_before).abs() > ORDER_FILL_TOLERANCE {
                                    *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                                    if is_spot {
                                         if let Err(e) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                             error!("op_id:{}: Failed update DB after dust cancel fill: {}", operation_id, e);
                                         }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!("op_id:{}: Failed get final status after dust cancel: {}", operation_id, e);
                        }
                    }
                    break Ok((cumulative_filled_qty, last_placed_order_id));
                }
            }
            // --- Конец проверки на пыль ---

            // --- Отмена текущего ордера ---
            if let Err(e) = cancel_order(hedger.exchange.clone(), symbol, &order_id_to_check, is_spot).await {
                warn!(
                    "op_id:{}: Failed cancel {} order {}: {}. Will attempt re-check and replacement. (Stage: {:?})",
                    operation_id, if is_spot { "spot" } else { "futures" }, order_id_to_check, e, stage
                );
                sleep(Duration::from_millis(200)).await;
            } else {
                info!(
                    "op_id:{}: Sent cancel request for {} order {}",
                    operation_id, if is_spot { "spot" } else { "futures" }, order_id_to_check
                );
                sleep(Duration::from_millis(500)).await;
            }
            let previous_order_id = current_order_id.take();
            qty_filled_in_current_order = 0.0;

            // --- Перепроверка статуса после отмены ---
            if let Some(prev_id) = previous_order_id {
                 match get_order_status(hedger.exchange.clone(), symbol, &prev_id, is_spot).await {
                    Ok(final_status) => {
                        let filled_after_cancel = final_status.filled_qty - previously_filled_in_current;
                        if filled_after_cancel > ORDER_FILL_TOLERANCE {
                            info!(
                                "op_id:{}: Order {} filled further ({}) during/after cancel. (Stage: {:?})",
                                operation_id, prev_id, filled_after_cancel, stage
                            );
                            let filled_before = cumulative_filled_qty;
                            cumulative_filled_qty += filled_after_cancel;
                            cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_target_qty * 1.00001);
                             if (cumulative_filled_qty - filled_before).abs() > ORDER_FILL_TOLERANCE {
                                *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                                if is_spot {
                                     if let Err(e) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                         error!("op_id:{}: Failed update DB after cancel fill: {}", operation_id, e);
                                     }
                                }
                            }
                        }
                        // --- ИСПРАВЛЕНО: Условие выхода после отмены ---
                        // Проверяем, достигнута ли цель ПОСЛЕ обновления cumulative_filled_qty
                        if cumulative_filled_qty >= initial_target_qty - ORDER_FILL_TOLERANCE {
                            info!(
                                "op_id:{}: Target reached after checking cancelled order {}. Exiting loop. (Stage: {:?})", // Уточнили лог
                                operation_id, prev_id, stage
                            );
                            // Финальная коррекция до цели, если нужно (остается)
                            if (cumulative_filled_qty - initial_target_qty).abs() > ORDER_FILL_TOLERANCE && cumulative_filled_qty < initial_target_qty {
                                warn!(
                                    "op_id:{}: Final fill correction after cancel check: {:.8} -> {:.8}. (Stage: {:?})",
                                    operation_id, cumulative_filled_qty, initial_target_qty, stage
                                );
                                cumulative_filled_qty = initial_target_qty;
                                *total_filled_qty_storage.lock().await = cumulative_filled_qty; // Обновляем хранилище
                                if is_spot {
                                    if let Err(e) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                        error!("op_id:{}: Failed update DB after final cancel correction: {}", operation_id, e);
                                    }
                                }
                            }
                            break Ok((cumulative_filled_qty, last_placed_order_id)); // Выходим, так как цель достигнута
                        } else {
                            // Цель НЕ достигнута, даже если remaining_qty == 0 из-за отмены.
                            // Просто логируем оставшееся количество (если оно есть) и продолжаем к замене.
                            info!(
                                "op_id:{}: Target NOT reached ({:.8}/{:.8}) after checking cancelled order {}. Proceeding with replacement. (Stage: {:?})",
                                operation_id, cumulative_filled_qty, initial_target_qty, prev_id, stage
                            );
                            // Ничего не делаем, цикл продолжится и перейдет к размещению нового ордера
                        }
                        // --- КОНЕЦ ИСПРАВЛЕНИЯ ---
                    }
                    Err(e) => {
                         if !e.to_string().contains("Order not found") {
                            warn!(
                                "op_id:{}: Failed get {} order status after cancel for {}: {}. Assuming processed. (Stage: {:?})",
                                operation_id, if is_spot { "spot" } else { "futures" }, prev_id, e, stage
                            );
                         } else {
                             info!("op_id:{}: Order {} not found after cancel, assuming processed. (Stage: {:?})", operation_id, prev_id, stage);
                         }
                         if cumulative_filled_qty >= initial_target_qty - ORDER_FILL_TOLERANCE {
                             info!("op_id:{}: Target reached after order cancel/not found. Exiting loop. (Stage: {:?})", operation_id, stage);
                             break Ok((cumulative_filled_qty, last_placed_order_id));
                         }
                    }
                }
            }


            // --- Пересчет остатка и размещение нового ордера ---
            let remaining_total_qty = initial_target_qty - cumulative_filled_qty;
            if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                info!("op_id:{}: Remaining qty {:.8} is negligible after cancel/recheck. Exiting loop. (Stage: {:?})", operation_id, remaining_total_qty, stage);
                break Ok((cumulative_filled_qty, last_placed_order_id));
            }

            // Получаем новую цену (если еще не получили при проверке свежести)
            if !should_replace { // should_replace был false, значит, цена не проверялась
                 current_market_price = match get_market_price(hedger.exchange.clone(), symbol, is_spot).await {
                    Ok(p) => p,
                    Err(e) => {
                        error!("op_id:{}: Failed to get new market price for replacement: {}. Aborting stage.", operation_id, e);
                        return Err(anyhow!("Failed get price for replacement: {}", e));
                    }
                };
            } // Иначе используем current_market_price, полученную при проверке свежести

            // Используем config для доступа к slippage
            limit_price = calculate_limit_price(current_market_price, side, hedger.config.slippage);
            current_order_target_qty = remaining_total_qty;

            info!("op_id:{}: Placing new {} {} order at {:.8} for remaining qty {:.8} (Stage: {:?})", operation_id, if is_spot { "spot" } else { "futures" }, side, limit_price, current_order_target_qty, stage);
            let new_order_id = place_order(hedger.exchange.clone(), symbol, side, current_order_target_qty, limit_price, is_spot).await?;
            info!("op_id:{}: Placed replacement {} order: id={} (Stage: {:?})", operation_id, if is_spot { "spot" } else { "futures" }, new_order_id, stage);
            current_order_id = Some(new_order_id.clone());
            last_placed_order_id = current_order_id.clone();
            if is_spot {
                if let Err(e) = update_hedge_spot_order(db, operation_id, current_order_id.as_deref(), cumulative_filled_qty).await {
                    error!("op_id:{}: Failed update DB after replacement order placement: {}", operation_id, e);
                }
             }

            start_of_current_order = now; // Сбрасываем таймер для нового ордера
            last_price_check = now; // Сбрасываем и таймер проверки цены
        }

        // --- Вызов колбэка прогресса ---
        let elapsed_since_update = now.duration_since(last_update_sent);
        if is_replacement || elapsed_since_update > update_interval {
            let price_for_cb = current_market_price;

            let update = HedgeProgressUpdate {
                stage,
                current_spot_price: price_for_cb,
                new_limit_price: limit_price,
                is_replacement,
                filled_qty: qty_filled_in_current_order,
                target_qty: current_order_target_qty,
                cumulative_filled_qty,
                total_target_qty: initial_target_qty,
            };
            tokio::task::yield_now().await;
            if let Err(e) = progress_callback(update).await {
                if !e.to_string().contains("message is not modified") {
                    warn!("op_id:{}: Progress callback failed (Stage: {:?}): {}", operation_id, stage, e);
                }
            }
            last_update_sent = now;
        }
        // --- Конец вызова колбэка ---

    } // --- Конец основного цикла loop ---
}


// --- Вспомогательные асинхронные функции для работы с биржей ---

async fn place_order<E: Exchange>(
    exchange: E,
    symbol: &str,
    side: OrderSide,
    qty: f64,
    price: f64,
    is_spot: bool,
) -> Result<String> { // --- Возвращаем String (ID ордера) ---
    if is_spot {
        let order_info = exchange.place_limit_order(symbol, side, qty, price).await?;
        Ok(order_info.id)
    } else {
        let order_info = exchange
            .place_futures_limit_order(symbol, side, qty, price)
            .await?;
        Ok(order_info.id)
    }
}
async fn get_order_status<E: Exchange>(
    exchange: E,
    symbol: &str,
    order_id: &str,
    is_spot: bool,
) -> Result<ExchangeOrderStatus> {
    if is_spot {
        exchange.get_spot_order_status(symbol, order_id).await
    } else {
        exchange.get_futures_order_status(symbol, order_id).await
    }
}

async fn cancel_order<E: Exchange>(
    exchange: E,
    symbol: &str,
    order_id: &str,
    is_spot: bool,
) -> Result<()> {
    if is_spot {
        exchange.cancel_spot_order(symbol, order_id).await
    } else {
        exchange.cancel_futures_order(symbol, order_id).await
    }
}

async fn get_market_price<E: Exchange>(
    exchange: E,
    symbol: &str, // Символ для API (spot или futures)
    is_spot: bool,
) -> Result<f64> {
    let price = if is_spot {
        exchange.get_spot_price(symbol).await?
    } else {
        // Для фьючерса берем среднюю цену между бидом и аском или последнюю цену
        match exchange.get_futures_ticker(symbol).await {
            Ok(ticker) if ticker.bid_price > 0.0 && ticker.ask_price > 0.0 => (ticker.bid_price + ticker.ask_price) / 2.0,
            Ok(ticker) => {
                 warn!("Invalid ticker prices bid={}, ask={}. Using last_price if available.", ticker.bid_price, ticker.ask_price);
                 if ticker.last_price > 0.0 {
                     ticker.last_price
                 } else {
                     warn!("Falling back to spot price due to invalid futures ticker data.");
                     get_spot_price_fallback(&exchange, symbol).await?
                 }
            }
            Err(e) => {
                 warn!("Failed get futures ticker for price: {}. Falling back to spot price.", e);
                 get_spot_price_fallback(&exchange, symbol).await?
            }
        }
    };
    if price <= 0.0 {
        Err(anyhow!("Invalid market price received: {}", price))
    } else {
        Ok(price)
    }
}

// --- Helper for spot price fallback in get_market_price ---
async fn get_spot_price_fallback<E: Exchange>(exchange: &E, futures_symbol: &str) -> Result<f64> {
    // TODO: Implement a robust way to get base symbol from futures symbol
    // --- ИСПРАВЛЕНО: Используем quote_currency из exchange ---
    // Получаем quote_currency. В идеале, он должен быть в структуре Hedger или Config.
    // Здесь предполагаем, что можем получить его как-то из exchange (если он там есть)
    // или передать как параметр. Пока что хардкодим USDT как пример.
    // let quote_currency = exchange.get_quote_currency(); // Примерный метод
    let quote_currency = "USDT"; // ЗАГЛУШКА! Нужно получить реальный quote_currency
    // --- КОНЕЦ ИСПРАВЛЕНИЯ ---

    let base_symbol = futures_symbol.trim_end_matches(quote_currency).trim_end_matches("PERP");
    if base_symbol == futures_symbol {
        return Err(anyhow!("Could not determine base symbol from futures symbol '{}' for fallback", futures_symbol));
    }
    exchange.get_spot_price(base_symbol).await
}


// --- Вспомогательные синхронные функции ---
pub(super) fn calculate_limit_price(market_price: f64, side: OrderSide, slippage: f64) -> f64 {
    market_price * (1.0 - slippage * side.sign()) // Buy: ниже рынка, Sell: выше рынка
}

// Расширяем OrderSide для получения знака
trait SideSign {
    fn sign(&self) -> f64;
    fn opposite(&self) -> Self; // Added opposite helper
}
impl SideSign for OrderSide {
    fn sign(&self) -> f64 {
        match self {
            OrderSide::Buy => -1.0,
            OrderSide::Sell => 1.0,
        }
    }
    fn opposite(&self) -> Self {
        match self {
            OrderSide::Buy => OrderSide::Sell,
            OrderSide::Sell => OrderSide::Buy,
        }
    }
}
