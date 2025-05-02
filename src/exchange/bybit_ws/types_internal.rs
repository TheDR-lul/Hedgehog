// src/exchange/bybit_ws/types_internal.rs

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;
use crate::exchange::types::{OrderSide, OrderStatusText, DetailedOrderStatus, OrderbookLevel}; // Импорт общих типов
use std::str::FromStr;
use anyhow::anyhow;

// Структуры для парсинга ответов
#[derive(Deserialize, Debug)]
pub(super) struct BybitWsResponse { // Делаем pub(super), чтобы были видны в protocol.rs
    pub(super) op: Option<String>,
    pub(super) conn_id: Option<String>,
    pub(super) req_id: Option<String>,
    pub(super) success: Option<bool>,
    pub(super) ret_msg: Option<String>,
    pub(super) topic: Option<String>,
    #[serde(rename = "type")]
    pub(super) message_type: Option<String>,
    pub(super) data: Option<Value>,
    pub(super) ts: Option<i64>,
    #[serde(rename = "creationTime")]
    pub(super) _creation_time: Option<i64>,
    #[serde(rename = "pong")]
    pub(super) _pong_ts: Option<i64>,
}

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsOrderData { // pub(super)
    #[serde(rename = "orderId")] pub(super) order_id: String,
    pub(super) symbol: String,
    pub(super) side: String,
    #[serde(rename = "orderStatus")] pub(super) status: String,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) cum_exec_qty: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) cum_exec_value: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) avg_price: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) leaves_qty: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) last_filled_qty: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) last_filled_price: Option<f64>,
    #[serde(rename = "rejectReason")] pub(super) reject_reason: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsTradeData { // pub(super)
    #[serde(rename = "T")] pub(super) timestamp: i64,
    #[serde(rename = "s")] pub(super) symbol: String,
    #[serde(rename = "S")] pub(super) side: String,
    #[serde(rename = "v", with = "super::protocol::str_or_empty_as_f64")] pub(super) qty: f64,
    #[serde(rename = "p", with = "super::protocol::str_or_empty_as_f64")] pub(super) price: f64,
}

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsOrderbookData { // pub(super)
    #[serde(rename = "s")] pub(super) symbol: String,
    #[serde(rename = "b")] pub(super) bids: Vec<[String; 2]>,
    #[serde(rename = "a")] pub(super) asks: Vec<[String; 2]>,
    #[serde(rename = "u")] pub(super) _update_id: i64,
    #[serde(default)] pub(super) _sequence: Option<i64>,
}

// --- Функции парсинга конкретных данных ---
// Делаем pub(super), чтобы были доступны в protocol.rs

pub(super) fn parse_order_update(data: Value) -> Result<DetailedOrderStatus, anyhow::Error> {
    let orders_data: Vec<BybitWsOrderData> = serde_json::from_value(data)
        .map_err(|e| anyhow!("Failed to parse order data array: {}", e))?;
    if let Some(order_data) = orders_data.into_iter().next() {
        let status_text = OrderStatusText::from_str(&order_data.status)
            .unwrap_or_else(|_| OrderStatusText::Unknown(order_data.status.clone()));
        let side = OrderSide::from_str(&order_data.side)?;

        Ok(DetailedOrderStatus {
            order_id: order_data.order_id,
            symbol: order_data.symbol,
            side,
            filled_qty: order_data.cum_exec_qty.unwrap_or(0.0),
            remaining_qty: order_data.leaves_qty.unwrap_or(0.0),
            cumulative_executed_value: order_data.cum_exec_value.unwrap_or(0.0),
            average_price: order_data.avg_price.unwrap_or(0.0),
            status_text,
            last_filled_qty: order_data.last_filled_qty,
            last_filled_price: order_data.last_filled_price,
            reject_reason: order_data.reject_reason,
        })
    } else {
        Err(anyhow!("Received empty data array for order topic"))
    }
}

pub(super) fn parse_orderbook_update(data: Value) -> Result<(String, Vec<OrderbookLevel>, Vec<OrderbookLevel>), anyhow::Error> {
     let book_data: BybitWsOrderbookData = serde_json::from_value(data)
         .map_err(|e| anyhow!("Failed to parse orderbook data: {}", e))?;

     let parse_level = |level: [String; 2]| -> Result<OrderbookLevel, anyhow::Error> {
         Ok(OrderbookLevel {
             price: Decimal::from_str(&level[0]).map_err(|e| anyhow!("Failed parse orderbook price '{}': {}", level[0], e))?,
             quantity: Decimal::from_str(&level[1]).map_err(|e| anyhow!("Failed parse orderbook quantity '{}': {}", level[1], e))?,
         })
     };

     let bids = book_data.bids.into_iter().map(parse_level).collect::<Result<Vec<_>, _>>()?;
     let asks = book_data.asks.into_iter().map(parse_level).collect::<Result<Vec<_>, _>>()?;

     Ok((book_data.symbol, bids, asks))
}

 pub(super) fn parse_public_trade_update(data: Value) -> Result<Option<(String, f64, f64, OrderSide, i64)>, anyhow::Error> { // Возвращает Option
     let trades_data: Vec<BybitWsTradeData> = serde_json::from_value(data)
         .map_err(|e| anyhow!("Failed to parse public trade data array: {}", e))?;
     if let Some(trade_data) = trades_data.into_iter().next() {
         let side = OrderSide::from_str(&trade_data.side)?;
         Ok(Some((
             trade_data.symbol,
             trade_data.price,
             trade_data.qty,
             side,
             trade_data.timestamp,
         )))
     } else {
         // Пустой массив - не ошибка, просто нет данных в этом сообщении
         warn!("Received empty data array for publicTrade topic");
         Ok(None)
     }
 }