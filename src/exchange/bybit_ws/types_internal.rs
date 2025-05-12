// src/exchange/bybit_ws/types_internal.rs

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;
use crate::exchange::types::{OrderSide, OrderStatusText, DetailedOrderStatus, OrderbookLevel};
use std::str::FromStr;
use anyhow::anyhow;

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsResponse {
    pub(super) op: Option<String>,
    pub(super) conn_id: Option<String>,
    // Сделаем req_id изменяемым, чтобы можно было его заполнить для логов
    #[serde(default)] pub(super) req_id: Option<String>, 
    pub(super) success: Option<bool>,
    pub(super) ret_msg: Option<String>,
    pub(super) topic: Option<String>,
    #[serde(rename = "type")]
    pub(super) message_type: Option<String>, // "snapshot" или "delta" для ордербука
    pub(super) data: Option<Value>,
    pub(super) ts: Option<i64>, // Таймстемп сообщения от биржи (event time)
    #[serde(rename = "creationTime")]
    pub(super) _creation_time: Option<i64>, // Не используется активно
    #[serde(rename = "pong")]
    pub(super) _pong_ts: Option<i64>, // Не используется активно
    // Для ордербука V5, seq и u могут быть в data, а cts на верхнем уровне
    pub(super) cts: Option<i64>, // Таймстемп создания сообщения на клиенте Bybit
}

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsOrderData {
    #[serde(rename = "orderId")] pub(super) order_id: String,
    pub(super) symbol: String,
    pub(super) side: String, // "Buy" или "Sell"
    #[serde(rename = "orderStatus")] pub(super) status: String, // e.g., "New", "Filled"
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) cum_exec_qty: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) cum_exec_value: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) avg_price: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) leaves_qty: Option<f64>, // Оставшееся количество
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) last_exec_qty: Option<f64>, // Имя поля в V5 может быть 'lastExecQty'
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) last_exec_price: Option<f64>,// Имя поля в V5 может быть 'lastExecPrice'
    #[serde(rename = "rejectReason")] pub(super) reject_reason: Option<String>,
    // Дополнительные поля из V5, если нужны
    // #[serde(rename = "orderIv")] pub(super) order_iv: Option<String>,
    // #[serde(rename = "triggerPrice")] pub(super) trigger_price: Option<String>,
    // #[serde(rename = "takeProfit")] pub(super) take_profit: Option<String>,
    // #[serde(rename = "stopLoss")] pub(super) stop_loss: Option<String>,
    // #[serde(rename = "tpTriggerBy")] pub(super) tp_trigger_by: Option<String>,
    // #[serde(rename = "slTriggerBy")] pub(super) sl_trigger_by: Option<String>,
    // #[serde(rename = "createdTime")] pub(super) created_time: Option<String>,
    // #[serde(rename = "updatedTime")] pub(super) updated_time: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsTradeData {
    #[serde(rename = "T")] pub(super) timestamp: i64, // Trade time
    #[serde(rename = "s")] pub(super) symbol: String,
    #[serde(rename = "S")] pub(super) side: String, // Side of taker
    #[serde(rename = "v", with = "super::protocol::str_or_empty_as_f64")] pub(super) qty: f64, // Trade size
    #[serde(rename = "p", with = "super::protocol::str_or_empty_as_f64")] pub(super) price: f64, // Trade price
    // i: Option<String>, // Trade ID - не используем пока
    // L: Option<String>, // Side of maker - не используем пока
    // BT: Option<bool>, // Block trade - не используем пока
}

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsOrderbookData {
    #[serde(rename = "s")] pub(super) symbol: String, // Symbol name
    #[serde(rename = "b")] pub(super) bids: Vec<[String; 2]>, // Bid levels: [price, size]
    #[serde(rename = "a")] pub(super) asks: Vec<[String; 2]>, // Ask levels: [price, size]
    #[serde(rename = "u")] pub(super) update_id: i64, // Update ID. Is unique within a single op_id stream.
    #[serde(default)] pub(super) seq: Option<i64>, // Sequence no. Only used in a option
    // ts из BybitWsResponse используется как event time
}


// --- Функции парсинга конкретных данных ---

pub(super) fn parse_order_update(data: Value, _event_ts: Option<i64>) -> Result<DetailedOrderStatus, anyhow::Error> {
    // order_data может быть массивом в V5, берем первый элемент
    let orders_data_array: Vec<BybitWsOrderData> = serde_json::from_value(data.clone()) // Клонируем data для лога при ошибке
        .map_err(|e| anyhow!("Не удалось распарсить массив данных ордера: {}. Data: {:?}", e, data))?;

    if let Some(order_data) = orders_data_array.into_iter().next() {
        let status_text = OrderStatusText::from_str(&order_data.status)
            .unwrap_or_else(|_| {
                warn!("Неизвестный статус ордера от WS: '{}'", order_data.status);
                OrderStatusText::Unknown(order_data.status.clone())
            });
        let side = OrderSide::from_str(&order_data.side)
            .map_err(|e| anyhow!("Неверная сторона ордера от WS: '{}': {}", order_data.side, e))?;

        Ok(DetailedOrderStatus {
            order_id: order_data.order_id,
            symbol: order_data.symbol,
            side,
            filled_qty: order_data.cum_exec_qty.unwrap_or(0.0),
            remaining_qty: order_data.leaves_qty.unwrap_or(0.0),
            cumulative_executed_value: order_data.cum_exec_value.unwrap_or(0.0),
            average_price: order_data.avg_price.unwrap_or(0.0),
            status_text,
            last_filled_qty: order_data.last_exec_qty, // Используем last_exec_qty
            last_filled_price: order_data.last_exec_price, // Используем last_exec_price
            reject_reason: order_data.reject_reason,
        })
    } else {
        Err(anyhow!("Получен пустой массив данных для топика ордера. Data: {:?}", data))
    }
}

pub(super) fn parse_orderbook_update(data: Value, _event_ts: Option<i64>) -> Result<(String, Vec<OrderbookLevel>, Vec<OrderbookLevel>), anyhow::Error> {
     // В V5 'data' для ордербука уже является объектом BybitWsOrderbookData
     let book_data: BybitWsOrderbookData = serde_json::from_value(data.clone()) // Клонируем для лога при ошибке
         .map_err(|e| anyhow!("Не удалось распарсить данные ордербука: {}. Data: {:?}", e, data))?;

     let parse_level = |level_str_array: [String; 2]| -> Result<OrderbookLevel, anyhow::Error> {
         let price = Decimal::from_str(&level_str_array[0])
            .map_err(|e| anyhow!("Не удалось распарсить цену ордербука '{}': {}", level_str_array[0], e))?;
         let quantity = Decimal::from_str(&level_str_array[1])
            .map_err(|e| anyhow!("Не удалось распарсить количество ордербука '{}': {}", level_str_array[1], e))?;
         if quantity < Decimal::ZERO {
            warn!("Получено отрицательное количество в уровне ордербука: [{}, {}]", price, quantity);
            // Можно либо вернуть ошибку, либо проигнорировать этот уровень, либо взять abs()
            // Для простоты пока оставим как есть, но это странно.
         }
         Ok(OrderbookLevel { price, quantity })
     };

     let bids = book_data.bids.into_iter().map(parse_level).collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("Ошибка парсинга уровней бид: {}", e))?;
     let asks = book_data.asks.into_iter().map(parse_level).collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("Ошибка парсинга уровней аск: {}", e))?;

     Ok((book_data.symbol, bids, asks))
}

 pub(super) fn parse_public_trade_update(data: Value) -> Result<Option<(String, f64, f64, OrderSide, i64)>, anyhow::Error> {
     let trades_data_array: Vec<BybitWsTradeData> = serde_json::from_value(data.clone()) // Клонируем для лога
         .map_err(|e| anyhow!("Не удалось распарсить массив данных публичных сделок: {}. Data: {:?}", e, data))?;
     
     // Обычно Bybit отправляет массив из одного элемента для publicTrade
     if let Some(trade_data) = trades_data_array.into_iter().next() {
         let side = OrderSide::from_str(&trade_data.side)
            .map_err(|e| anyhow!("Неверная сторона публичной сделки от WS: '{}': {}", trade_data.side, e))?;
         Ok(Some((
             trade_data.symbol,
             trade_data.price,
             trade_data.qty,
             side,
             trade_data.timestamp,
         )))
     } else {
         warn!("Получен пустой массив данных для топика publicTrade. Data: {:?}", data);
         Ok(None)
     }
 }