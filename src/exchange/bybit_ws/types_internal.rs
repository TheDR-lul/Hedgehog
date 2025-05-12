// src/exchange/bybit_ws/types_internal.rs

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use tracing::{warn, trace, debug, error}; // Добавлены debug, error
use crate::exchange::types::{OrderSide, OrderStatusText, DetailedOrderStatus, OrderbookLevel};
use std::str::FromStr;
use anyhow::anyhow;

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsResponse {
    pub(super) op: Option<String>,
    pub(super) conn_id: Option<String>,
    #[serde(default)] pub(super) req_id: Option<String>, 
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
    pub(super) cts: Option<i64>, 
}

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsOrderData {
    #[serde(rename = "orderId")] pub(super) order_id: String,
    pub(super) symbol: String,
    pub(super) side: String, 
    #[serde(rename = "orderStatus")] pub(super) status: String, 
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) cum_exec_qty: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) cum_exec_value: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) avg_price: Option<f64>,
    #[serde(default, with = "super::protocol::str_or_empty_as_f64_option")] pub(super) leaves_qty: Option<f64>, 
    #[serde(default, rename = "lastExecQty", with = "super::protocol::str_or_empty_as_f64_option")] pub(super) last_filled_qty: Option<f64>,
    #[serde(default, rename = "lastExecPrice", with = "super::protocol::str_or_empty_as_f64_option")] pub(super) last_filled_price: Option<f64>,
    #[serde(rename = "rejectReason")] pub(super) reject_reason: Option<String>,
    #[serde(default, rename = "execQty", with = "super::protocol::str_or_empty_as_f64_option")] pub(super) _exec_qty_v5: Option<f64>, // Поле из V5, если отличается от last_exec_qty
    #[serde(default, rename = "execPrice", with = "super::protocol::str_or_empty_as_f64_option")] pub(super) _exec_price_v5: Option<f64>,// Поле из V5
    // createdTime и updatedTime обычно нужны для отслеживания, но не для DetailedOrderStatus напрямую
    // #[serde(rename = "createdTime")] pub(super) _created_time: Option<String>,
    // #[serde(rename = "updatedTime")] pub(super) _updated_time: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsTradeData {
    #[serde(rename = "T")] pub(super) timestamp: i64, 
    #[serde(rename = "s")] pub(super) symbol: String,
    #[serde(rename = "S")] pub(super) side: String, 
    #[serde(rename = "v", with = "super::protocol::str_or_empty_as_f64")] pub(super) qty: f64, 
    #[serde(rename = "p", with = "super::protocol::str_or_empty_as_f64")] pub(super) price: f64, 
}

#[derive(Deserialize, Debug, Clone)]
pub(super) struct BybitWsOrderbookData {
    #[serde(rename = "s")] pub(super) symbol: String, 
    #[serde(rename = "b")] pub(super) bids: Vec<[String; 2]>, 
    #[serde(rename = "a")] pub(super) asks: Vec<[String; 2]>, 
    #[serde(rename = "u")] pub(super) update_id: i64, 
    #[serde(default)] pub(super) seq: Option<i64>, 
    // ts (event time) и cts (cross time) теперь на верхнем уровне BybitWsResponse
}


pub(super) fn parse_order_update(data: Value, _event_ts: Option<i64>) -> Result<DetailedOrderStatus, anyhow::Error> {
    let orders_data_array: Vec<BybitWsOrderData> = serde_json::from_value(data.clone())
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
            last_filled_qty: order_data.last_filled_qty,
            last_filled_price: order_data.last_filled_price,
            reject_reason: order_data.reject_reason,
        })
    } else {
        Err(anyhow!("Получен пустой массив данных для топика ордера. Data: {:?}", data))
    }
}

pub(super) fn parse_orderbook_update(data: Value, event_ts: Option<i64>) -> Result<(String, Vec<OrderbookLevel>, Vec<OrderbookLevel>), anyhow::Error> {
    debug!("parse_orderbook_update: attempting to parse data: {:?}", data); 
    let book_data: BybitWsOrderbookData = serde_json::from_value(data.clone())
        .map_err(|e| {
            error!("parse_orderbook_update: ОШИБКА ДЕСЕРИАЛИЗАЦИИ BybitWsOrderbookData: {}. Data: {:?}", e, data);
            anyhow!("Не удалось распарсить BybitWsOrderbookData: {}. Data: {:?}", e, data)
        })?;
    trace!("parse_orderbook_update: parsed book_data: {:?}, event_ts: {:?}", book_data, event_ts);

    let parse_level = |level_str_array: [String; 2]| -> Result<OrderbookLevel, anyhow::Error> {
        trace!("parse_level: parsing: price='{}', quantity='{}'", &level_str_array[0], &level_str_array[1]);
        let price_str = &level_str_array[0];
        let quantity_str = &level_str_array[1];

        if price_str.is_empty() || quantity_str.is_empty() {
            warn!("parse_level: Пустая строка для цены ('{}') или количества ('{}') в уровне ордербука.", price_str, quantity_str);
            return Err(anyhow!("Пустая строка для цены или количества в уровне ордербука."));
        }

        let price = Decimal::from_str(price_str)
            .map_err(|e| {
                error!("parse_level: НЕ УДАЛОСЬ распарсить ЦЕНУ ордербука '{}': {}", price_str, e);
                anyhow!("Не удалось распарсить ЦЕНУ ордербука '{}': {}", price_str, e)
            })?;
        let quantity = Decimal::from_str(quantity_str)
            .map_err(|e| {
                error!("parse_level: НЕ УДАЛОСЬ распарсить КОЛИЧЕСТВО ордербука '{}': {}", quantity_str, e);
                anyhow!("Не удалось распарсить КОЛИЧЕСТВО ордербука '{}': {}", quantity_str, e)
            })?;
        
        if quantity < Decimal::ZERO {
            warn!("Получено отрицательное количество в уровне ордербука: [{}, {}]", price, quantity);
        }
        trace!("parse_level: parsed price={}, quantity={}", price, quantity);
        Ok(OrderbookLevel { price, quantity })
    };

    let bids = book_data.bids.into_iter()
        .map(parse_level)
        .filter_map(Result::ok) // Собираем только успешно распарсенные, логируем ошибки выше
        .collect::<Vec<_>>();

    let asks = book_data.asks.into_iter()
        .map(parse_level)
        .filter_map(Result::ok) // Собираем только успешно распарсенные
        .collect::<Vec<_>>();
    
    if bids.is_empty() && asks.is_empty() {
        // Это может быть нормально для дельт, но для снапшота - подозрительно, если не было ошибок парсинга отдельных уровней
        warn!("parse_orderbook_update: symbol={}, ПУСТЫЕ bids И asks после парсинга! event_ts={:?}", book_data.symbol, event_ts);
    } else {
        debug!("parse_orderbook_update: symbol={}, bids_count={}, asks_count={}, event_ts={:?}", book_data.symbol, bids.len(), asks.len(), event_ts);
    }
    Ok((book_data.symbol, bids, asks))
}

 pub(super) fn parse_public_trade_update(data: Value) -> Result<Option<(String, f64, f64, OrderSide, i64)>, anyhow::Error> {
     let trades_data_array: Vec<BybitWsTradeData> = serde_json::from_value(data.clone()) 
         .map_err(|e| anyhow!("Не удалось распарсить массив данных публичных сделок: {}. Data: {:?}", e, data))?;
     
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