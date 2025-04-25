use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
use tracing::{error, info, warn};

use super::common::{manage_order_loop, OrderLoopParams}; // Используем общую функцию
use super::{
    HedgeProgressCallback, HedgeProgressUpdate, HedgeStage, Hedger, ORDER_FILL_TOLERANCE,
};
use crate::exchange::types::OrderSide;
use crate::exchange::Exchange;
use crate::storage::{mark_hedge_as_unhedged, Db, HedgeOperation};

pub(super) async fn run_unhedge_impl<E>(
    hedger: &Hedger<E>,
    original_op: HedgeOperation,
    db: &Db,
    mut progress_callback: HedgeProgressCallback,
) -> Result<(f64, f64)>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let symbol = original_op.base_symbol.clone(); // Базовый символ (e.g., "BTC")
    let original_hedge_op_id = original_op.id;
    let futures_buy_qty = original_op.target_futures_qty; // Сколько фьюча откупать
    let target_spot_sell_qty = original_op.spot_filled_qty; // Сколько спота продавать (цель)
    let futures_symbol = format!("{}{}", symbol, hedger.quote_currency); // Символ фьючерса

    info!(
        "Starting unhedge for original op_id={} ({}), target spot sell qty={:.8}, target futures qty to buy back={:.8}",
        original_hedge_op_id, symbol, target_spot_sell_qty, futures_buy_qty
    );

    // Проверка целевого количества спота
    if target_spot_sell_qty <= ORDER_FILL_TOLERANCE {
        let msg = format!(
            "Target spot sell quantity ({:.8}) based on original operation is too low",
            target_spot_sell_qty
        );
        error!("op_id={}: {}. Cannot unhedge.", original_hedge_op_id, msg);
        // Не меняем статус в БД, т.к. операция не началась
        return Err(anyhow!(msg));
    }
    // Проверка целевого количества фьючерса
    if futures_buy_qty <= ORDER_FILL_TOLERANCE {
         let msg = format!(
             "Target futures buy quantity ({:.8}) based on original operation is too low",
             futures_buy_qty
         );
         error!("op_id={}: {}. Cannot unhedge.", original_hedge_op_id, msg);
         return Err(anyhow!(msg));
     }


    // --- Проверка баланса и определение реального кол-ва спота для продажи ---
    let available_balance = match hedger.exchange.get_balance(&symbol).await {
        Ok(balance) => {
            info!(
                "op_id={}: Checked available balance {} for {}",
                original_hedge_op_id, balance.free, symbol
            );
            balance.free
        }
        Err(e) => {
            let msg = format!("Failed to get balance before unhedge: {}", e);
            error!(
                "op_id={}: Failed to get balance for {} before unhedge: {}. Aborting.",
                original_hedge_op_id, symbol, e
            );
            return Err(anyhow!(msg));
        }
    };

    // Реальное количество для продажи = минимум из цели и доступного
    let actual_spot_sell_qty = target_spot_sell_qty.min(available_balance);

    if actual_spot_sell_qty < target_spot_sell_qty - ORDER_FILL_TOLERANCE {
        warn!(
            "op_id={}: Available balance {:.8} is less than target sell quantity {:.8}. Selling available amount.",
            original_hedge_op_id, available_balance, target_spot_sell_qty
        );
    }

    // --- Получаем Min Order Qty для спота и проверяем РЕАЛЬНОЕ количество ---
    let spot_info = hedger
        .exchange
        .get_spot_instrument_info(&symbol)
        .await
        .map_err(|e| {
            anyhow!(
                "op_id={}: Failed to get SPOT instrument info for {}: {}",
                original_hedge_op_id,
                symbol,
                e
            )
        })?;
    let min_spot_qty_decimal = Decimal::from_str(&spot_info.lot_size_filter.min_order_qty)
        .map_err(|e| {
            anyhow!(
                "op_id={}: Failed to parse min_order_qty '{}': {}",
                original_hedge_op_id,
                &spot_info.lot_size_filter.min_order_qty,
                e
            )
        })?;
    info!(
        "op_id={}: Minimum spot order quantity for {}: {}",
        original_hedge_op_id, symbol, min_spot_qty_decimal
    );

    let actual_spot_sell_qty_decimal =
        Decimal::from_f64(actual_spot_sell_qty).unwrap_or_else(|| Decimal::ZERO);

    if actual_spot_sell_qty <= ORDER_FILL_TOLERANCE
        || actual_spot_sell_qty_decimal < min_spot_qty_decimal
    {
        let msg = format!(
            "Actual quantity to sell ({:.8}) is below minimum order size ({}) or zero",
            actual_spot_sell_qty, min_spot_qty_decimal
        );
        error!(
            "op_id={}: {}. Cannot unhedge.",
            original_hedge_op_id, msg
        );
        return Err(anyhow!(msg));
    }
    // --- КОНЕЦ ПРОВЕРКИ РЕАЛЬНОГО КОЛ-ВА ---

    info!(
        "op_id={}: Proceeding unhedge with actual spot sell quantity target: {:.8}",
        original_hedge_op_id, actual_spot_sell_qty
    );


    // --- Этап 1: Спот (Продажа) ---
    info!("op_id:{}: Starting SPOT sell stage...", original_hedge_op_id);
    let spot_filled_storage = Arc::new(TokioMutex::new(0.0)); // Свой счетчик для спота

    // Получаем начальную цену спота
    let current_spot_price = match hedger.exchange.get_spot_price(&symbol).await {
        Ok(p) if p > 0.0 => p,
        Ok(p) => {
            let msg = format!("Invalid initial spot price received: {}", p);
            error!("op_id={}: {}", original_hedge_op_id, msg);
            return Err(anyhow!(msg));
        }
        Err(e) => {
            let msg = format!("Failed to get initial spot price: {}", e);
            error!("op_id={}: {}. Aborting.", original_hedge_op_id, msg);
            return Err(anyhow!(msg));
        }
    };
    let spot_initial_limit_price =
        super::common::calculate_limit_price(current_spot_price, OrderSide::Sell, hedger.slippage);

    let spot_loop_params = OrderLoopParams {
        hedger,
        db, // Db не используется в цикле спота для unhedge, но тип требует
        operation_id: original_hedge_op_id,
        symbol: &symbol, // Базовый символ
        side: OrderSide::Sell,
        initial_target_qty: actual_spot_sell_qty, // Продаем реальное кол-во
        initial_limit_price: spot_initial_limit_price,
        progress_callback: &mut progress_callback,
        stage: HedgeStage::Spot,
        is_spot: true,
        min_order_qty_decimal: Some(min_spot_qty_decimal), // Передаем для проверки на пыль
        current_order_id_storage: None, // Не нужно внешнее хранилище ID
        total_filled_qty_storage: spot_filled_storage.clone(), // Свой счетчик
    };

    let final_spot_sold_qty = match manage_order_loop(spot_loop_params).await {
        Ok(filled_qty) => {
            info!(
                "op_id={}: Unhedge SPOT sell stage finished. Final actual spot sold quantity: {:.8}",
                original_hedge_op_id, filled_qty
            );
            if (filled_qty - actual_spot_sell_qty).abs() > ORDER_FILL_TOLERANCE * 10.0 {
                warn!(
                    "op_id={}: Final spot sold qty {:.8} significantly differs from target {:.8}.",
                    original_hedge_op_id, filled_qty, actual_spot_sell_qty
                );
            }
            filled_qty
        }
        Err(loop_err) => {
            error!(
                "op_id={}: Unhedge SPOT sell stage failed: {}",
                original_hedge_op_id, loop_err
            );
            // Статус в БД не меняем, т.к. операция не завершена и не факт, что что-то продалось
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
        new_limit_price: spot_initial_limit_price, // Не так важно
        is_replacement: false,
        filled_qty: final_spot_sold_qty,
        target_qty: final_spot_sold_qty,
        cumulative_filled_qty: final_spot_sold_qty,
        total_target_qty: actual_spot_sell_qty,
    };
    if let Err(e) = progress_callback(spot_done_update).await {
        if !e.to_string().contains("message is not modified") {
            warn!(
                "op_id:{}: Unhedge progress callback failed after spot sell: {}",
                original_hedge_op_id, e
            );
        }
    }
    // --- Конец колбэка спота ---


    // --- Этап 2: Фьючерс (Покупка) ---
    info!("op_id:{}: Starting FUTURES buy stage...", original_hedge_op_id);
    let futures_filled_storage = Arc::new(TokioMutex::new(0.0)); // Свой счетчик

    // Получаем актуальную цену фьючерса для начального ордера
    let futures_market_price = match hedger.exchange.get_futures_ticker(&futures_symbol).await {
        Ok(ticker) => {
            // Для покупки используем Ask
            info!(
                "op_id:{}: Futures ticker received: bid={:.2}, ask={:.2}.",
                original_hedge_op_id, ticker.bid_price, ticker.ask_price
            );
            ticker.ask_price
        }
        Err(e) => {
            warn!(
                "op_id:{}: Failed to get futures ticker: {}. Using last spot price as fallback.",
                original_hedge_op_id, e
            );
            spot_price_for_cb // Fallback на последнюю цену спота
        }
    };
    let futures_initial_limit_price =
        super::common::calculate_limit_price(futures_market_price, OrderSide::Buy, hedger.slippage);

    let futures_loop_params = OrderLoopParams {
        hedger,
        db, // Не используется
        operation_id: original_hedge_op_id,
        symbol: &futures_symbol, // Символ фьючерса
        side: OrderSide::Buy,
        initial_target_qty: futures_buy_qty, // Откупаем исходное кол-во фьюча
        initial_limit_price: futures_initial_limit_price,
        progress_callback: &mut progress_callback,
        stage: HedgeStage::Futures,
        is_spot: false,
        min_order_qty_decimal: None, // Не нужно
        current_order_id_storage: None, // Не нужно
        total_filled_qty_storage: futures_filled_storage.clone(), // Свой счетчик
    };

    let final_fut_bought_qty = match manage_order_loop(futures_loop_params).await {
        Ok(filled_qty) => {
            info!(
                "op_id:{}: Unhedge FUTURES buy stage finished. Final actual futures bought quantity: {:.8}",
                original_hedge_op_id, filled_qty
            );
             if (filled_qty - futures_buy_qty).abs() > ORDER_FILL_TOLERANCE * 10.0 {
                warn!(
                    "op_id:{}: Final futures bought qty {:.8} significantly differs from target {:.8}.",
                    original_hedge_op_id, filled_qty, futures_buy_qty
                );
            }
            filled_qty
        }
        Err(loop_err) => {
            error!(
                "op_id:{}: Unhedge FUTURES buy stage failed: {}",
                original_hedge_op_id, loop_err
            );
            // Спот уже продан! Это частичный успех/неудача.
            // Статус в БД НЕ МЕНЯЕМ на unhedged.
            // Возвращаем ошибку, вызывающий код должен обработать ситуацию.
            warn!("op_id:{}: Spot was sold, but futures buy failed! Manual intervention may be required.", original_hedge_op_id);
            return Err(loop_err);
        }
    };

    // --- Успешное завершение ---
    // Помечаем исходную операцию как расхеджированную
    if let Err(e) = mark_hedge_as_unhedged(db, original_hedge_op_id).await {
        error!(
            "op_id:{}: Failed mark original hedge {} as unhedged in DB: {}",
            original_hedge_op_id, original_hedge_op_id, e
        );
        // Не фатально для самой операции, но плохо для учета
    }
    info!(
        "op_id:{}: Unhedge completed successfully for original op_id={}. Spot Sold: {:.8}, Fut Bought: {:.8}",
        original_hedge_op_id, original_hedge_op_id, final_spot_sold_qty, final_fut_bought_qty
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
        filled_qty: final_fut_bought_qty,
        target_qty: final_fut_bought_qty,
        cumulative_filled_qty: final_fut_bought_qty,
        total_target_qty: futures_buy_qty,
    };
    let _ = progress_callback(fut_done_update).await; // Игнорируем ошибку
    // --- Конец колбэка фьючерса ---

    Ok((final_spot_sold_qty, final_fut_bought_qty))
}
