// src/hedger/hedge.rs
// ВЕРСИЯ С ИСПРАВЛЕНИЯМИ ОШИБОК КОМПИЛЯЦИИ (v6 - основана на v5 и ошибках)

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::sleep;
use tracing::{error, info, warn};

use super::common::{calculate_limit_price, manage_order_loop, OrderLoopParams};
use super::{
    HedgeParams, HedgeProgressCallback, HedgeProgressUpdate, HedgeStage, Hedger,
    ORDER_FILL_TOLERANCE,
};
use crate::exchange::types::{DetailedOrderStatus, OrderSide};
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
    Ok(decimal_value) // <-- ИСПРАВЛЕНО: Просто возвращаем Ok(Decimal)
}

// Реализация основной логики хеджирования
pub(super) async fn run_hedge_impl<ExchangeType>(
    hedger: &Hedger<ExchangeType>,
    params: HedgeParams,
    mut progress_callback: HedgeProgressCallback,
    current_spot_order_identifier_storage: Arc<TokioMutex<Option<String>>>,
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
        current_order_id_storage: Some(Arc::clone(&current_spot_order_identifier_storage)), // Передаем хранилище ID
        total_filled_qty_storage: Arc::clone(&total_filled_spot_quantity_storage), // Передаем хранилище общего кол-ва
    };

    // Внешняя переменная для ID последнего спот ордера
    let mut last_spot_order_identifier: Option<String> = None;

    // manage_order_loop возвращает Result<f64> (итоговое исполненное количество)
    // ID ордера сохраняется внутри цикла в current_spot_order_identifier_storage
    let final_spot_quantity_gross = match manage_order_loop(spot_loop_params).await {
        Ok(filled_quantity) => {
            // Получаем ID ордера из хранилища, которое было передано в manage_order_loop
            let order_id_option = current_spot_order_identifier_storage.lock().await.clone();

            match order_id_option {
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
                     // Обновляем БД с ID последнего ордера
                     if let Err(e) = update_hedge_spot_order(
                         database,
                         operation_identifier,
                         Some(&id), // Используем ID из хранилища
                         filled_quantity,
                     )
                     .await
                     {
                         error!(
                             "op_id:{}: Failed to update FINAL spot order info in DB: {}",
                             operation_identifier, e
                         );
                         // Не фатально, продолжаем, но логируем
                     }
                     // Сохраняем ID для использования вне match
                     last_spot_order_identifier = Some(id);
                    filled_quantity // Возвращаем количество
                }
                _ => {
                    // Это критическая ситуация: цикл завершился успешно, но ID ордера не записался.
                    let error_message = "Spot order loop succeeded but failed to retrieve order ID from storage.".to_string();
                    error!("op_id:{}: {}", operation_identifier, error_message);
                    // Обновляем статус в БД перед возвратом ошибки
                    let _ = update_hedge_final_status(
                        database,
                        operation_identifier,
                        "Failed",
                        None, // ID неизвестен
                        filled_quantity, // Используем полученное количество
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
            // Получаем последнее известное состояние перед ошибкой
            let current_filled_quantity = *total_filled_spot_quantity_storage.lock().await;
            let last_known_order_id = current_spot_order_identifier_storage.lock().await.clone();
            let _ = update_hedge_final_status(
                database,
                operation_identifier,
                "Failed",
                last_known_order_id.as_deref(), // Используем последний известный ID, если он есть
                current_filled_quantity,
                Some(&format!("Spot stage failed: {}", loop_error)),
            )
            .await;
            return Err(loop_error);
        }
    };

    // Проверка, что ID ордера был получен и сохранен
    let final_spot_order_id = match last_spot_order_identifier {
        Some(id) => id,
        None => {
             // Эта ветка не должна достигаться из-за проверки внутри Ok() выше, но для полноты
             error!("op_id:{}: Critical error - last spot order identifier is None after spot stage completion.", operation_identifier);
             let error_message = "Failed to retrieve last spot order ID internally".to_string();
              let _ = update_hedge_final_status(database, operation_identifier, "Failed", None, final_spot_quantity_gross, Some(&error_message)).await;
             return Err(anyhow!(error_message));
        }
    };

    // --- Получение точной стоимости исполненного спота ---
    info!("op_id:{}: Fetching execution details for spot order {}...", operation_identifier, final_spot_order_id);
    let detailed_spot_status: DetailedOrderStatus = match hedger.exchange.get_spot_order_execution_details(&symbol, &final_spot_order_id).await {
         Ok(details) => details,
         Err(error) => {
             error!("op_id:{}: Failed get spot execution details for order {}: {}. Aborting futures stage.", operation_identifier, final_spot_order_id, error);
             let error_message = format!("Failed get spot exec details: {}", error);
             let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
             return Err(anyhow!(error_message));
         }
    };

    // Проверяем, что полученное количество совпадает (в пределах допуска)
    // Используем поле filled_qty вместо несуществующего cumulative_filled_quantity
    if (detailed_spot_status.filled_qty - final_spot_quantity_gross).abs() > ORDER_FILL_TOLERANCE { // <-- ИСПРАВЛЕНО
        warn!(
            "op_id:{}: Discrepancy between loop final quantity {:.8} and execution details quantity {:.8} for order {}. Using execution details.",
            operation_identifier, final_spot_quantity_gross, detailed_spot_status.filled_qty, final_spot_order_id // <-- ИСПРАВЛЕНО
        );
        // Можно переприсвоить final_spot_quantity_gross, если это важно, но для расчета фьючерса используем cumulative_executed_value
    }

    let actual_spot_value = detailed_spot_status.cumulative_executed_value;
     if actual_spot_value <= ORDER_FILL_TOLERANCE / 10.0 { // Более строгая проверка для стоимости
         let error_message = format!("Cumulative executed value for spot order {} is effectively zero ({:.8}). Cannot calculate dynamic futures quantity.", final_spot_order_id, actual_spot_value);
         error!("op_id:{}: {}", operation_identifier, error_message);
         let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
         return Err(anyhow!(error_message));
     }
    info!("op_id:{}: Actual executed spot value: {:.8}", operation_identifier, actual_spot_value);

    // --- Отправляем финальный колбэк для спота (100%) ---
    let spot_price_for_callback = if detailed_spot_status.average_price > 0.0 {
        detailed_spot_status.average_price
    } else {
        warn!("op_id:{}: Average price for spot order {} was zero, using fallback price for progress callback.", operation_identifier, final_spot_order_id);
        // Пытаемся получить текущую цену, если не удается - используем цену из параметров
        match hedger.exchange.get_spot_price(&symbol).await {
             Ok(price) => price,
             Err(_) => current_spot_price
        }
    };

    let spot_done_update = HedgeProgressUpdate {
        stage: HedgeStage::Spot,
        current_spot_price: spot_price_for_callback,
        new_limit_price: initial_spot_limit_price, // Цена последнего ордера может быть другой, но для UI показываем начальную
        is_replacement: false, // Это финальный апдейт
        filled_qty: final_spot_quantity_gross, // Общее исполненное кол-во
        target_qty: final_spot_quantity_gross, // Цель достигнута
        cumulative_filled_qty: final_spot_quantity_gross,
        total_target_qty: initial_spot_quantity, // Начальная цель для сравнения
    };
    if let Err(error) = progress_callback(spot_done_update).await {
        // Игнорируем ошибку "message is not modified"
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
    // Используем среднюю цену между бидом и аском для расчета
    let futures_price_now = (futures_ticker.bid_price + futures_ticker.ask_price) / 2.0;
    if futures_price_now <= 0.0 {
         let error_message = format!("Invalid futures price for calculation: {:.2}", futures_price_now);
         error!("op_id:{}: {}", operation_identifier, error_message);
         let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
         return Err(anyhow!(error_message));
    }

    let dynamic_futures_quantity = actual_spot_value / futures_price_now;
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

    // Сравниваем с минимальным размером ордера для фьючерсов
    if rounded_dynamic_futures_quantity_decimal < min_futures_quantity_decimal {
         let error_message = format!(
             "Calculated dynamic futures quantity {:.8} is less than minimum order size {}",
            rounded_dynamic_futures_quantity_decimal, min_futures_quantity_decimal
         );
         error!("op_id:{}: {}", operation_identifier, error_message);
         let _ = update_hedge_final_status(database, operation_identifier, "Failed", Some(&final_spot_order_id), final_spot_quantity_gross, Some(&error_message)).await;
         return Err(anyhow!(error_message));
    }

    // Конвертируем обратно в f64 для использования в цикле ордеров
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
    let futures_filled_storage = Arc::new(TokioMutex::new(0.0)); // Отдельное хранилище для фьючерсов
    // Рассчитываем лимитную цену для первого фьючерсного ордера
    let futures_initial_limit_price =
        calculate_limit_price(futures_price_now, OrderSide::Sell, hedger.slippage);

    let futures_loop_params = OrderLoopParams {
        hedger,
        db: database,
        operation_id: operation_identifier,
        symbol: &futures_symbol, // Фьючерсный символ
        side: OrderSide::Sell,
        initial_target_qty: final_futures_target_quantity, // <-- Используем динамический объем
        initial_limit_price: futures_initial_limit_price,
        progress_callback: &mut progress_callback,
        stage: HedgeStage::Futures,
        is_spot: false,
        min_order_qty_decimal: Some(min_futures_quantity_decimal), // Передаем минимальный размер для проверки внутри цикла
        current_order_id_storage: None, // Не передаем хранилище ID для фьючерса (можно добавить при необходимости)
        total_filled_qty_storage: futures_filled_storage.clone(), // Передаем хранилище общего кол-ва фьючерса
    };

    // manage_order_loop возвращает Result<f64>
    let final_futures_quantity = match manage_order_loop(futures_loop_params).await {
        Ok(filled_quantity) => {
            // ID ордера здесь не получаем, т.к. current_order_id_storage = None
            info!(
                "op_id:{}: Hedge FUTURES sell stage finished. Final actual futures net quantity: {:.8}", // Убрали упоминание ID
                operation_identifier, filled_quantity
            );
             if (filled_quantity - final_futures_target_quantity).abs() > ORDER_FILL_TOLERANCE * 10.0 {
                warn!(
                    "op_id:{}: Final FUTURES net filled {:.8} significantly differs from dynamic target {:.8}. Using actual filled.",
                    operation_identifier, filled_quantity, final_futures_target_quantity
                );
            }
            // Здесь можно было бы получить ID последнего фьючерсного ордера, если бы передали хранилище
            // и обновить БД аналогично споту.
            filled_quantity // Возвращаем количество
        }
        Err(loop_error) => {
            error!(
                "op_id:{}: Hedge FUTURES sell stage failed: {}",
                operation_identifier, loop_error
            );
            let last_futures_filled_quantity = *futures_filled_storage.lock().await; // Получаем исполненное кол-во перед ошибкой
            let _ = update_hedge_final_status(
                database,
                operation_identifier,
                "Failed",
                Some(&final_spot_order_id), // ID спот ордера известен
                final_spot_quantity_gross, // Исполненное кол-во спота
                Some(&format!("Futures stage failed: {}", loop_error)),
                // Можно добавить поля для сохранения состояния фьючерса при ошибке
                // None, // futures_order_id_on_fail (неизвестен)
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
    // Обновляем финальный статус как "Completed"
    let _ = update_hedge_final_status(
        database,
        operation_identifier,
        "Completed",
        Some(&final_spot_order_id), // Финальный ID спот ордера
        final_spot_quantity_gross, // Финальное кол-во спота
        None, // Нет ошибки
        // TODO: Добавить поля в БД и функцию для сохранения финального ID и кол-ва фьючерса?
        // None, // final_futures_order_id (неизвестен)
        // Some(final_futures_quantity), // final_futures_filled_qty
    )
    .await;

    // --- Отправляем финальный колбэк для фьючерса (100%) ---
     // Получаем последнюю цену фьючерса для колбэка
     let futures_price_for_callback = match hedger.exchange.get_futures_ticker(&futures_symbol).await {
         Ok(ticker) => (ticker.bid_price + ticker.ask_price) / 2.0,
         Err(_) => futures_price_now // Используем цену, по которой считали объем, если не удалось получить новую
     };
    let futures_done_update = HedgeProgressUpdate {
        stage: HedgeStage::Futures,
        current_spot_price: futures_price_for_callback, // Цена фьючерса для этого этапа
        new_limit_price: futures_initial_limit_price, // Начальная лимитная цена фьючерса
        is_replacement: false, // Финальный апдейт
        filled_qty: final_futures_quantity, // Общее исполненное кол-во фьючерса
        target_qty: final_futures_quantity, // Цель достигнута
        cumulative_filled_qty: final_futures_quantity,
        total_target_qty: final_futures_target_quantity, // Динамическая цель фьючерса
    };
    if let Err(error) = progress_callback(futures_done_update).await {
         if !error.to_string().contains("message is not modified") {
            warn!("op_id:{}: Futures progress callback failed: {}", operation_identifier, error);
         }
    }
    // --- Конец колбэка фьючерса ---

    // Возвращаем финальные количества и стоимость спота
    Ok((final_spot_quantity_gross, final_futures_quantity, actual_spot_value))
}

// Вспомогательная функция для установки плеча
async fn set_leverage_if_needed<ExchangeType>(
    hedger: &Hedger<ExchangeType>,
    futures_symbol: &str, // Используем фьючерсный символ
    required_leverage: f64,
    operation_identifier: i64,
    database: &Db,
) -> Result<()>
where
    ExchangeType: Exchange + Clone + Send + Sync + 'static,
{
    // Получаем текущее плечо для фьючерсного символа
    let current_leverage = match hedger.exchange.get_current_leverage(futures_symbol).await {
        Ok(leverage_value) => leverage_value,
        Err(error) => {
            let error_message = format!("Leverage check failed for {}: {}", futures_symbol, error);
            error!("op_id:{}: {}. Aborting.", operation_identifier, error_message);
            // Обновляем статус перед возвратом ошибки
            let _ = update_hedge_final_status(database, operation_identifier, "Failed", None, 0.0, Some(&error_message)).await;
            return Err(anyhow!(error_message));
        }
    };

    // Округляем требуемое плечо до сотых (или до точности, поддерживаемой биржей)
    // Убедимся, что округление не приводит к нулю или отрицательному значению
    let target_leverage_to_set = (required_leverage.max(0.01) * 100.0).round() / 100.0; // Минимум 0.01x

    // Сравниваем с небольшим допуском
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
            // Обновляем статус перед возвратом ошибки
            let _ = update_hedge_final_status(database, operation_identifier, "Failed", None, 0.0, Some(&error_message)).await;
            return Err(anyhow!(error_message));
        }
        info!("op_id:{}: Leverage set successfully for {}.", operation_identifier, futures_symbol);
        // Небольшая пауза после установки плеча, чтобы биржа успела обработать
        sleep(Duration::from_millis(500)).await;
    } else {
        info!(
            "op_id:{}: Leverage {:.2}x already set for {}.",
            operation_identifier, current_leverage, futures_symbol
        );
    }
    Ok(())
}
