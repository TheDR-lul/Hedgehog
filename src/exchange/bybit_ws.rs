// src/exchange/bybit_ws.rs

use crate::exchange::types::{DetailedOrderStatus, OrderSide, OrderStatusText, SubscriptionType, WebSocketMessage};
use anyhow::{anyhow, Context, Result};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::{str::FromStr, time::{Duration, SystemTime, UNIX_EPOCH}};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{interval, sleep, timeout},
};
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info, trace, warn};

type HmacSha256 = Hmac<Sha256>;
type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

const CONNECT_TIMEOUT_SECONDS: u64 = 10;
const AUTH_TIMEOUT_SECONDS: u64 = 5;
const SUBSCRIBE_TIMEOUT_SECONDS: u64 = 5;
const READ_TIMEOUT_SECONDS: u64 = 60; // Таймаут чтения сообщения (включая пинг/понг)
const PING_INTERVAL_SECONDS: u64 = 20; // Интервал отправки Ping по ТЗ

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
    data: Option<Value>,           // Данные могут иметь разную структуру
    ts: Option<i64>,               // Timestamp сообщения от биржи
}

// Структура для данных ордера из WS (может отличаться от REST)
#[derive(Deserialize, Debug, Clone)]
struct BybitWsOrderData {
    #[serde(rename = "orderId")]
    order_id: String,
    symbol: String,
    side: String, // "Buy" или "Sell"
    #[serde(rename = "orderStatus")]
    status: String, // "New", "Filled", etc.
    #[serde(rename = "cumExecQty")]
    cum_exec_qty: String,
    #[serde(rename = "cumExecValue")]
    cum_exec_value: String,
    #[serde(rename = "avgPrice")]
    avg_price: String,
    #[serde(rename = "leavesQty")]
    leaves_qty: String,
    #[serde(rename = "execQty")]
    last_filled_qty: Option<String>, // Объем последнего исполнения
    #[serde(rename = "execPrice")]
    last_filled_price: Option<String>, // Цена последнего исполнения
    #[serde(rename = "rejectReason")]
    reject_reason: Option<String>, // Причина отклонения
                                   // Добавить другие нужные поля
}

// --- Основная функция ---

pub async fn connect_and_subscribe(
    api_key: &str,
    api_secret: &str,
    base_ws_url: &str, // e.g., "wss://stream.bybit.com/v5/private"
    subscriptions: Vec<SubscriptionType>,
    ping_interval: u64, // Из Config.ws_ping_interval_secs
                       // TODO: Добавить параметры для реконнекта из Config
) -> Result<mpsc::Receiver<Result<WebSocketMessage>>> {
    info!(url = %base_ws_url, "Attempting to connect to Bybit WebSocket V5...");

    let url = url::Url::parse(base_ws_url)?;
    let connect_future = connect_async(url);

    let (ws_stream, response) = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS), connect_future)
        .await
        .context("WebSocket connection timed out")? // Ошибка если таймаут
        .context("WebSocket connection failed")?; // Ошибка если connect_async вернул Err

    info!(status = %response.status(), "WebSocket connection established.");

    let (mut ws_sender, ws_reader) = ws_stream.split();

    // Аутентификация (только для приватных потоков, base_ws_url должен быть приватным)
    if base_ws_url.contains("private") {
        authenticate(&mut ws_sender, api_key, api_secret).await?;
        // TODO: Нужна проверка ответа на аутентификацию из потока чтения
        // Пока предполагаем успех, но это нужно будет добавить в цикле чтения
        info!("Authentication request sent.");
        // Небольшая пауза для обработки аутентификации сервером
        sleep(Duration::from_millis(500)).await;
    } else {
        info!("Skipping authentication for public WebSocket stream.");
    }

    // Формирование аргументов для подписки
    let args = subscriptions
        .iter()
        .map(|sub| match sub {
            SubscriptionType::Order => "order".to_string(),
            SubscriptionType::PublicTrade { symbol } => format!("publicTrade.{}", symbol),
            SubscriptionType::Orderbook { symbol, depth } => {
                format!("orderbook.{}.{}", depth, symbol)
            }
        })
        .collect::<Vec<String>>();

    if !args.is_empty() {
        subscribe(&mut ws_sender, args).await?;
        // TODO: Проверка ответа на подписку из потока чтения
        info!("Subscription request sent.");
    } else {
        warn!("No subscriptions requested.");
    }

    // Канал для отправки сообщений наружу
    let (sender, receiver) = mpsc::channel::<Result<WebSocketMessage>>(100);

    // Запуск задачи для чтения сообщений и пингов
    tokio::spawn(read_loop(
        ws_reader,
        ws_sender, // Передаем sender для пингов
        sender.clone(), // Клон для отправки сообщений
        ping_interval,
        // TODO: Передать параметры реконнекта
    ));

    info!("WebSocket reader task spawned. Returning receiver channel.");
    Ok(receiver)
}

// --- Вспомогательные функции ---

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
    mut ws_sender: WsSink, // Для отправки пингов
    mpsc_sender: mpsc::Sender<Result<WebSocketMessage>>,
    ping_interval_secs: u64,
    // TODO: Параметры реконнекта
) {
    info!("WebSocket read_loop started.");
    let mut ping_timer = interval(Duration::from_secs(ping_interval_secs));

    loop {
        tokio::select! {
            // Чтение сообщения из WebSocket
            maybe_message = timeout(Duration::from_secs(READ_TIMEOUT_SECONDS), ws_reader.next()) => {
                match maybe_message {
                    Ok(Some(Ok(message))) => {
                        // Обработка сообщения
                        if let Err(parse_error) = handle_message(message, &mpsc_sender).await {
                            warn!("Error handling WebSocket message: {}", parse_error);
                            // Отправляем ошибку парсинга, если канал еще жив
                             if !mpsc_sender.is_closed() {
                                 if let Err(e) = mpsc_sender.send(Err(parse_error)).await {
                                    error!("Failed to send parse error to mpsc channel: {}", e);
                                    break; // Выход из цикла, если канал закрыт
                                 }
                             } else {
                                 info!("MPSC channel closed, exiting read_loop.");
                                 break;
                             }
                        }
                    }
                    Ok(Some(Err(protocol_error))) => {
                        error!("WebSocket protocol error: {}", protocol_error);
                         if !mpsc_sender.is_closed() {
                             let _ = mpsc_sender.send(Err(anyhow!("WebSocket protocol error: {}", protocol_error))).await;
                         }
                        break; // Ошибка протокола обычно фатальна для соединения
                    }
                    Ok(None) => {
                        info!("WebSocket stream closed by remote.");
                         if !mpsc_sender.is_closed() {
                             let _ = mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await;
                         }
                        break; // Поток закрыт
                    }
                     Err(_) => { // Таймаут чтения
                        warn!("WebSocket read timeout ({} seconds). Possible connection issue.", READ_TIMEOUT_SECONDS);
                        // TODO: Попытаться отправить пинг или инициировать реконнект
                         if !mpsc_sender.is_closed() {
                             let _ = mpsc_sender.send(Err(anyhow!("WebSocket read timeout"))).await;
                         }
                        // Пока просто выходим, но нужна логика реконнекта
                        break;
                    }
                }
            }
            // Таймер для отправки Ping
            _ = ping_timer.tick() => {
                debug!("Sending WebSocket Ping");
                let ping_message = json!({"op": "ping"}).to_string();
                if let Err(e) = ws_sender.send(Message::Text(ping_message)).await {
                    error!("Failed to send WebSocket Ping: {}", e);
                     if !mpsc_sender.is_closed() {
                         let _ = mpsc_sender.send(Err(anyhow!("Failed to send WebSocket Ping: {}", e))).await;
                     }
                    break; // Ошибка отправки пинга = проблема с соединением
                }
            }
            // Проверка, закрыт ли канал MPSC снаружи
             else => { // else ветка нужна для select! макроса
                 if mpsc_sender.is_closed() {
                     info!("MPSC channel closed externally, stopping read_loop.");
                     break;
                 }
            }
        }
    }
    info!("WebSocket read_loop finished.");
    // TODO: Добавить логику реконнекта здесь, если необходимо
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
                    // Определяем тип сообщения и отправляем в канал
                    let ws_message_result = parse_bybit_response(parsed_response);
                    if let Ok(ref ws_msg) = ws_message_result {
                        // Логируем успешно распарсенные сообщения для отладки
                        debug!("Parsed WebSocket message: {:?}", ws_msg);
                    }
                    if mpsc_sender.send(ws_message_result).await.is_err() {
                        warn!("MPSC receiver dropped while handling text message.");
                        return Err(anyhow!("MPSC receiver dropped")); // Сигнализируем об ошибке для выхода из цикла
                    }
                }
                Err(e) => {
                    warn!("Failed to parse WebSocket JSON: {}. Raw: {}", e, text);
                    // Можно отправить ошибку парсинга или просто проигнорировать
                    // return Err(anyhow!("JSON parse error: {}", e));
                }
            }
        }
        Message::Binary(data) => {
            warn!("Received unexpected WebSocket Binary data ({} bytes)", data.len());
        }
        Message::Ping(data) => {
            // Автоматически отвечаем Pong через библиотеку, но можно и вручную
            debug!("Received WebSocket Ping: {:?}", data);
        }
        Message::Pong(data) => {
            // Получили ответ на наш Ping
            debug!("Received WebSocket Pong: {:?}", data);
        }
        Message::Close(close_frame) => {
            info!("Received WebSocket Close frame: {:?}", close_frame);
             if !mpsc_sender.is_closed() {
                let _ = mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await;
             }
            return Err(anyhow!("WebSocket closed")); // Сигнализируем для выхода из цикла
        }
        Message::Frame(_) => {
            // Обычно не используется напрямую
            trace!("Received WebSocket Frame");
        }
    }
    Ok(())
}

// Парсинг ответа Bybit в наш WebSocketMessage
fn parse_bybit_response(response: BybitWsResponse) -> Result<WebSocketMessage> {
    if let Some(operation) = response.op {
        match operation.as_str() {
            "auth" => Ok(WebSocketMessage::Authenticated(
                response.success.unwrap_or(false),
            )),
            "subscribe" => Ok(WebSocketMessage::SubscriptionResponse {
                success: response.success.unwrap_or(false),
                topic: response.req_id.or(response.topic).unwrap_or_default(), // req_id используется в запросе
            }),
            "ping" => {
                // Мы сами отправляем пинг, так что входящий пинг - странно, но обработаем
                warn!("Received ping operation response? {:?}", response);
                Ok(WebSocketMessage::Pong) // Просто считаем за понг
            }
            "pong" => Ok(WebSocketMessage::Pong),
            _ => {
                warn!("Unknown operation received: {}", operation);
                Err(anyhow!("Unknown WS operation: {}", operation))
            }
        }
    } else if let Some(topic) = response.topic {
        // Это обновление данных по подписке
        let data = response.data.ok_or_else(|| anyhow!("Missing data field for topic {}", topic))?;

        if topic == "order" {
            // Парсим массив ордеров
             let orders_data: Vec<BybitWsOrderData> = serde_json::from_value(data)?;
             // Обычно в обновлении ордера приходит один элемент
             if let Some(order_data) = orders_data.into_iter().next() {
                 // Конвертируем в наш DetailedOrderStatus
                 let status_text = OrderStatusText::from_str(&order_data.status).unwrap_or_else(|_| OrderStatusText::Unknown(order_data.status));
                 let filled_qty = order_data.cum_exec_qty.parse().unwrap_or(0.0);
                 let remaining_qty = order_data.leaves_qty.parse().unwrap_or(0.0);
                 let avg_price = order_data.avg_price.parse().unwrap_or(0.0);
                 let cum_value = order_data.cum_exec_value.parse().unwrap_or(0.0);
                 let last_filled_qty = order_data.last_filled_qty.and_then(|s| s.parse().ok());
                 let last_filled_price = order_data.last_filled_price.and_then(|s| s.parse().ok());
                 let side = OrderSide::from_str(&order_data.side)?; // Используем FromStr

                 Ok(WebSocketMessage::OrderUpdate(DetailedOrderStatus {
                    order_id: order_data.order_id,
                    symbol: order_data.symbol,
                    side,
                    filled_qty,
                    remaining_qty,
                    cumulative_executed_value: cum_value,
                    average_price: avg_price,
                    status_text,
                    last_filled_qty,
                    last_filled_price,
                    reject_reason: order_data.reject_reason,
                 }))
             } else {
                 Err(anyhow!("Received empty data array for order topic"))
             }
        } else if topic.starts_with("orderbook.") {
            // TODO: Реализовать парсинг стакана (orderbook)
            warn!("Orderbook parsing not implemented yet. Topic: {}", topic);
             Err(anyhow!("Orderbook parsing not implemented"))
        } else if topic.starts_with("publicTrade.") {
            // TODO: Реализовать парсинг публичных сделок (publicTrade)
             warn!("Public trade parsing not implemented yet. Topic: {}", topic);
             Err(anyhow!("Public trade parsing not implemented"))
        } else {
            warn!("Unknown topic received: {}", topic);
             Err(anyhow!("Unknown WS topic: {}", topic))
        }
    } else if response.ret_msg.is_some() && response.success == Some(false) {
        // Обработка сообщений об ошибках без 'op'
        let error_message = response.ret_msg.unwrap_or_else(|| "Unknown error".to_string());
        error!("Received WebSocket error message: {}", error_message);
        Ok(WebSocketMessage::Error(error_message)) // Отправляем как наше сообщение об ошибке
    }
    else {
        warn!("Received unexpected WebSocket message format: {:?}", response);
        Err(anyhow!("Unexpected WS message format"))
    }
}