// src/hedger/hedge.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::sleep;
use tracing::{error, info, warn};

// --- ИСПРАВЛЕНО: manage_order_loop теперь возвращает Result<(f64, Option<String>)> ---
use super::common::{calculate_limit_price, manage_order_loop, OrderLoopParams};
use super::{
    HedgeParams, HedgeProgressCallback, HedgeProgressUpdate, HedgeStage, Hedger,
    ORDER_FILL_TOLERANCE,
};
use crate::exchange::types::{DetailedOrderStatus, OrderSide, OrderStatusText}; // Добавили OrderStatusText
use crate::exchange::Exchange;
use crate::storage::{update_hedge_final_status, update_hedge_spot_order, Db};

// Вспомогательная функция для округления ВНИЗ
fn round_down_to_precision(value: f64, decimals: u32) -> Result<Decimal> {
    // Конвертируем f64 в строку, затем в Decimal
    let decimal_value = Decimal::from_str(&value.to_string())
        .map_err(|e| anyhow!("Invalid f64 value for decimal conversion: {} ({})", value, e))?
        // Округляем вниз до нужного количества знаков после запятой
        .trunc_with_scale(decimals)
        // Нормализуем (убираем лишние нули)
        .normalize();
    // Возвращаем успешный результат, обернутый в Ok
    Ok(decimal_value)
}

// Реализация основной логики хеджирования
pub(super) async fn run_hedge_impl<ExchangeType>(
    hedger: &Hedger<ExchangeType>,
    params: HedgeParams,
    mut progress_callback: HedgeProgressCallback,
    total_filled_spot_quantity_storage: Arc<TokioMutex<f64>>,
    operation_identifier: i64,
    database: &Db,
) -> Result<(f64, f64, f64)>
where
    ExchangeType: Exchange + Clone + Send + Sync + 'static,
{
    let HedgeParams {
        spot_order_qty: initial_spot_quantity,
        fut_order_qty: _initial_futures_quantity, // Используется только для оценки плеча
        current_spot_price,
        initial_limit_price: initial_spot_limit_price,
        symbol,
        spot_value: _estimated_spot_value, // Не используется напрямую, т.к. есть динамический расчет
        available_collateral,
        min_spot_qty_decimal: _min_spot_quantity_decimal, // Не используется напрямую
        min_fut_qty_decimal: min_futures_quantity_decimal,
        spot_decimals: _spot_quantity_decimals, // Не используется напрямую
        fut_decimals: futures_quantity_decimals,
        futures_symbol,
    } = params;

    info!(
        "op_id:{}: Running hedge for {} with spot target={:.8}, initial futures estimate={:.8}",
        operation_identifier, symbol, initial_spot_quantity, _initial_futures_quantity
    );

    // --- Проверка и установка плеча ---
    // Используем начальную оценку фьючерса для расчета плеча
    let futures_position_value_estimate = _initial_futures_quantity * current_spot_price;
    if available_collateral <= 0.0 {
        let error_message = "Available collateral is non-positive".to_string();
        error!("op_id:{}: {}. Aborting.", operation_identifier, error_message);
        let _ = update_hedge_final_status(database, operation_identifier, "Failed", None, 0.0, Some(&error_message)).await;
        return Err(anyhow!(error_message));
    }
    let required_leverage = futures_position_value_estimate / available_collateral;
    info!(
        "op_id:{}: Confirming required leverage based on estimate: {:.2}x",
        operation_identifier, required_leverage
    );
    if required_leverage.is_nan() || required_leverage.is_infinite() || required_leverage <= 0.0 {
        let error_message = format!("Invalid required leverage calculation: {}", required_leverage);
        error!("op_id:{}: {}. Aborting.", operation_identifier, error_message);
        let _ = update_hedge_final_status(database, operation_identifier, "Failed", None, 0.0, Some(&error_message)).await;
        return Err(anyhow!(error_message));
    }

    // Используем futures_symbol для установки плеча
    match set_leverage_if_needed(hedger, &futures_symbol, required_leverage, operation_identifier, database).await {
        Ok(_) => info!("op_id:{}: Leverage check/set successful for {}.", operation_identifier, futures_symbol),
        Err(error) => {
            // Ошибка уже залогирована и статус обновлен в set_leverage_if_needed
            return Err(error);
        }
    }
    // --- Конец проверки плеча ---

    // --- Этап 1: Спот ---
    info!("op_id:{}: Starting SPOT buy stage...", operation_identifier);
    *total_filled_spot_quantity_storage.lock().await = 0.0; // Сбрасываем счетчик перед циклом

    let spot_loop_params = OrderLoopParams {
        hedger,
        db: database,
        operation_id: operation_identifier,
        symbol: &symbol, // Спотовый символ
        side: OrderSide::Buy,
        initial_target_qty: initial_spot_quantity,
        initial_limit_price: initial_spot_limit_price,
        progress_callback: &mut progress_callback,
        stage: HedgeStage::Spot,
        is_spot: true,
        min_order_qty_decimal: None, // Минимальный размер проверяется внутри цикла
        total_filled_qty_storage: Arc::clone(&total_filled_spot_quantity_storage), // Передаем хранилище общего кол-ва
    };

    let (final_spot_quantity_gross, last_spot_order_id_option) = match manage_order_loop(spot_loop_params).await {
        Ok((filled_quantity, last_order_id_opt)) => {
            match last_order_id_opt {
                 Some(id) if !id.is_empty() => {
                    info!(
                        "op_id:{}: Hedge SPOT buy stage finished. Final actual spot gross quantity: {:.8}, Last Order ID: {}",
                        operation_identifier,
                        filled_quantity,
                        id
                    );
                    if (filled_quantity - initial_spot_quantity).abs() > ORDER_FILL_TOLERANCE * 10.0 {
                        warn!(
                            "op_id:{}: Final SPOT gross filled {:.8} significantly differs from target {:.8}. Using actual filled.",
                            operation_identifier, filled_quantity, initial_spot_quantity
                        );
                    }
                     if let Err(e) = update_hedge_spot_order(
                         database,
                         operation_identifier,
                         Some(&id),
                         filled_quantity,
                     )
                     .await
                     {
                         error!(
                             "op_id:{}: Failed to update FINAL spot order info in DB (after loop): {}",
                             operation_identifier, e
                         );
                     }
                     (filled_quantity, Some(id))
                 }
                _ => {
                    let error_message = "Spot order loop succeeded but failed to return order ID.".to_string();
                    error!("op_id:{}: {}", operation_identifier, error_message);
                    let _ = update_hedge_final_status(
                        database,
                        operation_identifier,
                        "Failed",
                        None,
                        filled_quantity,
                        Some(&error_message),
                    )
                    .await;
                    return Err(anyhow!(error_message));
                }
            }
        }
        Err(loop_error) => {
            error!(
                "op_id:{}: Hedge SPOT buy stage failed: {}",
                operation_identifier, loop_error
            );
            let current_filled_quantity = *total_filled_spot_quantity_storage.lock().await;
            let _ = update_hedge_final_status(
                database,
                operation_identifier,
                "Failed",
                None,
                current_filled_quantity,
                Some(&format!("Spot stage failed: {}", loop_error)),
            )
            .await;
            return Err(loop_error);
        }
    };

    let final_spot_order_id = match last_spot_order_id_option {
        Some(id) => id,
        None => {
             error!("op_id:{}: Critical error - last spot order identifier is None after spot stage completion.", operation_identifier);
             let error_message = "Failed to retrieve last spot order ID internally".to_string();
              let _ = update_hedge_final_status(database, operation_identifier, "Failed", None, final_spot_quantity_gross, Some(&error_message)).await;
             return Err(anyhow!(error_message));
        }
    };

    // --- Получение точной стоимости исполненного спота ---
    info!("op_id:{}: Fetching execution details for last spot order {}...", operation_identifier, final_spot_order_id);
    // Запрашиваем детали в основном для получения средней цены последнего(их) исполнения
    let detailed_spot_status: DetailedOrderStatus = match hedger.exchange.get_spot_order_execution_details(&symbol, &final_spot_order_id).await {
         Ok(details) => details,
         Err(error) => {
             // Если детали не получены, логируем предупреждение и используем запасной вариант
             warn!("op_id:{}: Failed get spot execution details for order {}: {}. Will use loop quantity and current price for value estimation.", operation_identifier, final_spot_order_id, error);
             // Создаем "пустой" статус; стоимость будет пересчитана с запасной ценой
             DetailedOrderStatus {
                 order_id: "".to_string(), // Placeholder for missing order ID
                 symbol: symbol.clone(), // Use the symbol from the parameters
                 side: OrderSide::Buy, // Assuming the side is Buy for this context
                 last_filled_price: Some(0.0), // Placeholder for last filled price
                 last_filled_qty: Some(0.0), // Placeholder for last filled quantity
                 reject_reason: None, // No reject reason in this context
                 filled_qty: 0.0, // Будет проигнорировано
                 remaining_qty: 0.0,
                 cumulative_executed_value: 0.0, // Будет пересчитано
                 average_price: 0.0, // Заставит использовать запасную цену
                 status_text: OrderStatusText::Unknown("DetailsFailed".to_string()), // Используем OrderStatusText
             }
         }
    };

    // Логируем расхождение (ожидаемо при заменах)
    // Используем info вместо warn, т.к. это ожидаемо при заменах
    if detailed_spot_status.filled_qty > ORDER_FILL_TOLERANCE && (detailed_spot_status.filled_qty - final_spot_quantity_gross).abs() > ORDER_FILL_TOLERANCE {
        info!(
            "op_id:{}: Last order ({}) filled qty {:.8} differs from total loop filled qty {:.8}. This is expected with replacements.",
            operation_identifier, final_spot_order_id, detailed_spot_status.filled_qty, final_spot_quantity_gross
        );
     }

    // --- ИСПРАВЛЕНО: Вычисление actual_spot_value ---
    // Используем ОБЩЕЕ исполненное количество из цикла и СРЕДНЮЮ цену из деталей ПОСЛЕДНЕГО ордера (как приближение)
    // или запасную текущую цену.
    let avg_price_for_value_calc = if detailed_spot_status.average_price > 0.0 {
        detailed_spot_status.average_price
    } else {
        warn!("op_id:{}: Average price from details of last order {} was zero or unavailable. Using current spot price as fallback for value calculation.", operation_identifier, final_spot_order_id);
        // Получаем текущую цену снова как запасной вариант
        match hedger.exchange.get_spot_price(&symbol).await {
             Ok(price) if price > 0.0 => price,
             _ => {
                 error!("op_id:{}: Failed to get fallback spot price. Using initial price from params.", operation_identifier);
                 current_spot_price // Запасной вариант - цена из параметров, если текущая не получена
             }
        }
    };

    // Считаем стоимость: ОБЩЕЕ КОЛИЧЕСТВО * СРЕДНЯЯ ЦЕНА (приближенная)
    let actual_spot_value = final_spot_quantity_gross * avg_price_for_value_calc;
    // --- КОНЕЦ ИСПРАВЛЕНИЯ ---

     if actual_spot_value <= ORDER_FILL_TOLERANCE / 10.0 { // Проверяем вычисленное значение
         let error_message = format!(
             "Calculated actual spot value is effectively zero ({:.8}). Cannot calculate dynamic futures quantity. (Total Qty: {:.8}, Avg Price Used: {:.8})",
             actual_spot_value, final_spot_quantity_gross, avg_price_for_value_calc
         );
         error!("op_id:{}: {}", operation_identifier, error_message);
         let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
         return Err(anyhow!(error_message));
     }
    info!("op_id:{}: Estimated actual executed spot value: {:.8} (Total Qty: {:.8}, Avg Price Used: {:.8})", // Уточнили лог
        operation_identifier, actual_spot_value, final_spot_quantity_gross, avg_price_for_value_calc);


    // --- Отправляем финальный колбэк для спота (100%) ---
    let spot_price_for_callback = avg_price_for_value_calc; // Используем ту же цену для колбэка

    let spot_done_update = HedgeProgressUpdate {
        stage: HedgeStage::Spot,
        current_spot_price: spot_price_for_callback,
        new_limit_price: initial_spot_limit_price,
        is_replacement: false,
        filled_qty: final_spot_quantity_gross,
        target_qty: final_spot_quantity_gross,
        cumulative_filled_qty: final_spot_quantity_gross,
        total_target_qty: initial_spot_quantity,
    };
    if let Err(error) = progress_callback(spot_done_update).await {
        if !error.to_string().contains("message is not modified") {
            warn!(
                "op_id:{}: Progress callback failed after spot fill: {}",
                operation_identifier, error
            );
        }
    }
    // --- Конец колбэка спота ---

    // --- Динамический Расчет Объема Фьючерса ---
    info!("op_id:{}: Calculating dynamic futures quantity...", operation_identifier);
    let futures_ticker = match hedger.exchange.get_futures_ticker(&futures_symbol).await {
         Ok(ticker) => ticker,
         Err(error) => {
              error!("op_id:{}: Failed get futures ticker for dynamic quantity calculation: {}. Aborting futures stage.", operation_identifier, error);
              let error_message = format!("Failed get futures ticker: {}", error);
              let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
              return Err(anyhow!(error_message));
         }
    };
    let futures_price_now = (futures_ticker.bid_price + futures_ticker.ask_price) / 2.0;
    if futures_price_now <= 0.0 {
         let error_message = format!("Invalid futures price for calculation: {:.2}", futures_price_now);
         error!("op_id:{}: {}", operation_identifier, error_message);
         let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
         return Err(anyhow!(error_message));
    }

    let dynamic_futures_quantity = actual_spot_value / futures_price_now; // Используем исправленное значение
    info!("op_id:{}: Calculated dynamic futures quantity: {:.12} (Spot Value: {:.8}, Fut Price: {:.8})",
          operation_identifier, dynamic_futures_quantity, actual_spot_value, futures_price_now);

    // --- Округление и проверка минимального размера фьючерса ---
    let rounded_dynamic_futures_quantity_decimal = match round_down_to_precision(dynamic_futures_quantity, futures_quantity_decimals) {
        Ok(decimal_value) => decimal_value,
        Err(error) => {
            error!("op_id:{}: Failed to round dynamic futures quantity: {}", operation_identifier, error);
            let error_message = format!("Failed to round fut qty: {}", error);
             let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
            return Err(anyhow!(error_message));
        }
    };

    if rounded_dynamic_futures_quantity_decimal <= Decimal::ZERO {
         let error_message = format!("Rounded dynamic futures quantity is zero or negative: {}", rounded_dynamic_futures_quantity_decimal);
         error!("op_id:{}: {}", operation_identifier, error_message);
         let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
         return Err(anyhow!(error_message));
    }

    if rounded_dynamic_futures_quantity_decimal < min_futures_quantity_decimal {
         let error_message = format!(
             "Calculated dynamic futures quantity {:.8} is less than minimum order size {}",
            rounded_dynamic_futures_quantity_decimal, min_futures_quantity_decimal
         );
         error!("op_id:{}: {}", operation_identifier, error_message);
         let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
         return Err(anyhow!(error_message));
    }

    let final_futures_target_quantity = match rounded_dynamic_futures_quantity_decimal.to_f64() {
        Some(value) => value,
        None => {
            let error_message = format!("Failed to convert final futures decimal {} back to f64", rounded_dynamic_futures_quantity_decimal);
             error!("op_id:{}: {}", operation_identifier, error_message);
            let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
            return Err(anyhow!(error_message));
        }
    };

    info!("op_id:{}: Final rounded futures target quantity: {:.8}", operation_identifier, final_futures_target_quantity);
    // --- Конец динамического расчета и проверки ---

    // --- Этап 2: Фьючерс ---
    info!("op_id:{}: Starting FUTURES sell stage with dynamic quantity {:.8}...", operation_identifier, final_futures_target_quantity);
    let futures_filled_storage = Arc::new(TokioMutex::new(0.0));
    // Используем config для доступа к slippage
    let futures_initial_limit_price =
        calculate_limit_price(futures_price_now, OrderSide::Sell, hedger.config.slippage);

    let futures_loop_params = OrderLoopParams {
        hedger,
        db: database,
        operation_id: operation_identifier,
        symbol: &futures_symbol,
        side: OrderSide::Sell,
        initial_target_qty: final_futures_target_quantity,
        initial_limit_price: futures_initial_limit_price,
        progress_callback: &mut progress_callback,
        stage: HedgeStage::Futures,
        is_spot: false,
        min_order_qty_decimal: Some(min_futures_quantity_decimal),
        total_filled_qty_storage: futures_filled_storage.clone(),
    };

    let (final_futures_quantity, _last_futures_order_id_option) = match manage_order_loop(futures_loop_params).await {
        Ok((filled_quantity, last_order_id_opt)) => {
            info!(
                "op_id:{}: Hedge FUTURES sell stage finished. Final actual futures net quantity: {:.8}",
                operation_identifier, filled_quantity
            );
             if (filled_quantity - final_futures_target_quantity).abs() > ORDER_FILL_TOLERANCE * 10.0 {
                warn!(
                    "op_id:{}: Final FUTURES net filled {:.8} significantly differs from dynamic target {:.8}. Using actual filled.",
                    operation_identifier, filled_quantity, final_futures_target_quantity
                );
            }
            (filled_quantity, last_order_id_opt)
        }
        Err(loop_error) => {
            error!(
                "op_id:{}: Hedge FUTURES sell stage failed: {}",
                operation_identifier, loop_error
            );
            let last_futures_filled_quantity = *futures_filled_storage.lock().await;
            let _ = update_hedge_final_status(
                database,
                operation_identifier,
                "Failed",
                Some(&final_spot_order_id),
                final_spot_quantity_gross,
                Some(&format!("Futures stage failed: {}", loop_error)),
                // None, // futures_order_id_on_fail
                // Some(last_futures_filled_quantity), // futures_filled_qty_on_fail
            )
            .await;
            return Err(loop_error);
        }
    };

    // --- Успешное завершение ---
    info!(
        "op_id:{}: Hedge completed successfully. Spot Gross: {:.8}, Fut Net: {:.8}, Actual Spot Value: {:.8}",
        operation_identifier, final_spot_quantity_gross, final_futures_quantity, actual_spot_value
    );
    let _ = update_hedge_final_status(
        database,
        operation_identifier,
        "Completed",
        Some(&final_spot_order_id),
        final_spot_quantity_gross,
        None,
        // _last_futures_order_id_option.as_deref(),
        // Some(final_futures_quantity),
    )
    .await;

    // --- Отправляем финальный колбэк для фьючерса (100%) ---
     let futures_price_for_callback = match hedger.exchange.get_futures_ticker(&futures_symbol).await {
         Ok(ticker) => (ticker.bid_price + ticker.ask_price) / 2.0,
         Err(_) => futures_price_now
     };
    let futures_done_update = HedgeProgressUpdate {
        stage: HedgeStage::Futures,
        current_spot_price: futures_price_for_callback,
        new_limit_price: futures_initial_limit_price,
        is_replacement: false,
        filled_qty: final_futures_quantity,
        target_qty: final_futures_quantity,
        cumulative_filled_qty: final_futures_quantity,
        total_target_qty: final_futures_target_quantity,
    };
    if let Err(error) = progress_callback(futures_done_update).await {
         if !error.to_string().contains("message is not modified") {
            warn!("op_id:{}: Futures progress callback failed: {}", operation_identifier, error);
         }
    }
    // --- Конец колбэка фьючерса ---

    Ok((final_spot_quantity_gross, final_futures_quantity, actual_spot_value))
}

// Вспомогательная функция для установки плеча
async fn set_leverage_if_needed<ExchangeType>(
    hedger: &Hedger<ExchangeType>,
    futures_symbol: &str,
    required_leverage: f64,
    operation_identifier: i64,
    database: &Db,
) -> Result<()>
where
    ExchangeType: Exchange + Clone + Send + Sync + 'static,
{
    let current_leverage = match hedger.exchange.get_current_leverage(futures_symbol).await {
        Ok(leverage_value) => leverage_value,
        Err(error) => {
            let error_message = format!("Leverage check failed for {}: {}", futures_symbol, error);
            error!("op_id:{}: {}. Aborting.", operation_identifier, error_message);
            let _ = update_hedge_final_status(database, operation_identifier, "Failed", None, 0.0, Some(&error_message)).await;
            return Err(anyhow!(error_message));
        }
    };

    let target_leverage_to_set = (required_leverage.max(0.01) * 100.0).round() / 100.0;

    if (target_leverage_to_set - current_leverage).abs() > 0.01 {
        info!(
            "op_id:{}: Setting leverage for {} from {:.2}x to {:.2}x",
            operation_identifier, futures_symbol, current_leverage, target_leverage_to_set
        );
        if let Err(error) = hedger.exchange.set_leverage(futures_symbol, target_leverage_to_set).await {
            let error_message = format!("Failed to set leverage for {}: {}", futures_symbol, error);
            error!(
                "op_id:{}: Failed to set leverage to {:.2}x: {}. Aborting.",
                operation_identifier, target_leverage_to_set, error
            );
            let _ = update_hedge_final_status(database, operation_identifier, "Failed", None, 0.0, Some(&error_message)).await;
            return Err(anyhow!(error_message));
        }
        info!("op_id:{}: Leverage set successfully for {}.", operation_identifier, futures_symbol);
        sleep(Duration::from_millis(500)).await;
    } else {
        info!(
            "op_id:{}: Leverage {:.2}x already set for {}.",
            operation_identifier, current_leverage, futures_symbol
        );
    }
    Ok(())
}
