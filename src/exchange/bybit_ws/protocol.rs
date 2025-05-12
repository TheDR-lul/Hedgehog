// src/exchange/bybit_ws/protocol.rs

use crate::exchange::bybit_ws::types_internal::*; // Внутренние типы
use crate::exchange::types::WebSocketMessage;
use anyhow::{anyhow, Result, Context};
use futures_util::SinkExt;
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{debug, error, info, trace, warn};
use crate::exchange::bybit_ws::WsSink;

type HmacSha256 = Hmac<Sha256>;

// --- Вспомогательные функции парсинга строк ---
pub(super) mod str_or_empty_as_f64 {
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
    let req_id = format!("auth_{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
    let msg = json!({"op": "auth", "args": [api_key, expires, signature], "req_id": req_id}).to_string();
    debug!(request_id = %req_id, "Отправка аутентификации: {}", msg);
    ws_sender.send(Message::Text(msg.into())).await.context("Отправка аутентификации не удалась")
}

pub(super) async fn subscribe(ws_sender: &mut WsSink, args: Vec<String>, stream_type: &str) -> Result<()> {
    let req_id = format!("subscribe_{}_{}", stream_type, SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
    let msg = json!({"op": "subscribe", "args": args, "req_id": req_id}).to_string();
    debug!(request_id = %req_id, "Отправка подписки: {}", msg);
    ws_sender.send(Message::Text(msg.into())).await.context("Отправка подписки не удалась")
}

// Обработка одного сообщения WebSocket
pub(super) async fn handle_message(
    message: Message,
    mpsc_sender: &tokio::sync::mpsc::Sender<Result<WebSocketMessage>>,
) -> Result<()> {
    match message {
        Message::Text(text) => {
            // Логируем ПОЛНЫЙ текст сообщения ПЕРЕД парсингом
            debug!("Raw WS Text Received: {}", text); // <--- ДОБАВЛЕН ЭТОТ ЛОГ

            match serde_json::from_str::<BybitWsResponse>(&text) {
                Ok(mut parsed_response) => { // Сделали parsed_response мутабельным
                    // Логируем распарсенный BybitWsResponse
                    trace!("Parsed BybitWsResponse: {:?}", parsed_response);
                    
                    // Добавляем req_id в parsed_response если его нет, для контекста в parse_bybit_response
                    // Это не идеальное решение, но поможет для логов внутри parse_bybit_response
                    if parsed_response.req_id.is_none() && parsed_response.topic.is_some() {
                         parsed_response.req_id = parsed_response.topic.clone(); // Используем topic как req_id для логов
                    }


                    match parse_bybit_response(parsed_response) { // передаем parsed_response по значению
                        Ok(Some(ws_message)) => {
                            debug!("Сгенерировано WebSocketMessage: {:?}", ws_message);
                            if mpsc_sender.send(Ok(ws_message)).await.is_err() {
                                 warn!("MPSC получатель сброшен при обработке текстового сообщения.");
                                 return Err(anyhow!("MPSC получатель сброшен"));
                            }
                        }
                        Ok(None) => {
                            trace!("parse_bybit_response вернул Ok(None), не отправляем в MPSC.");
                        }
                        Err(parse_err) => {
                             warn!("parse_bybit_response не удался: {}. Исходный текст: {}", parse_err, text);
                             if mpsc_sender.send(Err(parse_err)).await.is_err() { return Err(anyhow!("MPSC получатель сброшен")); }
                        }
                    }
                }
                Err(e) => {
                    warn!("Не удалось распарсить WebSocket JSON: {}. Исходный текст: {}", e, text);
                     if mpsc_sender.send(Err(anyhow!("Ошибка парсинга JSON: {}", e))).await.is_err() {
                         return Err(anyhow!("MPSC получатель сброшен"));
                     }
                }
            }
        }
        Message::Binary(data) => { warn!("Получены неожиданные бинарные WebSocket данные ({} байт)", data.len()); }
        Message::Ping(data) => { debug!("Получен WebSocket Ping: {:?} (автоматически обработан)", data); }
        Message::Pong(_) => { /* Обрабатывается в read_loop */ }
        Message::Close(close_frame) => {
            info!("Получен WebSocket Close frame: {:?}", close_frame);
            if !mpsc_sender.is_closed() {
                let _ = mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await;
            }
            return Err(anyhow!("WebSocket закрыт удаленной стороной"));
        }
        Message::Frame(frame) => { trace!("Получен WebSocket Frame: {:?}", frame); }
    }
    Ok(())
}

// Парсинг ответа Bybit в наш WebSocketMessage
fn parse_bybit_response(response: BybitWsResponse) -> Result<Option<WebSocketMessage>> {
    // Используем req_id или topic для контекста в логах
    let log_ctx = response.req_id.as_deref().or(response.topic.as_deref()).unwrap_or("N/A_ctx");
    debug!(context = %log_ctx, "Внутри parse_bybit_response с: {:?}", response);

    if let Some(operation) = response.op {
        match operation.as_str() {
            "auth" => {
                let success = response.success.unwrap_or(false);
                if !success { warn!(context = %log_ctx, "Аутентификация WebSocket не удалась: {:?}", response.ret_msg); }
                else { info!(context = %log_ctx, "Аутентификация WebSocket успешна.");}
                Ok(Some(WebSocketMessage::Authenticated(success)))
            },
            "subscribe" => {
                let success = response.success.unwrap_or(false);
                let topic_or_req_display = response.req_id.as_deref().unwrap_or_else(|| response.topic.as_deref().unwrap_or("N/A_topic"));
                if !success { warn!(context = %log_ctx, "Подписка WebSocket не удалась: {:?} для topic/req_id: {}", response.ret_msg, topic_or_req_display); }
                else { info!(context = %log_ctx, "Подписка WebSocket успешна для topic/req_id: {}", topic_or_req_display); }
                Ok(Some(WebSocketMessage::SubscriptionResponse { success, topic: topic_or_req_display.to_string() }))
            },
            "ping" | "pong" => {
                debug!(context = %log_ctx, "Получен ping/pong operation: {:?}", operation);
                Ok(Some(WebSocketMessage::Pong))
            },
            _ => {
                warn!(context = %log_ctx, "Получена неизвестная операция: {}", operation);
                Err(anyhow!("Неизвестная WS операция: {}", operation))
            }
        }
    } else if let Some(topic) = response.topic.as_ref() { // Берем topic по ссылке
        let data = response.data.ok_or_else(|| anyhow!("Отсутствует поле data для топика {}", topic))?;
        let message_type = response.message_type.as_deref();
        let ts = response.ts; // Используем ts из BybitWsResponse

        if topic == "order" {
            debug!(context = %log_ctx, "Попытка парсинга обновления ордера для топика: {}", topic);
            crate::exchange::bybit_ws::types_internal::parse_order_update(data, ts).map(WebSocketMessage::OrderUpdate).map(Some)
        } else if topic.starts_with("orderbook.") {
            debug!(context = %log_ctx, "Попытка парсинга обновления ордербука для топика: {}", topic);
            let result = crate::exchange::bybit_ws::types_internal::parse_orderbook_update(data, ts);
            match &result {
                Ok((symbol, bids, asks)) => debug!(context = %log_ctx,
                                               "Успешно распарсен ордербук для {}. Биды: {}, Аски: {}", symbol, bids.len(), asks.len()),
                Err(e) => warn!(context = %log_ctx,
                                 "Не удалось распарсить обновление ордербука для {}: {}", topic, e),
            }
            result.map(|(symbol, bids, asks)| WebSocketMessage::OrderBookL2 {
                symbol, bids, asks, is_snapshot: message_type == Some("snapshot")
            }).map(Some)
        } else if topic.starts_with("publicTrade.") {
            debug!(context = %log_ctx, "Попытка парсинга публичной сделки для топика: {}", topic);
            crate::exchange::bybit_ws::types_internal::parse_public_trade_update(data).map(|opt_data| opt_data.map(
                |(symbol, price, qty, side, trade_ts)| WebSocketMessage::PublicTrade { symbol, price, qty, side, timestamp: trade_ts }
            ))
        } else {
            warn!(context = %log_ctx, "Получен неизвестный топик: {}", topic);
            Err(anyhow!("Неизвестный WS топик: {}", topic))
        }
    } else if response.success == Some(false) && response.ret_msg.is_some() {
         let error_message = response.ret_msg.unwrap_or_else(|| "Неизвестная ошибка".to_string());
         error!(context = %log_ctx, "Получено сообщение об ошибке WebSocket: {}", error_message);
         Ok(Some(WebSocketMessage::Error(error_message)))
    } else if response.success == Some(true) && response.op.is_none() && response.topic.is_none() {
         // Это может быть ответ на аутентификацию или подписку без явного op в некоторых случаях (хотя req_id должен быть)
         info!(context = %log_ctx, "Получено общее сообщение об успехе без op/topic (conn_id: {:?}). Игнорируется.", response.conn_id);
         Ok(None)
    } else {
        warn!(context = %log_ctx, "Неожиданный формат WS сообщения: {:?}", response);
        Err(anyhow!("Неожиданный формат WS сообщения"))
    }
}