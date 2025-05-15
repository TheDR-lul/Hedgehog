// src/webservice_hedge/hedge_logic/ws_handlers.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
// ИЗМЕНЕНО: Добавил error в импорт tracing
use tracing::{debug, error, info, warn, trace};

use crate::exchange::types::{WebSocketMessage, DetailedOrderStatus, OrderSide, OrderStatusText, OrderbookLevel};
use crate::webservice_hedge::hedge_task::HedgerWsHedgeTask;
use crate::webservice_hedge::state::{HedgerWsStatus, Leg, OperationType, ChunkOrderState};
use crate::webservice_hedge::hedge_logic::order_management::handle_cancel_confirmation;
use crate::webservice_hedge::hedge_logic::helpers::{get_current_price, send_progress_update};
use crate::hedger::ORDER_FILL_TOLERANCE;

const ORDER_FILL_TOLERANCE_DECIMAL: Decimal = dec!(1e-8);


pub async fn handle_websocket_message(
    task: &mut HedgerWsHedgeTask,
    message: WebSocketMessage,
    stream_category: &str,
) -> Result<()> {
    match message {
        WebSocketMessage::OrderUpdate(details) => {
            // ИЗМЕНЕНО: Логирование входящей структуры details
            info!(operation_id = task.operation_id, stream_category, "Received OrderUpdate details: {:?}", details);

            if stream_category != "private" {
                warn!(operation_id = task.operation_id, "OrderUpdate received on non-private stream '{}'. Ignoring.", stream_category);
                return Ok(());
            }
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
            handle_order_book_update(task, symbol, bids, asks, is_snapshot, stream_category).await?;
            if stream_category == "spot" || stream_category == "linear" {
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
            info!(operation_id = task.operation_id, stream_category, "WS Connected event received");
        }
    }
    Ok(())
}

async fn handle_order_update(task: &mut HedgerWsHedgeTask, details: DetailedOrderStatus) -> Result<()> {
    // ИЗМЕНЕНО: Логирование деталей еще раз в начале этой специфичной функции
    debug!(operation_id = task.operation_id, order_id = %details.order_id, "Entering handle_order_update with details: {:?}", details);
    info!(operation_id = task.operation_id, order_id = %details.order_id, status = ?details.status_text, raw_filled_qty_from_details = details.filled_qty, "Handling order update");

    let operation_type = task.state.operation_type;
    let mut leg_found = None;
    let mut quantity_diff = Decimal::ZERO; 
    let mut value_diff = Decimal::ZERO;    
    let mut avg_price_from_state = Decimal::ZERO;
    let mut last_filled_price_opt: Option<Decimal> = None;
    let mut final_status_reached = false;
    let mut order_status_text_for_log = details.status_text.clone(); 

    let old_filled_qty_in_state: Decimal = if task.state.active_spot_order.as_ref().map_or(false, |o| o.order_id == details.order_id) {
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
        let old_order_status_in_state = order_state.status.clone();
        
        order_state.update_from_details(&details);
        avg_price_from_state = order_state.average_price; 

        let mut current_filled_qty_for_calc = order_state.filled_quantity; 

        if order_state.status == OrderStatusText::Filled &&
           order_state.target_quantity > Decimal::ZERO { 
            if current_filled_qty_for_calc < order_state.target_quantity * dec!(0.999) || current_filled_qty_for_calc.abs() < ORDER_FILL_TOLERANCE_DECIMAL {
                 warn!(operation_id = task.operation_id, order_id = %details.order_id, leg = ?leg,
                      status = ?order_state.status, parsed_filled_qty = %current_filled_qty_for_calc, target_qty = %order_state.target_quantity,
                      "Order status is 'Filled' but parsed 'filled_qty' is unexpectedly low/zero. \
                      For accounting, assuming full execution to target_quantity.");
                 current_filled_qty_for_calc = order_state.target_quantity;
                 order_state.filled_quantity = current_filled_qty_for_calc; 

                 if avg_price_from_state > Decimal::ZERO {
                     order_state.filled_value = current_filled_qty_for_calc * avg_price_from_state;
                 } else if order_state.limit_price > Decimal::ZERO {
                     order_state.filled_value = current_filled_qty_for_calc * order_state.limit_price;
                     warn!(operation_id = task.operation_id, order_id = %details.order_id, leg = ?leg, "Corrected filled_value using limit_price as avg_price was zero.");
                 }
            }
        }
        
        quantity_diff = current_filled_qty_for_calc - old_filled_qty_in_state;
        // ИЗМЕНЕНО: Логируем quantity_diff
        debug!(operation_id = task.operation_id, order_id = %details.order_id, leg = ?leg, %current_filled_qty_for_calc, %old_filled_qty_in_state, %quantity_diff, "Calculated quantity_diff");

        if quantity_diff.abs() > ORDER_FILL_TOLERANCE_DECIMAL {
            needs_progress_update = true;
            if let Some(last_price_f64) = details.last_filled_price { 
                 last_filled_price_opt = Decimal::try_from(last_price_f64).ok();
            }
        }

        if old_order_status_in_state != order_state.status &&
           matches!(order_state.status, OrderStatusText::Filled | OrderStatusText::Cancelled | OrderStatusText::PartiallyFilledCanceled | OrderStatusText::Rejected)
        {
            final_status_reached = true;
            info!(operation_id=task.operation_id, order_id=%order_state.order_id, final_status=?order_state.status, "{:?} order reached final state.", leg);
        }
    }

    if needs_progress_update {
        // ИЗМЕНЕНО: Логирование перед определением price_for_value_diff_calc
        debug!(operation_id = task.operation_id, order_id = %details.order_id, leg = ?leg, %avg_price_from_state, ?last_filled_price_opt, "Determining price_for_value_diff_calc.");

        let price_for_value_diff_calc = {
            if avg_price_from_state > Decimal::ZERO {
                debug!(operation_id = task.operation_id, order_id = %details.order_id, "Using avg_price_from_state: {}", avg_price_from_state);
                avg_price_from_state
            } else if let Some(last_price) = last_filled_price_opt {
                if last_price > Decimal::ZERO {
                    debug!(operation_id = task.operation_id, order_id = %details.order_id, "Using last_filled_price_opt: {}", last_price);
                    last_price
                } else {
                    debug!(operation_id = task.operation_id, order_id = %details.order_id, "last_filled_price_opt is zero or negative ({}), falling back to get_current_price.", last_price);
                    let fallback_price = get_current_price(task, leg).ok_or_else(|| anyhow!("Cannot determine price for value calculation for order {}", details.order_id))?;
                    debug!(operation_id = task.operation_id, order_id = %details.order_id, "Fallback get_current_price returned: {}", fallback_price);
                    fallback_price
                }
            } else {
                debug!(operation_id = task.operation_id, order_id = %details.order_id, "avg_price_from_state is zero and last_filled_price_opt is None, falling back to get_current_price.");
                let fallback_price = get_current_price(task, leg).ok_or_else(|| anyhow!("Cannot determine price for value calculation for order {}", details.order_id))?;
                debug!(operation_id = task.operation_id, order_id = %details.order_id, "Fallback get_current_price returned: {}", fallback_price);
                fallback_price
            }
        };
        
        value_diff = quantity_diff * price_for_value_diff_calc; 
        debug!(operation_id = task.operation_id, order_id = %details.order_id, leg = ?leg, %quantity_diff, %price_for_value_diff_calc, %value_diff, "Calculated value_diff");


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
        info!(operation_id=task.operation_id, order_id=%details.order_id, leg=?leg, %quantity_diff, %value_diff, cum_spot_qty=%task.state.cumulative_spot_filled_quantity, cum_spot_val=%task.state.cumulative_spot_filled_value, cum_fut_qty=%task.state.cumulative_futures_filled_quantity, cum_fut_val=%task.state.cumulative_futures_filled_value, "Cumulative quantities and values updated.");
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

async fn handle_order_book_update(
    task: &mut HedgerWsHedgeTask,
    symbol: String,
    bids: Vec<OrderbookLevel>,
    asks: Vec<OrderbookLevel>,
    _is_snapshot: bool,
    stream_category: &str,
) -> Result<()> {
    let market_data_ref = match stream_category {
        "spot" if symbol == task.state.symbol_spot => Some(&mut task.state.spot_market_data),
        "linear" if symbol == task.state.symbol_futures => Some(&mut task.state.futures_market_data),
        _ => {
            warn!(operation_id = task.operation_id,
                  "OrderBook update for symbol '{}' from stream_category '{}' does not match expected symbols (spot: '{}', futures: '{}'). Ignoring.",
                  symbol, stream_category, task.state.symbol_spot, task.state.symbol_futures);
            return Ok(());
        }
    };

    if let Some(data_ref) = market_data_ref {
        let old_bid = data_ref.best_bid_price;
        let old_ask = data_ref.best_ask_price;
        data_ref.best_bid_price = bids.first().map(|l| l.price);
        data_ref.best_bid_quantity = bids.first().map(|l| l.quantity);
        data_ref.best_ask_price = asks.first().map(|l| l.price);
        data_ref.best_ask_quantity = asks.first().map(|l| l.quantity);
        data_ref.last_update_time_ms = Some(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64);
        
        // ИЗМЕНЕНО: Логируем только если цена изменилась, чтобы не спамить
        if old_bid != data_ref.best_bid_price || old_ask != data_ref.best_ask_price {
            debug!(operation_id = task.operation_id, %symbol, category = stream_category, "Market data updated. Best bid: {:?}, Best ask: {:?}", data_ref.best_bid_price, data_ref.best_ask_price);
        } else {
            trace!(operation_id = task.operation_id, %symbol, category = stream_category, "Market data received, but best bid/ask unchanged. Bid: {:?}, Ask: {:?}", data_ref.best_bid_price, data_ref.best_ask_price);
        }
    }
    Ok(())
}

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
    Ok(())
}