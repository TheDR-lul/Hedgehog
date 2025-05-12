// src/webservice_hedge/hedge_logic/ws_handlers.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{debug, error, info, warn, trace};

use crate::exchange::types::{WebSocketMessage, DetailedOrderStatus, OrderSide, OrderStatusText, OrderbookLevel};
use crate::webservice_hedge::hedge_task::HedgerWsHedgeTask;
use crate::webservice_hedge::state::{HedgerWsStatus, Leg, OperationType};
use crate::webservice_hedge::hedge_logic::order_management::handle_cancel_confirmation;
use crate::webservice_hedge::hedge_logic::helpers::{get_current_price, send_progress_update};

// handle_websocket_message теперь принимает stream_category
pub async fn handle_websocket_message(
    task: &mut HedgerWsHedgeTask,
    message: WebSocketMessage,
    stream_category: &str, // "private", "spot", или "linear"
) -> Result<()> {
    match message {
        WebSocketMessage::OrderUpdate(details) => {
            if stream_category != "private" {
                warn!(operation_id = task.operation_id, "OrderUpdate received on non-private stream '{}'. Ignoring.", stream_category);
                return Ok(());
            }
            // Существующая логика обработки OrderUpdate
            let status_clone = task.state.status.clone();
            if let HedgerWsStatus::WaitingCancelConfirmation { ref cancelled_order_id, cancelled_leg, .. } = status_clone {
                 if details.order_id == *cancelled_order_id {
                     info!(operation_id=task.operation_id, order_id=%details.order_id, "Received update for the order being cancelled.");
                     handle_order_update(task, details.clone()).await?;

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
            handle_order_update(task, details).await?;
        }
        WebSocketMessage::OrderBookL2 { symbol, bids, asks, is_snapshot } => {
            // Передаем stream_category в handle_order_book_update
            handle_order_book_update(task, symbol, bids, asks, is_snapshot, stream_category).await?;
            // Проверка на "устаревшие" ордера должна запускаться только для публичных потоков данных
            if stream_category == "spot" || stream_category == "linear" {
                 // Убедитесь, что super::order_management::check_stale_orders существует и корректно работает
                 // Возможно, он должен быть частью `crate::webservice_hedge::hedge_logic::order_management`
                 match crate::webservice_hedge::hedge_logic::order_management::check_stale_orders(task).await {
                     Ok(_) => {},
                     Err(e) => warn!(operation_id = task.operation_id, "Error in check_stale_orders: {}", e),
                 }
            }
        }
        WebSocketMessage::PublicTrade { symbol, price, qty, side, timestamp } => {
             handle_public_trade_update(task, symbol, price, qty, side, timestamp, stream_category).await?;
        }
        WebSocketMessage::Error(error_message) => {
            warn!(operation_id = task.operation_id, stream_category, %error_message, "Received error message from WebSocket stream");
        }
        WebSocketMessage::Disconnected => {
             error!(operation_id = task.operation_id, stream_category, "WebSocket disconnected event received.");
             // В HedgerWsHedgeTask::run циклы select! для каждого receiver обработают закрытие канала (None)
             // и примут решение о дальнейших действиях. Здесь мы просто логируем.
        }
        WebSocketMessage::Pong => { debug!(operation_id = task.operation_id, stream_category, "Pong received"); }
        WebSocketMessage::Authenticated(success) => {
            if stream_category == "private" {
                info!(operation_id = task.operation_id, success, "WS Authenticated status received for private stream");
            } else {
                warn!(operation_id = task.operation_id, success, stream_category, "Authenticated message received on non-private stream!");
            }
        }
        WebSocketMessage::SubscriptionResponse { success, ref topic } => {
            info!(operation_id = task.operation_id, stream_category, success, %topic, "WS Subscription response received");
        }
        WebSocketMessage::Connected => {
            info!(operation_id = task.operation_id, stream_category, "WS Connected event received (likely redundant here, handled at connection setup)");
        }
    }
    Ok(())
}

// handle_order_update остается без изменений по сигнатуре, но его логика может быть уточнена
async fn handle_order_update(task: &mut HedgerWsHedgeTask, details: DetailedOrderStatus) -> Result<()> {
    info!(operation_id = task.operation_id, order_id = %details.order_id, status = ?details.status_text, filled_qty = details.filled_qty, "Handling order update");

    let operation_type = task.state.operation_type;
    let tolerance = dec!(1e-12);
    let mut leg_found = None;
    let mut quantity_diff = Decimal::ZERO;
    let mut avg_price = Decimal::ZERO;
    let mut last_filled_price_opt: Option<Decimal> = None;
    let mut final_status_reached = false;
    let mut order_status_text_for_log = OrderStatusText::Unknown("".to_string());

    let old_filled_qty = if task.state.active_spot_order.as_ref().map_or(false, |o| o.order_id == details.order_id) {
        leg_found = Some(Leg::Spot);
        task.state.active_spot_order.as_ref().map(|o| o.filled_quantity).unwrap_or_default()
    } else if task.state.active_futures_order.as_ref().map_or(false, |o| o.order_id == details.order_id) {
        leg_found = Some(Leg::Futures);
        task.state.active_futures_order.as_ref().map(|o| o.filled_quantity).unwrap_or_default()
    } else {
        warn!(operation_id = task.operation_id, order_id = %details.order_id, "Received update for an unknown or inactive order.");
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
        order_status_text_for_log = order_state.status.clone(); // Для логирования ниже

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
            info!(operation_id=task.operation_id, order_id=%order_state.order_id, final_status=?order_state.status, "{:?} order reached final state.", leg);
        }
    }

    if needs_progress_update {
        let price_for_diff = if avg_price > Decimal::ZERO { avg_price }
                             else if let Some(last_price) = last_filled_price_opt { last_price }
                             else { get_current_price(task, leg).ok_or_else(|| anyhow!("Cannot determine price for value calc for order {}", details.order_id))? };
        let value_diff = quantity_diff * price_for_diff;

        match leg {
            Leg::Spot => {
                task.state.cumulative_spot_filled_quantity += quantity_diff;
                task.state.cumulative_spot_filled_value += value_diff; // Покупка спота - значение положительное
                if operation_type == OperationType::Hedge { // Только для хеджа обновляем цель фьюча
                    task.state.target_total_futures_value = task.state.cumulative_spot_filled_value;
                    debug!(operation_id = task.operation_id, new_target_fut_value = %task.state.target_total_futures_value, "Updated target futures value based on spot fill.");
                }
            }
            Leg::Futures => {
                task.state.cumulative_futures_filled_quantity += quantity_diff;
                // Продажа фьюча - стоимость отрицательная, но для кумулятивного значения берем модуль
                task.state.cumulative_futures_filled_value += value_diff.abs();
            }
        }
        send_progress_update(task).await?;
    }

    if final_status_reached {
        match leg {
            Leg::Spot => task.state.active_spot_order = None,
            Leg::Futures => task.state.active_futures_order = None,
        }
        debug!(operation_id=task.operation_id, order_id=%details.order_id, final_status=?order_status_text_for_log, "Cleared active state for {:?} order.", leg);
    }
    Ok(())
}


// handle_order_book_update теперь принимает stream_category
async fn handle_order_book_update(
    task: &mut HedgerWsHedgeTask,
    symbol: String,
    bids: Vec<OrderbookLevel>,
    asks: Vec<OrderbookLevel>,
    _is_snapshot: bool, // is_snapshot может быть полезен для полной перезаписи стакана
    stream_category: &str, // "spot" или "linear"
) -> Result<()> {
    // Выбираем, какой MarketUpdate обновлять, на основе категории потока
    let market_data_ref = match stream_category {
        "spot" if symbol == task.state.symbol_spot => {
            // Убедимся, что символ из сообщения совпадает с ожидаемым спотовым символом
            Some(&mut task.state.spot_market_data)
        }
        "linear" if symbol == task.state.symbol_futures => {
            // Убедимся, что символ из сообщения совпадает с ожидаемым фьючерсным символом
            Some(&mut task.state.futures_market_data)
        }
        _ => {
            // Это может произойти, если символ в сообщении не совпадает с ожидаемыми
            // или категория потока неожиданная для данного сообщения ордербука.
            warn!(operation_id = task.operation_id,
                  "OrderBook update for symbol '{}' from stream_category '{}' does not match expected symbols (spot: '{}', futures: '{}'). Ignoring.",
                  symbol, stream_category, task.state.symbol_spot, task.state.symbol_futures);
            return Ok(());
        }
    };

    if let Some(data_ref) = market_data_ref {
        data_ref.best_bid_price = bids.first().map(|l| l.price);
        data_ref.best_bid_quantity = bids.first().map(|l| l.quantity);
        data_ref.best_ask_price = asks.first().map(|l| l.price);
        data_ref.best_ask_quantity = asks.first().map(|l| l.quantity);
        data_ref.last_update_time_ms = Some(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64);
        trace!(operation_id = task.operation_id, %symbol, category = stream_category, "Market data updated.");
    }
    Ok(())
}

// handle_public_trade_update теперь тоже может использовать stream_category, если это необходимо
async fn handle_public_trade_update(
    task: &mut HedgerWsHedgeTask,
    symbol: String,
    price: f64,
    qty: f64,
    side: OrderSide,
    timestamp: i64,
    stream_category: &str,
) -> Result<()> {
    trace!(operation_id = task.operation_id, stream_category, %symbol, %price, %qty, ?side, %timestamp, "Received public trade (currently ignored by main logic)");
    // Здесь можно добавить логику, если публичные сделки влияют на принятие решений,
    // и эта логика зависит от того, спотовый это рынок или фьючерсный.
    Ok(())
}