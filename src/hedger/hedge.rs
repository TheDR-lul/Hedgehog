use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::sleep;
use tracing::{error, info, warn};

use super::common::{manage_order_loop, OrderLoopParams}; // Используем общую функцию
use super::{
    HedgeParams, HedgeProgressCallback, HedgeProgressUpdate, HedgeStage, Hedger,
    ORDER_FILL_TOLERANCE,
};
use crate::exchange::types::OrderSide;
use crate::exchange::Exchange;
use crate::storage::{update_hedge_final_status, Db};

pub(super) async fn run_hedge_impl<E>(
    hedger: &Hedger<E>,
    params: HedgeParams,
    mut progress_callback: HedgeProgressCallback,
    current_order_id_storage: Arc<TokioMutex<Option<String>>>,
    total_filled_qty_storage: Arc<TokioMutex<f64>>, // Используется только для спота
    operation_id: i64,
    db: &Db,
) -> Result<(f64, f64, f64)>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let HedgeParams {
        spot_order_qty: initial_spot_qty,
        fut_order_qty: initial_fut_qty,
        current_spot_price, // Используется для расчета плеча и как fallback
        initial_limit_price, // Начальная цена для спота
        symbol, // Базовый символ (e.g., "BTC")
        spot_value: _spot_value, // Расчетное значение, не используется напрямую в цикле
        available_collateral: _available_collateral, // Расчетное значение, не используется напрямую в цикле
        min_spot_qty_decimal: _min_spot_qty_decimal, // Не нужно для hedge
        min_fut_qty_decimal: _min_fut_qty_decimal, // Не нужно для hedge
        spot_decimals: _spot_decimals, // Не используется напрямую
        fut_decimals: _fut_decimals, // Не используется напрямую
        futures_symbol, // Символ фьючерса (e.g., "BTCUSDT")
    } = params;

    info!(
        "Running hedge op_id:{} for {} with spot_qty(gross_target)={:.8}, fut_qty(net_target)={:.8}",
        operation_id, symbol, initial_spot_qty, initial_fut_qty
    );

    // --- Проверка и установка плеча ---
    // Используем расчетные значения из params
    let futures_position_value = initial_fut_qty * current_spot_price; // Оценка
    if _available_collateral <= 0.0 {
        let msg = "Available collateral is non-positive".to_string();
        error!("op_id:{}: {}. Aborting.", operation_id, msg);
        let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
        return Err(anyhow!(msg));
    }
    let required_leverage = futures_position_value / _available_collateral;
    info!(
        "op_id:{}: Confirming required leverage: {:.2}x",
        operation_id, required_leverage
    );
    if required_leverage.is_nan() || required_leverage.is_infinite() {
        let msg = "Invalid required leverage calculation".to_string();
        error!("op_id:{}: {}. Aborting.", operation_id, msg);
        let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
        return Err(anyhow!(msg));
    }

    // Получаем и устанавливаем плечо
    match set_leverage_if_needed(hedger, &symbol, required_leverage, operation_id, db).await {
        Ok(_) => info!("op_id:{}: Leverage check/set successful.", operation_id),
        Err(e) => {
            // Ошибка уже залогирована и статус в БД обновлен внутри set_leverage_if_needed
            return Err(e);
        }
    }
    // --- Конец проверки плеча ---

    // --- Этап 1: Спот ---
    info!("op_id:{}: Starting SPOT buy stage...", operation_id);
    // Сбрасываем счетчик исполненного для этого этапа
    *total_filled_qty_storage.lock().await = 0.0;

    let spot_loop_params = OrderLoopParams {
        hedger,
        db,
        operation_id,
        symbol: &symbol, // Используем базовый символ для спота
        side: OrderSide::Buy,
        initial_target_qty: initial_spot_qty,
        initial_limit_price, // Начальная цена для спота
        progress_callback: &mut progress_callback,
        stage: HedgeStage::Spot,
        is_spot: true,
        min_order_qty_decimal: None, // Не нужно для hedge
        current_order_id_storage: Some(Arc::clone(&current_order_id_storage)), // Передаем хранилище ID
        total_filled_qty_storage: Arc::clone(&total_filled_qty_storage), // Передаем хранилище кол-ва
    };

    let final_spot_qty_gross = match manage_order_loop(spot_loop_params).await {
        Ok(filled_qty) => {
            info!(
                "op_id:{}: Hedge SPOT buy stage finished. Final actual spot gross quantity: {:.8}",
                operation_id, filled_qty
            );
            // Проверка на расхождение с целью (уже делается в цикле, но можно добавить финальную)
            if (filled_qty - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE * 10.0 { // Увеличим допуск для финального варнинга
                warn!(
                    "op_id:{}: Final SPOT gross filled {:.8} significantly differs from target {:.8}. Using actual filled.",
                    operation_id, filled_qty, initial_spot_qty
                );
            }
            filled_qty
        }
        Err(loop_err) => {
            error!(
                "op_id:{}: Hedge SPOT buy stage failed: {}",
                operation_id, loop_err
            );
            let current_filled = *total_filled_qty_storage.lock().await;
            let _ = update_hedge_final_status(
                db,
                operation_id,
                "Failed",
                None, // Спот ордер ID уже обновлялся в цикле
                current_filled,
                Some(&format!("Spot stage failed: {}", loop_err)),
            )
            .await;
            return Err(loop_err);
        }
    };

    // --- Отправляем финальный колбэк для спота (100%) ---
    let spot_price_for_cb = match hedger.exchange.get_spot_price(&symbol).await {
         Ok(p) => p, Err(_) => current_spot_price // Fallback
    };
    let spot_done_update = HedgeProgressUpdate {
        stage: HedgeStage::Spot,
        current_spot_price: spot_price_for_cb,
        new_limit_price: initial_limit_price, // Последняя цена может быть другой, но это не так важно
        is_replacement: false,
        filled_qty: final_spot_qty_gross, // Исполнено 100%
        target_qty: final_spot_qty_gross, // Цель = Исполнено
        cumulative_filled_qty: final_spot_qty_gross,
        total_target_qty: initial_spot_qty,
    };
    if let Err(e) = progress_callback(spot_done_update).await {
        if !e.to_string().contains("message is not modified") {
            warn!(
                "op_id:{}: Progress callback failed after spot fill: {}",
                operation_id, e
            );
        }
    }
    // --- Конец колбэка спота ---

    // Оценка финальной стоимости спота (может быть неточной из-за изменения цены)
    // TODO: Внедрить динамический расчет фьюча на основе реальной стоимости спота
    let final_spot_value_gross_estimate = final_spot_qty_gross * spot_price_for_cb;


    // --- Этап 2: Фьючерс ---
    info!("op_id:{}: Starting FUTURES sell stage...", operation_id);
    // Сбрасываем счетчик исполненного для этого этапа
    let futures_filled_storage = Arc::new(TokioMutex::new(0.0));

    // Получаем актуальную цену фьючерса для начального ордера
    let futures_market_price = match hedger.exchange.get_futures_ticker(&futures_symbol).await {
        Ok(ticker) => {
            // Для продажи используем Bid
            info!(
                "op_id:{}: Futures ticker received: bid={:.2}, ask={:.2}.",
                operation_id, ticker.bid_price, ticker.ask_price
            );
            ticker.bid_price
        }
        Err(e) => {
            warn!(
                "op_id:{}: Failed to get futures ticker: {}. Using last spot price as fallback.",
                operation_id, e
            );
            spot_price_for_cb // Fallback на последнюю цену спота
        }
    };
    let futures_initial_limit_price =
        super::common::calculate_limit_price(futures_market_price, OrderSide::Sell, hedger.slippage);


    let futures_loop_params = OrderLoopParams {
        hedger,
        db, // Db не используется в цикле фьючерса для hedge, но тип требует
        operation_id,
        symbol: &futures_symbol, // Используем символ фьючерса
        side: OrderSide::Sell,
        initial_target_qty: initial_fut_qty, // Цель - расчетное кол-во фьюча
        initial_limit_price: futures_initial_limit_price,
        progress_callback: &mut progress_callback,
        stage: HedgeStage::Futures,
        is_spot: false,
        min_order_qty_decimal: None, // Не нужно
        current_order_id_storage: None, // Не отслеживаем ID фьюча во внешнем хранилище
        total_filled_qty_storage: futures_filled_storage.clone(), // Используем свой счетчик
    };

    let final_fut_qty = match manage_order_loop(futures_loop_params).await {
        Ok(filled_qty) => {
            info!(
                "op_id:{}: Hedge FUTURES sell stage finished. Final actual futures net quantity: {:.8}",
                operation_id, filled_qty
            );
             if (filled_qty - initial_fut_qty).abs() > ORDER_FILL_TOLERANCE * 10.0 {
                warn!(
                    "op_id:{}: Final FUTURES net filled {:.8} significantly differs from target {:.8}. Using actual filled.",
                    operation_id, filled_qty, initial_fut_qty
                );
            }
            filled_qty
        }
        Err(loop_err) => {
            error!(
                "op_id:{}: Hedge FUTURES sell stage failed: {}",
                operation_id, loop_err
            );
            // Спот уже куплен! Обновляем статус с ошибкой фьючерса
            let last_fut_filled = *futures_filled_storage.lock().await;
            let _ = update_hedge_final_status(
                db,
                operation_id,
                "Failed", // Статус Failed, но спот исполнен
                None, // ID фьюча не хранился глобально
                last_fut_filled, // Записываем сколько успело исполниться фьюча
                Some(&format!("Futures stage failed: {}", loop_err)),
            )
            .await;
             // Важно: Возвращаем ошибку, но вызывающий код должен знать, что спот куплен!
             // Возможно, стоит изменить возвращаемый тип или использовать кастомный тип ошибки.
            return Err(loop_err);
        }
    };

    // --- Успешное завершение ---
    // Обновляем финальный статус в БД
    // ID фьючерса не отслеживался глобально, передаем None
    let _ = update_hedge_final_status(
        db,
        operation_id,
        "Completed",
        None, // TODO: Получать ID последнего фьюч ордера из manage_order_loop?
        final_fut_qty,
        None, // Нет ошибки
    )
    .await;
    info!(
        "op_id:{}: Hedge completed successfully. Spot Gross: {:.8}, Fut Net: {:.8}",
        operation_id, final_spot_qty_gross, final_fut_qty
    );

    // --- Отправляем финальный колбэк для фьючерса (100%) ---
     let fut_price_for_cb = match hedger.exchange.get_futures_ticker(&futures_symbol).await {
         Ok(t) => (t.bid_price + t.ask_price) / 2.0, Err(_) => futures_market_price
     };
    let fut_done_update = HedgeProgressUpdate {
        stage: HedgeStage::Futures,
        current_spot_price: fut_price_for_cb, // Цена фьючерса
        new_limit_price: futures_initial_limit_price, // Не так важно
        is_replacement: false,
        filled_qty: final_fut_qty,
        target_qty: final_fut_qty,
        cumulative_filled_qty: final_fut_qty,
        total_target_qty: initial_fut_qty,
    };
    let _ = progress_callback(fut_done_update).await; // Игнорируем ошибку
    // --- Конец колбэка фьючерса ---

    Ok((final_spot_qty_gross, final_fut_qty, final_spot_value_gross_estimate))
}


// Вспомогательная функция для установки плеча
async fn set_leverage_if_needed<E>(
    hedger: &Hedger<E>,
    symbol: &str, // Базовый символ
    required_leverage: f64,
    operation_id: i64,
    db: &Db,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let current_leverage = match hedger.exchange.get_current_leverage(symbol).await {
        Ok(l) => l,
        Err(e) => {
            let msg = format!("Leverage check failed: {}", e);
            error!("op_id:{}: Failed to get current leverage: {}. Aborting.", operation_id, e);
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
            return Err(anyhow!(msg));
        }
    };

    // Округляем целевое плечо до 2 знаков, как часто требует API
    let target_leverage_set = (required_leverage * 100.0).round() / 100.0;

    // Сравниваем с небольшим допуском
    if (target_leverage_set - current_leverage).abs() > 0.01 {
        info!(
            "op_id:{}: Setting leverage {} -> {}",
            operation_id, current_leverage, target_leverage_set
        );
        if let Err(e) = hedger.exchange.set_leverage(symbol, target_leverage_set).await {
            let msg = format!("Failed to set leverage: {}", e);
            error!(
                "op_id:{}: Failed to set leverage to {:.2}x: {}. Aborting.",
                operation_id, target_leverage_set, e
            );
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
            return Err(anyhow!(msg));
        }
        info!("op_id:{}: Leverage set successfully.", operation_id);
        sleep(Duration::from_millis(500)).await; // Небольшая пауза после установки
    } else {
        info!(
            "op_id:{}: Leverage {:.2}x already set.",
            operation_id, current_leverage
        );
    }
    Ok(())
}

