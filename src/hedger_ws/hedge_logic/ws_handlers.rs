// src/hedger_ws/hedge_logic/ws_handlers.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{debug, error, info, warn, trace};

use crate::exchange::types::{WebSocketMessage, DetailedOrderStatus, OrderSide, OrderStatusText, OrderbookLevel};
use crate::hedger_ws::hedge_task::HedgerWsHedgeTask;
use crate::hedger_ws::state::{HedgerWsStatus, Leg, OperationType}; // Добавили OperationType
use crate::hedger_ws::hedge_logic::order_management::handle_cancel_confirmation;
use crate::hedger_ws::hedge_logic::helpers::{get_current_price, send_progress_update};

// ... handle_websocket_message, handle_order_book_update, handle_public_trade_update без изменений ...
pub async fn handle_websocket_message(task: &mut HedgerWsHedgeTask, message: WebSocketMessage) -> Result<()> {
    match message {
        WebSocketMessage::OrderUpdate(details) => {
            let status_clone = task.state.status.clone();
            if let HedgerWsStatus::WaitingCancelConfirmation { ref cancelled_order_id, cancelled_leg, .. } = status_clone {
                 if details.order_id == *cancelled_order_id {
                     info!(operation_id=task.operation_id, order_id=%details.order_id, "Received update for the order being cancelled.");
                     handle_order_update(task, details.clone()).await?; // <-- Вызываем ИСПРАВЛЕННУЮ версию ниже

                     let order_is_inactive = match cancelled_leg {
                         Leg::Spot => task.state.active_spot_order.is_none(),
                         Leg::Futures => task.state.active_futures_order.is_none(),
                     };

                     if matches!(task.state.status, HedgerWsStatus::WaitingCancelConfirmation {.. }) && order_is_inactive {
                          handle_cancel_confirmation(task, cancelled_order_id, cancelled_leg).await?;
                     } else {
                          debug!(operation_id=task.operation_id, order_id=%cancelled_order_id, status=?task.state.status, order_is_inactive, "Order update received, but order still active or state changed. Not calling handle_cancel_confirmation yet.");
                     }
                     return Ok(());
                 }
            }
            handle_order_update(task, details).await?; // <-- Вызываем ИСПРАВЛЕННУЮ версию ниже
        }
        WebSocketMessage::OrderBookL2 { symbol, bids, asks, is_snapshot } => {
            handle_order_book_update(task, symbol, bids, asks, is_snapshot).await?;
            super::order_management::check_stale_orders(task).await?;
        }
        WebSocketMessage::PublicTrade { symbol, price, qty, side, timestamp } => {
             handle_public_trade_update(task, symbol, price, qty, side, timestamp).await?;
        }
        WebSocketMessage::Error(error_message) => {
            warn!(operation_id = task.operation_id, %error_message, "Received error message from WebSocket stream");
        }
        WebSocketMessage::Disconnected => {
             error!(operation_id = task.operation_id, "WebSocket disconnected event received.");
             return Err(anyhow!("WebSocket disconnected"));
        }
        WebSocketMessage::Pong => { debug!(operation_id = task.operation_id, "Pong received"); }
        WebSocketMessage::Authenticated(success) => { info!(operation_id = task.operation_id, success, "WS Authenticated status received"); }
        WebSocketMessage::SubscriptionResponse { success, ref topic } => { info!(operation_id = task.operation_id, success, %topic, "WS Subscription response received"); }
        WebSocketMessage::Connected => { info!(operation_id = task.operation_id, "WS Connected event received (likely redundant)"); }
    }
    Ok(())
}


// --- ПЕРЕРАБОТАННАЯ ФУНКЦИЯ handle_order_update ---
async fn handle_order_update(task: &mut HedgerWsHedgeTask, details: DetailedOrderStatus) -> Result<()> {
    info!(operation_id = task.operation_id, order_id = %details.order_id, status = ?details.status_text, filled_qty = details.filled_qty, "Handling order update");

    let operation_type = task.state.operation_type; // Читаем сразу
    let tolerance = dec!(1e-12);
    let mut leg_found = None;
    let mut quantity_diff = Decimal::ZERO;
    let mut avg_price = Decimal::ZERO;
    let mut last_filled_price_opt: Option<Decimal> = None;
    let mut final_status_reached = false;
    let mut order_status_text = OrderStatusText::Unknown("".to_string()); // Для логики после

    // Этап 1: Определяем ногу и читаем старое значение filled_qty (immutable borrow)
    let old_filled_qty = if task.state.active_spot_order.as_ref().map_or(false, |o| o.order_id == details.order_id) {
        leg_found = Some(Leg::Spot);
        task.state.active_spot_order.as_ref().map(|o| o.filled_quantity).unwrap_or_default()
    } else if task.state.active_futures_order.as_ref().map_or(false, |o| o.order_id == details.order_id) {
        leg_found = Some(Leg::Futures);
        task.state.active_futures_order.as_ref().map(|o| o.filled_quantity).unwrap_or_default()
    } else {
        warn!(operation_id = task.operation_id, order_id = %details.order_id, "Received update for an unknown or inactive order (checked both legs).");
        return Ok(()); // Выходим, если ордер не найден
    };

    let leg = leg_found.unwrap(); // Мы уже вышли, если None

    // Этап 2: Обновляем состояние конкретного ордера (mutable borrow)
    let mut needs_progress_update = false;
    if let Some(order_state) = match leg { // Получаем &mut ссылки внутри этого блока
        Leg::Spot => task.state.active_spot_order.as_mut(),
        Leg::Futures => task.state.active_futures_order.as_mut(),
    } {
        let old_status = order_state.status.clone();
        order_state.update_from_details(&details); // Мутабельный вызов
        let new_filled_qty = order_state.filled_quantity;
        quantity_diff = new_filled_qty - old_filled_qty; // Сохраняем разницу
        avg_price = order_state.average_price; // Сохраняем среднюю цену для расчета стоимости
        order_status_text = order_state.status.clone(); // Сохраняем текущий статус ордера

        if quantity_diff.abs() > tolerance {
            needs_progress_update = true;
            if let Some(last_price_f64) = details.last_filled_price {
                 last_filled_price_opt = Decimal::try_from(last_price_f64).ok();
            }
        }

        // Проверяем, достиг ли ордер финального статуса
        if old_status != order_state.status &&
           matches!(order_state.status, OrderStatusText::Filled | OrderStatusText::Cancelled | OrderStatusText::PartiallyFilledCanceled | OrderStatusText::Rejected)
        {
            final_status_reached = true;
            info!(operation_id=task.operation_id, order_id=%order_state.order_id, final_status=?order_state.status, "{:?} order reached final state.", leg);
        }
    } // Мутабельное заимствование order_state заканчивается здесь

    // Этап 3: Обновляем кумулятивные значения и отправляем колбэк (mutable borrow task)
    if needs_progress_update {
        // Определяем цену для расчета изменения стоимости
        let price_for_diff = if avg_price > Decimal::ZERO {
            avg_price
        } else if let Some(last_price) = last_filled_price_opt {
            last_price
        } else {
            // Заимствуем task иммутабельно для get_current_price
            get_current_price(task, leg).ok_or_else(|| anyhow!("Cannot determine price for value calc for order {}", details.order_id))?
        };
        let value_diff = quantity_diff * price_for_diff;

        // Обновляем кумулятивные значения (мутабельное заимствование task.state)
        match leg {
            Leg::Spot => {
                task.state.cumulative_spot_filled_quantity += quantity_diff;
                task.state.cumulative_spot_filled_value += value_diff;
                if operation_type == OperationType::Hedge {
                    task.state.target_total_futures_value = task.state.cumulative_spot_filled_value;
                    debug!(operation_id = task.operation_id, new_target_fut_value = %task.state.target_total_futures_value, "Updated target futures value based on spot fill.");
                }
            }
            Leg::Futures => {
                task.state.cumulative_futures_filled_quantity += quantity_diff;
                task.state.cumulative_futures_filled_value += value_diff.abs();
            }
        }
        // Отправляем колбэк (мутабельное заимствование task)
        send_progress_update(task).await?;
    }

    // Этап 4: Обнуляем активный ордер, если он достиг финального статуса
    if final_status_reached {
        match leg {
            Leg::Spot => task.state.active_spot_order = None,
            Leg::Futures => task.state.active_futures_order = None,
        }
        debug!(operation_id=task.operation_id, order_id=%details.order_id, final_status=?order_status_text, "Cleared active state for {:?} order.", leg);
    }

    Ok(())
}

// ... (handle_order_book_update и handle_public_trade_update без изменений) ...
async fn handle_order_book_update(task: &mut HedgerWsHedgeTask, symbol: String, bids: Vec<OrderbookLevel>, asks: Vec<OrderbookLevel>, _is_snapshot: bool) -> Result<()> {
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
async fn handle_public_trade_update(task: &mut HedgerWsHedgeTask, symbol: String, price: f64, qty: f64, side: OrderSide, timestamp: i64) -> Result<()> {
    trace!(operation_id = task.operation_id, %symbol, %price, %qty, ?side, %timestamp, "Received public trade (currently ignored)");
    Ok(())
}