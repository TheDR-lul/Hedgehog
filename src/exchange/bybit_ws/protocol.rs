// src/exchange/bybit_ws/protocol.rs

use crate::exchange::bybit_ws::types_internal::*;
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

fn get_expires() -> String { (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() + 5000).to_string() }
fn sign(api_secret: &str, payload: &str) -> String { let mut mac = HmacSha256::new_from_slice(api_secret.as_bytes()).expect("HMAC"); mac.update(payload.as_bytes()); hex::encode(mac.finalize().into_bytes()) }

pub(super) async fn authenticate(ws_sender: &mut WsSink, api_key: &str, api_secret: &str) -> Result<String> {
    let expires = get_expires();
    let signature = sign(api_secret, &format!("GET/realtime{}", expires));
    let req_id = format!("auth_{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros());
    let msg = json!({"op": "auth", "args": [api_key, expires, signature], "req_id": req_id.clone()}).to_string();
    debug!(request_id = %req_id, "Отправка аутентификации");
    ws_sender.send(Message::Text(msg.into())).await.context("Отправка аутентификации не удалась")?;
    Ok(req_id)
}

pub(super) async fn subscribe(ws_sender: &mut WsSink, args: Vec<String>, stream_type: &str) -> Result<String> {
    let topics_part = args.join("_")
        .replace(['.',':','/'], "-")
        .chars().take(30).collect::<String>();

    let req_id = format!("subscribe_{}_{}_{}",
        stream_type.to_lowercase().replace(" ", "_").replace("/", "_"),
        topics_part,
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros()
    );
    let msg = json!({"op": "subscribe", "args": args, "req_id": req_id.clone()}).to_string();
    debug!(request_id = %req_id, "Отправка подписки (сообщение): {}", msg);
    ws_sender.send(Message::Text(msg.into())).await.context("Отправка подписки не удалась")?;
    Ok(req_id)
}

pub(super) async fn handle_message(
    message: Message,
    mpsc_sender: &tokio::sync::mpsc::Sender<Result<WebSocketMessage>>,
) -> Result<()> {
    match message {
        Message::Text(text) => {
            debug!("Raw WS Text Received: {}", text);
            match serde_json::from_str::<BybitWsResponse>(&text) {
                Ok(parsed_response) => {
                    trace!("Parsed BybitWsResponse: {:?}", parsed_response);
                    let log_ctx_from_resp = parsed_response.req_id.as_deref()
                        .or(parsed_response.topic.as_deref())
                        .unwrap_or("N/A_ctx_in_handle");
                    
                    match parse_bybit_response(parsed_response, log_ctx_from_resp) {
                        Ok(Some(ws_message)) => {
                            debug!(context = %log_ctx_from_resp, "Сгенерировано WebSocketMessage: {:?}", ws_message);
                            if mpsc_sender.send(Ok(ws_message)).await.is_err() {
                                 warn!(context = %log_ctx_from_resp, "MPSC получатель сброшен при обработке текстового сообщения.");
                                 return Err(anyhow!("MPSC получатель сброшен"));
                            }
                        }
                        Ok(None) => {
                            trace!(context = %log_ctx_from_resp, "parse_bybit_response вернул Ok(None), не отправляем в MPSC.");
                        }
                        Err(parse_err) => {
                             warn!(context = %log_ctx_from_resp, error = %parse_err, "parse_bybit_response не удался. Исходный текст: {}", text);
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
        Message::Ping(data) => { debug!("Получен WebSocket Ping от сервера: {:?}", data); } // Обычно сервер не шлет Ping, а отвечает Pong
        Message::Pong(data) => { 
            debug!("Получен WebSocket Pong от сервера: {:?}", data);
            // Pong обрабатывается в read_loop для сброса таймера, но можем также передать его дальше, если нужно
            if mpsc_sender.send(Ok(WebSocketMessage::Pong)).await.is_err() {
                warn!("MPSC получатель сброшен при отправке Pong сообщения.");
                return Err(anyhow!("MPSC получатель сброшен"));
            }
        }
        Message::Close(close_frame) => {
            info!("Получен WebSocket Close frame: {:?}", close_frame);
            if !mpsc_sender.is_closed() {
                let _ = mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await;
            }
            return Err(anyhow!("WebSocket закрыт удаленной стороной: {:?}", close_frame));
        }
        Message::Frame(frame) => { trace!("Получен WebSocket Frame: {:?}", frame); }
    }
    Ok(())
}

fn parse_bybit_response(response: BybitWsResponse, log_ctx: &str) -> Result<Option<WebSocketMessage>> {
    debug!(context = %log_ctx, "Внутри parse_bybit_response с: op={:?}, topic={:?}, req_id={:?}, success={:?}, ret_msg={:?}", 
           response.op, response.topic, response.req_id, response.success, response.ret_msg);

    if let Some(operation) = response.op {
        match operation.as_str() {
            "auth" => {
                let success = response.success.unwrap_or(false);
                let auth_req_id_from_response = response.req_id.as_deref().unwrap_or(log_ctx);
                if !success { warn!(context = %auth_req_id_from_response, "Аутентификация WebSocket не удалась: {:?}", response.ret_msg); }
                else { info!(context = %auth_req_id_from_response, "Аутентификация WebSocket успешна.");}
                Ok(Some(WebSocketMessage::Authenticated(success)))
            },
            "subscribe" => {
                let success = response.success.unwrap_or(false);
                let response_req_id = response.req_id.as_deref().unwrap_or_else(|| {
                    warn!(op_subscribe_context = %log_ctx, "Отсутствует 'req_id' в ответе Bybit на 'subscribe'. ret_msg: {:?}", response.ret_msg);
                    "" 
                });

                if !success { 
                    warn!(request_id = %response_req_id, "Подписка WebSocket не удалась: {:?}", response.ret_msg); 
                } else { 
                    info!(request_id = %response_req_id, "Подписка WebSocket успешна (ответ от биржи)."); 
                }
                // ВАЖНО: В поле 'topic' нашего сообщения передаем 'req_id' из ответа биржи.
                Ok(Some(WebSocketMessage::SubscriptionResponse { success, topic: response_req_id.to_string() }))
            },
            "ping" => {
                debug!(context = %log_ctx, "Получена операция ping от сервера: {:?}", response);
                // Серверный ping обычно не требует ответа pong от клиента, но мы можем его залогировать или обработать
                // Для простоты, пока не генерируем специальное сообщение, т.к. наш клиент сам шлет пинги.
                Ok(None) 
            }
            "pong" => { // Ответ на наш op: "ping"
                debug!(context = %log_ctx, request_id = response.req_id.as_deref().unwrap_or("N/A"), "Получен Pong от сервера (в ответ на наш Ping).");
                Ok(Some(WebSocketMessage::Pong))
            },
            _ => {
                warn!(context = %log_ctx, "Получена неизвестная операция: {}", operation);
                Err(anyhow!("Неизвестная WS операция: {}", operation))
            }
        }
    } else if let Some(topic_str) = response.topic.as_ref() {
        let data = response.data.ok_or_else(|| anyhow!("Отсутствует поле data для топика {}", topic_str))?;
        let message_type = response.message_type.as_deref();
        let event_ts = response.ts; // Используем общий ts из ответа

        if topic_str == "order" {
            debug!(context = %topic_str, "Попытка парсинга обновления ордера");
            crate::exchange::bybit_ws::types_internal::parse_order_update(data, event_ts).map(WebSocketMessage::OrderUpdate).map(Some)
        } else if topic_str.starts_with("orderbook.") {
            debug!(context = %topic_str, "Попытка парсинга обновления ордербука (type: {:?})", message_type);
            let result = crate::exchange::bybit_ws::types_internal::parse_orderbook_update(data, event_ts); // event_ts из BybitWsResponse
            match &result {
                Ok((symbol, bids, asks)) => debug!(context = %topic_str,
                                               "Успешно распарсен ордербук для {}. Биды: {}, Аски: {}, Тип: {:?}, TS: {:?}", 
                                               symbol, bids.len(), asks.len(), message_type, event_ts),
                Err(e) => warn!(context = %topic_str,
                                 "Не удалось распарсить обновление ордербука: {}", e),
            }
            result.map(|(symbol, bids, asks)| WebSocketMessage::OrderBookL2 {
                symbol, bids, asks, is_snapshot: message_type == Some("snapshot")
            }).map(Some)
        } else if topic_str.starts_with("publicTrade.") {
            debug!(context = %topic_str, "Попытка парсинга публичной сделки");
            crate::exchange::bybit_ws::types_internal::parse_public_trade_update(data).map(|opt_data| opt_data.map(
                |(symbol, price, qty, side, trade_ts)| WebSocketMessage::PublicTrade { symbol, price, qty, side, timestamp: trade_ts }
            ))
        } else {
            warn!(context = %log_ctx, "Получен неизвестный топик: {}", topic_str);
            Err(anyhow!("Неизвестный WS топик: {}", topic_str))
        }
    } else if response.success == Some(false) && response.ret_msg.is_some() {
         let error_message = response.ret_msg.unwrap_or_else(|| "Неизвестная ошибка".to_string());
         error!(context = %log_ctx, "Получено сообщение об ошибке WebSocket: {}", error_message);
         Ok(Some(WebSocketMessage::Error(error_message)))
    } else if response.success == Some(true) && response.op.is_none() && response.topic.is_none() {
         let conn_id_str = response.conn_id.as_deref().unwrap_or("N/A_conn");
         // Это может быть ответ на аутентификацию или Ping/Pong без явного "op" в некоторых случаях
         info!(context = %log_ctx, conn_id = %conn_id_str, "Получено общее сообщение об успехе без op/topic (вероятно, ответ на auth или pong). Игнорируется как отдельное событие здесь.");
         Ok(None)
    } else {
        warn!(context = %log_ctx, "Неожиданный формат WS сообщения (нет op и topic, но не явная ошибка): {:?}", response);
        Ok(None)
    }
}