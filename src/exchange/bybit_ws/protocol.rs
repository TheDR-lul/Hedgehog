// src/exchange/bybit_ws/protocol.rs

use crate::exchange::bybit_ws::types_internal::*; // Внутренние типы
use crate::exchange::types::WebSocketMessage; // Убрали OrderSide
use anyhow::{anyhow, Result, Context}; // Добавили Context
use futures_util::SinkExt; // Добавили SinkExt
use hmac::{Hmac, Mac};
use serde_json::json; // Value используется
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::protocol::Message; // Добавили Message
use tracing::{debug, error, info, trace, warn};
use crate::exchange::bybit_ws::WsSink; // Импорт типа WsSink
 // Используется хелперами
 // Используется хелперами
 // Используется хелперами

type HmacSha256 = Hmac<Sha256>;

// --- Вспомогательные функции парсинга строк ---
pub(super) mod str_or_empty_as_f64 {
    // Не изменяем, сигнатура и реализация верны (Result<f64, _>)
    use rust_decimal::prelude::{FromStr, ToPrimitive};
    use serde::{self, Deserialize, Deserializer};
    use rust_decimal::Decimal;
    pub fn deserialize<'de, D>(deserializer: D) -> Result<f64, D::Error> where D: Deserializer<'de> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() { Ok(0.0) }
        else {
            Decimal::from_str(&s).map_err(serde::de::Error::custom)?
                .to_f64().ok_or_else(|| serde::de::Error::custom("Failed to convert decimal to f64"))
        }
    }
}
pub(super) mod str_or_empty_as_f64_option {
     // Не изменяем, сигнатура и реализация верны (Result<Option<f64>, _>)
    use rust_decimal::prelude::{FromStr, ToPrimitive};
    use serde::{self, Deserialize, Deserializer};
    use rust_decimal::Decimal;
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error> where D: Deserializer<'de> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() {
             Ok(None)
        } else {
             Ok(Decimal::from_str(&s).ok().and_then(|d| d.to_f64()))
        }
    }
}
// --- Конец функций парсинга ---

// --- Функции протокола ---
fn get_expires() -> String { (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() + 5000).to_string() }
fn sign(api_secret: &str, payload: &str) -> String { let mut mac = HmacSha256::new_from_slice(api_secret.as_bytes()).expect("HMAC"); mac.update(payload.as_bytes()); hex::encode(mac.finalize().into_bytes()) }

pub(super) async fn authenticate(ws_sender: &mut WsSink, api_key: &str, api_secret: &str) -> Result<()> {
    let expires = get_expires();
    let signature = sign(api_secret, &format!("GET/realtime{}", expires));
    let msg = json!({"op": "auth", "args": [api_key, expires, signature]}).to_string();
    debug!("Sending auth: {}", msg);
    // Используем .into() для Message::Text и .context() для Result
    ws_sender.send(Message::Text(msg.into())).await.context("Send auth failed")
}

pub(super) async fn subscribe(ws_sender: &mut WsSink, args: Vec<String>) -> Result<()> {
    let msg = json!({"op": "subscribe", "args": args}).to_string();
    debug!("Sending subscribe: {}", msg);
    // Используем .into() для Message::Text и .context() для Result
    ws_sender.send(Message::Text(msg.into())).await.context("Send subscribe failed")
}

// Обработка одного сообщения WebSocket
pub(super) async fn handle_message(
    message: Message, // Используем импортированный тип
    mpsc_sender: &tokio::sync::mpsc::Sender<Result<WebSocketMessage>>,
) -> Result<()> {
    match message {
        Message::Text(text) => {
            trace!("Received WS Text: {}", text);
            match serde_json::from_str::<BybitWsResponse>(&text) {
                Ok(parsed_response) => {
                    match parse_bybit_response(parsed_response) {
                        Ok(Some(ws_message)) => {
                             if mpsc_sender.send(Ok(ws_message)).await.is_err() {
                                 warn!("MPSC receiver dropped while handling text message.");
                                 return Err(anyhow!("MPSC receiver dropped"));
                             }
                        }
                        Ok(None) => { trace!("Parsed message resulted in None (e.g. success ack), not sending."); }
                        Err(parse_err) => {
                             warn!("Failed to parse Bybit response content: {}. Raw: {}", parse_err, text);
                             if mpsc_sender.send(Err(parse_err)).await.is_err() { return Err(anyhow!("MPSC receiver dropped")); }
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to parse WebSocket JSON: {}. Raw: {}", e, text);
                     if mpsc_sender.send(Err(anyhow!("JSON parse error: {}", e))).await.is_err() {
                         return Err(anyhow!("MPSC receiver dropped"));
                     }
                }
            }
        }
        Message::Binary(data) => { warn!("Received unexpected WebSocket Binary data ({} bytes)", data.len()); }
        Message::Ping(data) => { debug!("Received WebSocket Ping: {:?} (auto-handled)", data); }
        Message::Pong(_) => { /* Обрабатывается в read_loop */ }
        Message::Close(close_frame) => {
            info!("Received WebSocket Close frame: {:?}", close_frame);
            // Отправляем Disconnected только если канал еще открыт
            if !mpsc_sender.is_closed() {
                let _ = mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await;
            }
            return Err(anyhow!("WebSocket closed by remote")); // Сигнал для read_loop о необходимости реконнекта
        }
        Message::Frame(frame) => { trace!("Received WebSocket Frame: {:?}", frame); } // Логируем для отладки, если нужно
    }
    Ok(())
}

// Парсинг ответа Bybit в наш WebSocketMessage
fn parse_bybit_response(response: BybitWsResponse) -> Result<Option<WebSocketMessage>> {
    trace!("Parsing Bybit WS Response: {:?}", response);
    if let Some(operation) = response.op {
        match operation.as_str() {
            "auth" => {
                let success = response.success.unwrap_or(false);
                if !success { warn!("WebSocket Authentication failed: {:?}", response.ret_msg); }
                Ok(Some(WebSocketMessage::Authenticated(success)))
            },
            "subscribe" => {
                let success = response.success.unwrap_or(false);
                let topic_or_req = response.req_id.or(response.topic).unwrap_or_default();
                if !success { warn!("WebSocket Subscription failed: {:?} for topic/req_id: {:?}", response.ret_msg, topic_or_req); }
                Ok(Some(WebSocketMessage::SubscriptionResponse { success, topic: topic_or_req }))
            },
            "ping" | "pong" => Ok(Some(WebSocketMessage::Pong)),
            _ => {
                warn!("Unknown operation received: {}", operation);
                Err(anyhow!("Unknown WS operation: {}", operation))
            }
        }
    } else if let Some(topic) = response.topic {
        let data = response.data.ok_or_else(|| anyhow!("Missing data field for topic {}", topic))?;
        let message_type = response.message_type.as_deref();

        if topic == "order" {
            // Вызываем функцию парсинга из types_internal
            crate::exchange::bybit_ws::types_internal::parse_order_update(data).map(WebSocketMessage::OrderUpdate).map(Some)
        } else if topic.starts_with("orderbook.") {
            // Вызываем функцию парсинга из types_internal
            crate::exchange::bybit_ws::types_internal::parse_orderbook_update(data).map(|(symbol, bids, asks)| WebSocketMessage::OrderBookL2 {
                symbol, bids, asks, is_snapshot: message_type == Some("snapshot")
            }).map(Some)
        } else if topic.starts_with("publicTrade.") {
            // Вызываем функцию парсинга из types_internal
            crate::exchange::bybit_ws::types_internal::parse_public_trade_update(data).map(|opt_data| opt_data.map(
                |(symbol, price, qty, side, timestamp)| WebSocketMessage::PublicTrade { symbol, price, qty, side, timestamp }
            ))
        } else {
            warn!("Unknown topic received: {}", topic);
            Err(anyhow!("Unknown WS topic: {}", topic))
        }
    } else if response.success == Some(false) && response.ret_msg.is_some() {
         let error_message = response.ret_msg.unwrap_or_else(|| "Unknown error".to_string());
         error!("Received WebSocket error message: {}", error_message);
         Ok(Some(WebSocketMessage::Error(error_message)))
    } else if response.success == Some(true) && response.op.is_none() && response.topic.is_none() {
         info!("Ignoring connection success message. ConnID: {:?}", response.conn_id);
         Ok(None) // Не отправляем это сообщение дальше
    } else {
        warn!("Unexpected WS message format: {:?}", response);
        Err(anyhow!("Unexpected WS message format"))
    }
}