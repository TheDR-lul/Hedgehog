// src/webservice_hedge/unhedge_logic/ws_handlers.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{debug, error, info, warn, trace};

use crate::exchange::types::{WebSocketMessage, DetailedOrderStatus, OrderSide, OrderStatusText, OrderbookLevel};
use crate::webservice_hedge::unhedge_task::HedgerWsUnhedgeTask;
use crate::webservice_hedge::state::{HedgerWsStatus, Leg};
// Используем хелперы из ЭТОГО модуля (unhedge_logic)
use crate::webservice_hedge::unhedge_logic::helpers::{get_current_price, send_progress_update};
// Импортируем функции управления ордерами из СОБСТВЕННОГО модуля unhedge_logic
use crate::webservice_hedge::unhedge_logic::order_management::{handle_cancel_confirmation, check_stale_orders};


// Главный обработчик сообщений WS для Unhedge
pub async fn handle_websocket_message(task: &mut HedgerWsUnhedgeTask, message: WebSocketMessage) -> Result<()> {
     match message {
        WebSocketMessage::OrderUpdate(details) => {
            let status_clone = task.state.status.clone();
            if let HedgerWsStatus::WaitingCancelConfirmation { ref cancelled_order_id, cancelled_leg, .. } = status_clone {
                 if details.order_id == *cancelled_order_id {
                     info!(operation_id=task.operation_id, order_id=%details.order_id, "Получено обновление для отменяемого ордера (расхедж).");
                     handle_order_update(task, details.clone()).await?; // Локальная функция

                     let order_is_inactive = match cancelled_leg {
                         Leg::Spot => task.state.active_spot_order.is_none(),
                         Leg::Futures => task.state.active_futures_order.is_none(),
                     };

                     // Проверяем, что мы все еще в состоянии ожидания отмены и ордер действительно неактивен
                     if matches!(task.state.status, HedgerWsStatus::WaitingCancelConfirmation {.. }) && order_is_inactive {
                          // Вызываем адаптированную функцию из unhedge_logic::order_management
                          handle_cancel_confirmation(task, cancelled_order_id, cancelled_leg).await?;
                     } else {
                          debug!(operation_id=task.operation_id, order_id=%cancelled_order_id, status=?task.state.status, order_is_inactive, "Обновление ордера (расхедж) получено, но ордер все еще активен или статус изменился. Пока не вызываем handle_cancel_confirmation.");
                     }
                     return Ok(());
                 }
            }
            handle_order_update(task, details).await?; // Локальная функция
        }
        WebSocketMessage::OrderBookL2 { symbol, bids, asks, is_snapshot } => {
            handle_order_book_update(task, symbol, bids, asks, is_snapshot).await?; // Локальная функция
            // Вызываем адаптированную функцию из unhedge_logic::order_management
            check_stale_orders(task).await?;
        }
        WebSocketMessage::PublicTrade { symbol, price, qty, side, timestamp } => {
             handle_public_trade_update(task, symbol, price, qty, side, timestamp).await?; // Локальная функция
        }
        WebSocketMessage::Error(error_message) => {
            warn!(operation_id = task.operation_id, %error_message, "Получено сообщение об ошибке из WebSocket потока");
            // Здесь можно добавить логику перехода задачи в состояние Failed, если ошибка критическая
        }
        WebSocketMessage::Disconnected => {
             error!(operation_id = task.operation_id, "Получено событие разрыва WebSocket соединения.");
             // Ошибка будет передана в цикл run задачи unhedge_task.rs
             return Err(anyhow!("WebSocket соединение разорвано"));
        }
        WebSocketMessage::Pong => { debug!(operation_id = task.operation_id, "Pong получен"); }
        WebSocketMessage::Authenticated(success) => { info!(operation_id = task.operation_id, success, "Получен статус аутентификации WS"); }
        WebSocketMessage::SubscriptionResponse { success, ref topic } => { info!(operation_id = task.operation_id, success, %topic, "Получен ответ на подписку WS"); }
        WebSocketMessage::Connected => { info!(operation_id = task.operation_id, "Получено событие подключения WS (вероятно, избыточно)"); }
    }
    Ok(())
}

// Обработка обновления ордера (Unhedge) - эта функция остается без изменений, так как она уже специфична для HedgerWsUnhedgeTask
async fn handle_order_update(task: &mut HedgerWsUnhedgeTask, details: DetailedOrderStatus) -> Result<()> {
    info!(operation_id = task.operation_id, order_id = %details.order_id, status = ?details.status_text, filled_qty = details.filled_qty, "Обработка обновления ордера РАСХЕДЖИРОВАНИЯ");

    let mut leg_found = None;
    let mut quantity_diff = Decimal::ZERO;
    let mut avg_price = Decimal::ZERO;
    let mut last_filled_price_opt: Option<Decimal> = None;
    let mut final_status_reached = false;
    let mut order_status_text = OrderStatusText::Unknown("".to_string());

    let old_filled_qty = if task.state.active_spot_order.as_ref().map_or(false, |o| o.order_id == details.order_id) {
        leg_found = Some(Leg::Spot);
        task.state.active_spot_order.as_ref().map(|o| o.filled_quantity).unwrap_or_default()
    } else if task.state.active_futures_order.as_ref().map_or(false, |o| o.order_id == details.order_id) {
        leg_found = Some(Leg::Futures);
        task.state.active_futures_order.as_ref().map(|o| o.filled_quantity).unwrap_or_default()
    } else {
        warn!(operation_id = task.operation_id, order_id = %details.order_id, "Получено обновление для неизвестного или неактивного ордера расхеджирования.");
        return Ok(());
    };

    let leg = leg_found.unwrap();

    let mut needs_progress_update = false;
    if let Some(order_state) = match leg {
        Leg::Spot => task.state.active_spot_order.as_mut(),
        Leg::Futures => task.state.active_futures_order.as_mut(),
    } {
        let old_status = order_state.status.clone();
        order_state.update_from_details(&details);
        let new_filled_qty = order_state.filled_quantity;
        quantity_diff = new_filled_qty - old_filled_qty;
        avg_price = order_state.average_price;
        order_status_text = order_state.status.clone();

        let tolerance = dec!(1e-12);
        if quantity_diff.abs() > tolerance {
            needs_progress_update = true;
            if let Some(last_price_f64) = details.last_filled_price {
                 last_filled_price_opt = Decimal::try_from(last_price_f64).ok();
            }
        }

        if old_status != order_state.status &&
           matches!(order_state.status, OrderStatusText::Filled | OrderStatusText::Cancelled | OrderStatusText::PartiallyFilledCanceled | OrderStatusText::Rejected)
        {
            final_status_reached = true;
            info!(operation_id=task.operation_id, order_id=%order_state.order_id, final_status=?order_state.status, "Ордер расхеджирования {:?} достиг финального статуса.", leg);
        }
    } 

    if needs_progress_update {
        let price_for_diff = if avg_price > Decimal::ZERO {
            avg_price
        } else if let Some(last_price) = last_filled_price_opt {
            last_price
        } else {
            // Используем хелпер из unhedge_logic
            get_current_price(task, leg).ok_or_else(|| anyhow!("Не удалось определить цену для расчета стоимости для ордера расхеджирования {}", details.order_id))?
        };
        let value_diff = quantity_diff * price_for_diff;

        match leg {
            Leg::Spot => { // Продажа спота при расхеджировании
                task.state.cumulative_spot_filled_quantity += quantity_diff;
                // При продаже спота, стоимость (value) обычно положительная, если quantity_diff положительный (т.е. продали еще)
                // Если цена price_for_diff положительная, то value_diff будет иметь тот же знак, что и quantity_diff.
                // Мы хотим добавить абсолютное значение изменения стоимости к cumulative_spot_filled_value,
                // так как это отражает "высвобожденную" стоимость.
                task.state.cumulative_spot_filled_value += value_diff.abs(); 
            }
            Leg::Futures => { // Покупка фьючерса при расхеджировании
                task.state.cumulative_futures_filled_quantity += quantity_diff;
                // При покупке фьючерса, стоимость (value) обычно отрицательная (если quantity_diff положительный, т.е. купили еще)
                // так как это затраты. Мы хотим добавить абсолютное значение изменения стоимости.
                task.state.cumulative_futures_filled_value += value_diff.abs();
            }
        }
        // Используем хелпер из unhedge_logic
        send_progress_update(task).await?;
    }

    if final_status_reached {
        match leg {
            Leg::Spot => task.state.active_spot_order = None,
            Leg::Futures => task.state.active_futures_order = None,
        }
        debug!(operation_id=task.operation_id, order_id=%details.order_id, final_status=?order_status_text, "Очищено активное состояние для ордера расхеджирования {:?}.", leg);
    }

    Ok(())
}

// Обработка обновления стакана (Unhedge) - эта функция остается без изменений
async fn handle_order_book_update(task: &mut HedgerWsUnhedgeTask, symbol: String, bids: Vec<OrderbookLevel>, asks: Vec<OrderbookLevel>, _is_snapshot: bool) -> Result<()> {
    let market_data_ref = if symbol == task.state.symbol_spot { &mut task.state.spot_market_data }
                          else if symbol == task.state.symbol_futures { &mut task.state.futures_market_data }
                          else { return Ok(()); };
    market_data_ref.best_bid_price = bids.first().map(|l| l.price);
    market_data_ref.best_bid_quantity = bids.first().map(|l| l.quantity);
    market_data_ref.best_ask_price = asks.first().map(|l| l.price);
    market_data_ref.best_ask_quantity = asks.first().map(|l| l.quantity);
    market_data_ref.last_update_time_ms = Some(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64);
    Ok(())
}

// Обработка публичных сделок (Unhedge) - эта функция остается без изменений
async fn handle_public_trade_update(task: &mut HedgerWsUnhedgeTask, symbol: String, price: f64, qty: f64, side: OrderSide, timestamp: i64) -> Result<()> {
    trace!(operation_id = task.operation_id, %symbol, %price, %qty, ?side, %timestamp, "Получена публичная сделка во время расхеджирования (в настоящее время игнорируется)");
    Ok(())
}