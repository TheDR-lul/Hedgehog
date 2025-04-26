// src/exchange/bybit_ws.rs

use crate::{
    config::Config, // --- ДОБАВЛЕНО: Импорт Config ---
    exchange::types::{
        DetailedOrderStatus, OrderSide, OrderStatusText, SubscriptionType, WebSocketMessage,
        OrderbookLevel, // Добавили OrderbookLevel
    },
};
use anyhow::{anyhow, Context, Result};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use hmac::{Hmac, Mac};
use rust_decimal::Decimal; // --- ДОБАВЛЕНО: Импорт Decimal ---
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::{collections::HashMap, str::FromStr, time::{Duration, SystemTime, UNIX_EPOCH}}; // Добавили HashMap
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{interval, sleep, timeout},
};
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info, trace, warn};
use url::Url; // --- ДОБАВЛЕНО: Импорт Url ---

type HmacSha256 = Hmac<Sha256>;
type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

const CONNECT_TIMEOUT_SECONDS: u64 = 10;
// const AUTH_TIMEOUT_SECONDS: u64 = 5; // Пока не используем для ожидания ответа
// const SUBSCRIBE_TIMEOUT_SECONDS: u64 = 5; // Пока не используем для ожидания ответа
const READ_TIMEOUT_SECONDS: u64 = 60; // Общий таймаут чтения (включая пинг/понг)

// --- Структуры для парсинга сообщений Bybit WS ---
#[derive(Deserialize, Debug)]
struct BybitWsResponse {
    op: Option<String>,
    conn_id: Option<String>,
    req_id: Option<String>,
    success: Option<bool>,
    ret_msg: Option<String>,
    topic: Option<String>,
    #[serde(rename = "type")]
    message_type: Option<String>, // "snapshot", "delta"
    data: Option<Value>,
    ts: Option<i64>, // Timestamp сообщения от биржи (есть не во всех сообщениях)
    #[serde(rename = "creationTime")]
    _creation_time: Option<i64>, // Timestamp создания ws соединения
    #[serde(rename = "pong")]
    _pong_ts: Option<i64>, // Timestamp из ответа на наш ping
}

// Order Data (данные ордера в сообщении 'order')
#[derive(Deserialize, Debug, Clone)]
struct BybitWsOrderData {
    #[serde(rename = "orderId")]
    order_id: String,
    symbol: String,
    side: String,
    #[serde(rename = "orderStatus")]
    status: String,
    #[serde(rename = "cumExecQty", default, with = "str_or_empty_as_f64_option")]
    cum_exec_qty: Option<f64>,
    #[serde(rename = "cumExecValue", default, with = "str_or_empty_as_f64_option")]
    cum_exec_value: Option<f64>,
    #[serde(rename = "avgPrice", default, with = "str_or_empty_as_f64_option")]
    avg_price: Option<f64>,
    #[serde(rename = "leavesQty", default, with = "str_or_empty_as_f64_option")]
    leaves_qty: Option<f64>,
    #[serde(rename = "execQty", default, with = "str_or_empty_as_f64_option")]
    last_filled_qty: Option<f64>,
    #[serde(rename = "execPrice", default, with = "str_or_empty_as_f64_option")]
    last_filled_price: Option<f64>,
    #[serde(rename = "rejectReason")]
    reject_reason: Option<String>,
    // Добавить другие нужные поля при необходимости
}

// Trade Data (данные публичной сделки 'publicTrade')
#[derive(Deserialize, Debug, Clone)]
struct BybitWsTradeData {
    #[serde(rename = "T")] // Timestamp
    timestamp: i64,
    #[serde(rename = "s")] // Symbol
    symbol: String,
    #[serde(rename = "S")] // Side ("Buy" / "Sell")
    side: String,
    #[serde(rename = "v", with = "str_or_empty_as_f64")] // Volume / Qty
    qty: f64,
    #[serde(rename = "p", with = "str_or_empty_as_f64")] // Price
    price: f64,
    // "L": "PlusTick", // Tick direction
    // "i": "...", // Trade ID
    // "BT": false // Block Trade
}

// Orderbook Data (данные стакана 'orderbook')
#[derive(Deserialize, Debug, Clone)]
struct BybitWsOrderbookData {
    #[serde(rename = "s")] // Symbol
    symbol: String,
    #[serde(rename = "b")] // Bids [[price, qty], ...]
    bids: Vec<[String; 2]>,
    #[serde(rename = "a")] // Asks [[price, qty], ...]
    asks: Vec<[String; 2]>,
    #[serde(rename = "u")] // Update ID
    _update_id: i64,
    #[serde(rename = "seq", default)] // Sequence (для дельт)
    _sequence: Option<i64>,
}


// --- Вспомогательные функции для парсинга строк в f64 ---
mod str_or_empty_as_f64 {
    use rust_decimal::prelude::FromStr;
    use serde::{self, Deserialize, Deserializer};
    use rust_decimal::Decimal;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<f64, D::Error>
    where D: Deserializer<'de> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() { Ok(0.0) }
        else { Decimal::from_str(&s).map_err(serde::de::Error::custom)?.to_f64().ok_or_else(|| serde::de::Error::custom("Failed to convert decimal to f64"))}
    }
}
mod str_or_empty_as_f64_option {
     use rust_decimal::prelude::{FromStr, ToPrimitive}; // --- ДОБАВЛЕНО ToPrimitive ---
     use serde::{self, Deserialize, Deserializer};
     use rust_decimal::Decimal;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
    where D: Deserializer<'de> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() { Ok(None) }
         // --- ИСПРАВЛЕНО: Используем map и to_f64 ---
         else { Decimal::from_str(&s).ok().and_then(|d| d.to_f64()).map(Ok).unwrap_or(Ok(None)) }
    }
}
// --- Конец вспомогательных функций парсинга ---


// --- Основная функция ---

pub async fn connect_and_subscribe(
    config: &Config, // --- ИЗМЕНЕНО: Принимаем Config ---
    subscriptions: Vec<SubscriptionType>,
) -> Result<mpsc::Receiver<Result<WebSocketMessage>>> {
    // Определение URL на основе конфигурации
    let base_ws_url = if config.use_testnet {
        "wss://stream-testnet.bybit.com/v5/private" // Приватный поток для ордеров
                                                    // TODO: Нужен и публичный поток для стакана/сделок?
    } else {
        "wss://stream.bybit.com/v5/private"
    };

    info!(url = %base_ws_url, "Attempting to connect to Bybit WebSocket V5...");

    let url = Url::parse(base_ws_url)?;
    let connect_future = connect_async(url);

    // Таймаут подключения
    let (ws_stream, response) = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS), connect_future)
        .await
        .context("WebSocket connection timed out")?
        .context("WebSocket connection failed")?;

    info!(status = %response.status(), "WebSocket connection established.");

    let (mut ws_sender, ws_reader) = ws_stream.split();

    // Аутентификация
    authenticate(&mut ws_sender, &config.bybit_api_key, &config.bybit_api_secret).await?;
    info!("Authentication request sent.");
    // TODO: Ждать и проверить ответ Authenticated(true) в read_loop

    // Подписка
    let args: Vec<String> = subscriptions
        .iter()
        .map(|sub| match sub {
            SubscriptionType::Order => "order".to_string(),
            SubscriptionType::PublicTrade { symbol } => format!("publicTrade.{}", symbol),
            SubscriptionType::Orderbook { symbol, depth } => {
                format!("orderbook.{}.{}", depth, symbol)
            }
        })
        .collect();

    if !args.is_empty() {
        subscribe(&mut ws_sender, args).await?;
        info!("Subscription request sent.");
         // TODO: Ждать и проверить ответ SubscriptionResponse { success: true, ... } в read_loop
    } else {
        warn!("No subscriptions requested.");
    }

    // Канал MPSC
    let (mpsc_tx, mpsc_rx) = mpsc::channel::<Result<WebSocketMessage>>(100);

    // Запуск задачи чтения/пинга
    tokio::spawn(read_loop(
        ws_reader,
        ws_sender,
        mpsc_tx.clone(), // Клон для отправки сообщений
        config.ws_ping_interval_secs,
        config.ws_reconnect_delay_secs,
        // Передаем нужные параметры для возможного реконнекта
        config.bybit_api_key.clone(),
        config.bybit_api_secret.clone(),
        base_ws_url.to_string(),
        subscriptions, // Клонируем подписки для реконнекта
        config.clone() // Клонируем весь конфиг для реконнекта
    ));

    info!("WebSocket reader task spawned. Returning receiver channel.");
    Ok(mpsc_rx)
}

// --- Вспомогательные функции (Аутентификация, Подписка) ---
// (get_expires, sign, authenticate, subscribe - остаются как в предыдущем шаге)
fn get_expires() -> String {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        + 5000) // +5 секунд для времени жизни подписи
        .to_string()
}

fn sign(api_secret: &str, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(api_secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

async fn authenticate(ws_sender: &mut WsSink, api_key: &str, api_secret: &str) -> Result<()> {
    let expires = get_expires();
    let signature_payload = format!("GET/realtime{}", expires);
    let signature = sign(api_secret, &signature_payload);

    let auth_payload = json!({
        "op": "auth",
        "args": [api_key, expires, signature]
    });

    let message_text = serde_json::to_string(&auth_payload)?;
    debug!("Sending auth message: {}", message_text);
    ws_sender
        .send(Message::Text(message_text))
        .await
        .context("Failed to send authentication message")
}

async fn subscribe(ws_sender: &mut WsSink, args: Vec<String>) -> Result<()> {
    let sub_payload = json!({
        "op": "subscribe",
        "args": args
    });
    let message_text = serde_json::to_string(&sub_payload)?;
    debug!("Sending subscribe message: {}", message_text);
    ws_sender
        .send(Message::Text(message_text))
        .await
        .context("Failed to send subscription message")
}


// Основной цикл чтения сообщений и отправки пингов
async fn read_loop(
    mut ws_reader: WsStream,
    mut ws_sender: WsSink,
    mpsc_sender: mpsc::Sender<Result<WebSocketMessage>>,
    ping_interval_secs: u64,
    reconnect_delay_secs: u64,
    // Параметры для реконнекта
    api_key: String,
    api_secret: String,
    base_ws_url: String,
    subscriptions: Vec<SubscriptionType>,
    config: Config, // Весь конфиг для передачи в connect_and_subscribe
) {
    info!("WebSocket read_loop started.");
    let mut ping_timer = interval(Duration::from_secs(ping_interval_secs));
    let mut last_pong_received = std::time::Instant::now();
    let pong_timeout = Duration::from_secs(READ_TIMEOUT_SECONDS); // Используем READ_TIMEOUT

    loop {
        tokio::select! {
            // Ожидание сообщения или таймаута
            maybe_message = timeout(pong_timeout, ws_reader.next()) => { // Используем таймаут pong_timeout
                match maybe_message {
                    // Сообщение получено вовремя
                    Ok(Some(Ok(message))) => {
                        // Сбрасываем таймер таймаута при получении любого сообщения
                        last_pong_received = std::time::Instant::now();
                        // Обработка сообщения
                        if message.is_pong() {
                             debug!("Received WebSocket Pong");
                             // Отправляем Pong в канал для информации, если нужно
                             if mpsc_sender.send(Ok(WebSocketMessage::Pong)).await.is_err() {
                                 warn!("MPSC receiver dropped while sending Pong.");
                                 break; // Выход, если канал закрыт
                             }
                             continue; // Переходим к следующей итерации select!
                        }
                        // Обработка других сообщений
                        if let Err(handle_error) = handle_message(message, &mpsc_sender).await {
                            warn!("Error handling WebSocket message: {}", handle_error);
                             if mpsc_sender.send(Err(handle_error)).await.is_err() {
                                error!("Failed to send handle error to mpsc channel.");
                                break;
                             }
                             if handle_error.to_string().contains("WebSocket closed") {
                                 break; // Выход из цикла при закрытии
                             }
                        }
                    }
                    // Ошибка протокола
                    Ok(Some(Err(protocol_error))) => {
                        error!("WebSocket protocol error: {}", protocol_error);
                        let _ = mpsc_sender.send(Err(anyhow!("WebSocket protocol error: {}", protocol_error))).await;
                        break;
                    }
                    // Поток закрыт удаленно
                    Ok(None) => {
                        info!("WebSocket stream closed by remote.");
                        let _ = mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await;
                        break;
                    }
                     // Таймаут чтения Pong
                     Err(_) => {
                        error!("WebSocket Pong read timeout ({} seconds). Connection lost.", pong_timeout.as_secs());
                        let _ = mpsc_sender.send(Err(anyhow!("WebSocket Pong read timeout"))).await;
                        break; // Считаем соединение потерянным
                    }
                }
            }
            // Таймер для отправки Ping
            _ = ping_timer.tick() => {
                // Проверяем, не прошел ли таймаут с момента последнего Pong
                if last_pong_received.elapsed() > pong_timeout {
                     error!("WebSocket Pong timeout check failed (elapsed: {:?}). Connection lost.", last_pong_received.elapsed());
                     let _ = mpsc_sender.send(Err(anyhow!("WebSocket Pong timeout"))).await;
                     break; // Считаем соединение потерянным
                }

                debug!("Sending WebSocket Ping");
                let ping_message = json!({"op": "ping"}).to_string();
                if let Err(e) = ws_sender.send(Message::Text(ping_message)).await {
                    error!("Failed to send WebSocket Ping: {}", e);
                    let _ = mpsc_sender.send(Err(anyhow!("Failed to send WebSocket Ping: {}", e))).await;
                    break;
                }
            }
             // Проверка внешнего закрытия канала
             else => {
                 if mpsc_sender.is_closed() {
                     info!("MPSC channel closed externally, stopping read_loop.");
                     break;
                 }
             }
        }
    }
    info!("WebSocket read_loop finished. Attempting reconnect after {} seconds...", reconnect_delay_secs);
    // TODO: Добавить логику реконнекта с использованием переданных параметров
    // Например:
    sleep(Duration::from_secs(reconnect_delay_secs)).await;
    warn!("Reconnect logic not implemented yet!");
    // connect_and_subscribe(...).await; // Рекурсивный вызов или другая логика
}


// Обработка одного сообщения WebSocket
async fn handle_message(
    message: Message,
    mpsc_sender: &mpsc::Sender<Result<WebSocketMessage>>,
) -> Result<()> {
    match message {
        Message::Text(text) => {
            trace!("Received WebSocket Text: {}", text);
            match serde_json::from_str::<BybitWsResponse>(&text) {
                Ok(parsed_response) => {
                    let ws_message_result = parse_bybit_response(parsed_response);
                    // Отправляем результат парсинга (Ok или Err)
                    if mpsc_sender.send(ws_message_result).await.is_err() {
                        warn!("MPSC receiver dropped while handling text message.");
                        return Err(anyhow!("MPSC receiver dropped"));
                    }
                }
                Err(e) => {
                    warn!("Failed to parse WebSocket JSON: {}. Raw: {}", e, text);
                    // Отправляем ошибку парсинга
                     if mpsc_sender.send(Err(anyhow!("JSON parse error: {}", e))).await.is_err() {
                         warn!("MPSC receiver dropped while sending parse error.");
                         return Err(anyhow!("MPSC receiver dropped"));
                     }
                }
            }
        }
        Message::Binary(data) => {
            warn!("Received unexpected WebSocket Binary data ({} bytes)", data.len());
        }
        Message::Ping(data) => {
            debug!("Received WebSocket Ping: {:?}", data);
            // Pong отправляется автоматически библиотекой или сервером Bybit
        }
        Message::Pong(_) => {
            // Pong обрабатывается в read_loop для сброса таймера
            // Сюда он попасть не должен, т.к. message.is_pong() проверяется раньше
        }
        Message::Close(close_frame) => {
            info!("Received WebSocket Close frame: {:?}", close_frame);
            let _ = mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await;
            return Err(anyhow!("WebSocket closed by remote")); // Сигнализируем для выхода из цикла
        }
        Message::Frame(_) => {
            trace!("Received WebSocket Frame");
        }
    }
    Ok(())
}

// Парсинг ответа Bybit в наш WebSocketMessage
fn parse_bybit_response(response: BybitWsResponse) -> Result<WebSocketMessage> {
    trace!("Parsing Bybit WS Response: {:?}", response);
    if let Some(operation) = response.op {
        match operation.as_str() {
            "auth" => {
                let success = response.success.unwrap_or(false);
                 if !success { warn!("WebSocket Authentication failed: {:?}", response.ret_msg); }
                Ok(WebSocketMessage::Authenticated(success))
            },
            "subscribe" => {
                let success = response.success.unwrap_or(false);
                 if !success { warn!("WebSocket Subscription failed: {:?} for topic/req_id: {:?}", response.ret_msg, response.req_id); }
                Ok(WebSocketMessage::SubscriptionResponse {
                    success,
                    topic: response.req_id.or(response.topic).unwrap_or_default(),
                })
            },
            "ping" => Ok(WebSocketMessage::Pong), // Bybit отвечает на ping pong-ом с op: "pong", но обработаем и так
            "pong" => Ok(WebSocketMessage::Pong), // Наш пинг -> ответ понг
            _ => {
                warn!("Unknown operation received: {}", operation);
                Err(anyhow!("Unknown WS operation: {}", operation))
            }
        }
    } else if let Some(topic) = response.topic {
        let data = response.data.ok_or_else(|| anyhow!("Missing data field for topic {}", topic))?;
        let message_type = response.message_type.as_deref(); // "snapshot" или "delta"

        if topic == "order" {
            parse_order_update(data)
        } else if topic.starts_with("orderbook.") {
            parse_orderbook_update(data, message_type.unwrap_or("snapshot") == "snapshot") // is_snapshot
        } else if topic.starts_with("publicTrade.") {
            parse_public_trade_update(data)
        } else {
            warn!("Unknown topic received: {}", topic);
            Err(anyhow!("Unknown WS topic: {}", topic))
        }
    } else if response.success == Some(false) && response.ret_msg.is_some() {
         let error_message = response.ret_msg.unwrap_or_else(|| "Unknown error".to_string());
         error!("Received WebSocket error message: {}", error_message);
         Ok(WebSocketMessage::Error(error_message))
    }
    else {
        // Например, сообщение о подключении {"success":true,"ret_msg":"","conn_id":"...", "req_id":""}
        if response.success == Some(true) && response.op.is_none() && response.topic.is_none() {
            info!("Received connection success message. ConnID: {:?}", response.conn_id);
            // Можно не отправлять отдельное сообщение Connected, т.к. оно подразумевается
            // после успешного connect_and_subscribe
            Err(anyhow!("Ignoring connection success message")) // Возвращаем Err, чтобы не засорять канал
        } else {
            warn!("Received unexpected WebSocket message format: {:?}", response);
            Err(anyhow!("Unexpected WS message format"))
        }
    }
}

// --- Функции парсинга для конкретных топиков ---

fn parse_order_update(data: Value) -> Result<WebSocketMessage> {
    let orders_data: Vec<BybitWsOrderData> = serde_json::from_value(data)
        .map_err(|e| anyhow!("Failed to parse order data array: {}", e))?;
    if let Some(order_data) = orders_data.into_iter().next() {
        let status_text = OrderStatusText::from_str(&order_data.status)
            .unwrap_or_else(|_| OrderStatusText::Unknown(order_data.status.clone()));
        let side = OrderSide::from_str(&order_data.side)?;

        Ok(WebSocketMessage::OrderUpdate(DetailedOrderStatus {
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
        }))
    } else {
        Err(anyhow!("Received empty data array for order topic"))
    }
}

fn parse_orderbook_update(data: Value, is_snapshot: bool) -> Result<WebSocketMessage> {
     let book_data: BybitWsOrderbookData = serde_json::from_value(data)
         .map_err(|e| anyhow!("Failed to parse orderbook data: {}", e))?;

     let parse_level = |level: [String; 2]| -> Result<OrderbookLevel> {
         Ok(OrderbookLevel {
             price: Decimal::from_str(&level[0]).map_err(|e| anyhow!("Failed parse orderbook price '{}': {}", level[0], e))?,
             quantity: Decimal::from_str(&level[1]).map_err(|e| anyhow!("Failed parse orderbook quantity '{}': {}", level[1], e))?,
         })
     };

     let bids = book_data.bids.into_iter().map(parse_level).collect::<Result<Vec<_>>>()?;
     let asks = book_data.asks.into_iter().map(parse_level).collect::<Result<Vec<_>>>()?;

     Ok(WebSocketMessage::OrderBookL2 {
         symbol: book_data.symbol,
         bids,
         asks,
         is_snapshot,
     })
}

fn parse_public_trade_update(data: Value) -> Result<WebSocketMessage> {
     let trades_data: Vec<BybitWsTradeData> = serde_json::from_value(data)
         .map_err(|e| anyhow!("Failed to parse public trade data array: {}", e))?;
     // Часто приходит массив из одной сделки
     if let Some(trade_data) = trades_data.into_iter().next() {
         let side = OrderSide::from_str(&trade_data.side)?;
         Ok(WebSocketMessage::PublicTrade {
             symbol: trade_data.symbol,
             price: trade_data.price,
             qty: trade_data.qty,
             side,
             timestamp: trade_data.timestamp,
         })
     } else {
         // Если массив пуст, может быть норм, но лучше залогировать
         warn!("Received empty data array for publicTrade topic");
         Err(anyhow!("Received empty data array for publicTrade topic")) // Возвращаем ошибку, чтобы не отправлять пустое сообщение
     }
}