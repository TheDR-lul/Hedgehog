// src/exchange/bybit_ws/read_loop.rs

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::{
    sync::mpsc,
    time::{interval, sleep, timeout},
};
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{debug, error, info, warn};
use std::time::SystemTime;

use crate::config::Config;
use crate::exchange::types::{SubscriptionType, WebSocketMessage};
use crate::exchange::bybit_ws::connection::connect_auth_and_subscribe_internal;
use crate::exchange::bybit_ws::protocol::handle_message;
use crate::exchange::bybit_ws::{WsStream, WsSink, READ_TIMEOUT_SECONDS};


pub(super) async fn read_loop(
    mut ws_reader: WsStream,
    mut ws_sender: WsSink,
    mpsc_sender: mpsc::Sender<Result<WebSocketMessage>>,
    config: Config,
    subscriptions: Vec<SubscriptionType>, // subscriptions передаются, но не используются напрямую в read_loop после коннекта
    base_ws_url: String,
    stream_description: String, // Для логирования (e.g., "Private", "Public SPOT", "Public LINEAR")
) {
    info!(stream = %stream_description, "WebSocket read_loop запущен.");
    let ping_interval_secs = config.ws_ping_interval_secs;
    let reconnect_delay_secs = config.ws_reconnect_delay_secs;

    'reconnect_loop: loop {

        if mpsc_sender.is_closed() {
            info!(stream = %stream_description, "MPSC канал закрыт извне, остановка read_loop.");
            break 'reconnect_loop;
        }

        let mut ping_timer = interval(Duration::from_secs(ping_interval_secs));
        let mut last_pong_received = Instant::now();
        // Увеличим таймаут с учетом возможной задержки на пинг + ответ
        let pong_timeout = Duration::from_secs(ping_interval_secs + READ_TIMEOUT_SECONDS); 

        info!(stream = %stream_description, "Вход во внутренний цикл обработки сообщений.");
        loop {
            tokio::select! {
                // Увеличим таймаут для чтения следующего сообщения, чтобы он был больше pong_timeout
                // Это основной таймаут на получение любого сообщения (включая понг)
                maybe_message_result = timeout(pong_timeout + Duration::from_secs(5), ws_reader.next()) => {
                    match maybe_message_result {
                        Ok(Some(Ok(message))) => { // Сообщение успешно прочитано из стрима
                            last_pong_received = Instant::now(); // Сбрасываем таймер понга при любом сообщении
                            if message.is_pong() {
                                 debug!(stream = %stream_description, "Получен Pong.");
                                 if mpsc_sender.send(Ok(WebSocketMessage::Pong)).await.is_err() {
                                     warn!(stream = %stream_description, "MPSC получатель сброшен при отправке Pong.");
                                     break 'reconnect_loop;
                                 }
                                 continue;
                            }
                            // Обработка остальных сообщений
                            if let Err(handle_error) = handle_message(message, &mpsc_sender).await {
                                warn!(stream = %stream_description, "Ошибка обработки WebSocket сообщения: {}", handle_error);
                                let is_fatal_error = handle_error.to_string().contains("WebSocket закрыт") ||
                                                     handle_error.to_string().contains("MPSC получатель сброшен");

                                // Отправляем ошибку в канал, только если это не ошибка "MPSC получатель сброшен"
                                // так как это означает, что отправлять уже некому.
                                if !handle_error.to_string().contains("MPSC получатель сброшен") {
                                    if mpsc_sender.send(Err(handle_error)).await.is_err() {
                                         warn!(stream = %stream_description, "MPSC получатель сброшен при отправке ошибки обработки.");
                                         break 'reconnect_loop; // Канал закрыт, выходим из всего
                                    }
                                } else if is_fatal_error && handle_error.to_string().contains("MPSC получатель сброшен") {
                                    // Если ошибка фатальная И это MPSC drop, то выходим из внешнего цикла
                                    break 'reconnect_loop;
                                }


                                 if is_fatal_error && !handle_error.to_string().contains("MPSC получатель сброшен") {
                                     info!(stream = %stream_description, "Разрыв внутреннего цикла из-за ошибки WebSocket.");
                                     break; // Выход из внутреннего цикла для реконнекта
                                 }
                            }
                        }
                        Ok(Some(Err(protocol_error))) => { // Ошибка протокола WebSocket (не ошибка парсинга)
                            error!(stream = %stream_description, "Ошибка протокола WebSocket: {}", protocol_error);
                            let _ = mpsc_sender.send(Err(anyhow!("Ошибка протокола WebSocket ({}) : {}", stream_description, protocol_error))).await;
                            break; // Выход из внутреннего цикла для реконнекта
                        }
                        Ok(None) => { // Стрим был закрыт удаленно (нормальное явление при реконнекте)
                            info!(stream = %stream_description, "WebSocket стрим закрыт удаленной стороной.");
                            let _ = mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await; // Уведомляем о дисконнекте
                            break; // Выход из внутреннего цикла для реконнекта
                        }
                         Err(_elapsed_error) => { // Таймаут чтения сообщения (ws_reader.next())
                            error!(stream = %stream_description, "Таймаут чтения WebSocket сообщения (>{:?}). Проверяем Pong.", pong_timeout + Duration::from_secs(5));
                            // Дополнительно проверяем, не пропустили ли мы понг
                            if last_pong_received.elapsed() > pong_timeout {
                                error!(stream = %stream_description, "Таймаут ответа Pong WebSocket ({:?} истекли). Попытка переподключения.", pong_timeout);
                                let _ = mpsc_sender.send(Err(anyhow!("Таймаут ответа Pong WebSocket ({})", stream_description))).await;
                                break; // Выход из внутреннего цикла для реконнекта
                            } else {
                                // Если понг был недавно, возможно, просто нет данных. Продолжаем ждать.
                                // Но если это повторяется, read_loop может зациклиться здесь без пингов.
                                // Пинг отправляется по своему таймеру.
                                warn!(stream = %stream_description, "Таймаут чтения сообщения, но понг был недавно. Продолжаем...");
                            }
                        }
                    }
                }
                _ = ping_timer.tick() => {
                    if ws_sender.is_closed() {
                        warn!(stream = %stream_description, "WebSocket sender закрыт, не могу отправить Ping.");
                        let _ = mpsc_sender.send(Err(anyhow!("WebSocket sender закрыт перед отправкой Ping ({})", stream_description))).await;
                        break; // Выход из внутреннего цикла для реконнекта
                    }
                    if last_pong_received.elapsed() > pong_timeout {
                         error!(stream = %stream_description, "Таймаут Pong WebSocket при проверке таймером Ping (elapsed: {:?}).", last_pong_received.elapsed());
                         let _ = mpsc_sender.send(Err(anyhow!("Таймаут Pong WebSocket при проверке Ping ({})", stream_description))).await;
                         break; // Выход из внутреннего цикла для реконнекта
                    }
                    debug!(stream = %stream_description, "Отправка WebSocket Ping");
                    let req_id = format!("ping_{}_{}", stream_description.to_lowercase().replace(" ", "_"), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
                    let ping_message = json!({"op": "ping", "req_id": req_id}).to_string();
                    if let Err(e) = ws_sender.send(Message::Text(ping_message.into())).await {
                        error!(stream = %stream_description, "Не удалось отправить WebSocket Ping: {}", e);
                        let _ = mpsc_sender.send(Err(anyhow!("Не удалось отправить WebSocket Ping ({}) : {}", stream_description, e))).await;
                        break; // Выход из внутреннего цикла для реконнекта
                    }
                }
                 else => {
                     if mpsc_sender.is_closed() {
                        info!(stream = %stream_description, "MPSC канал закрыт извне (блок else).");
                        break 'reconnect_loop;
                     }
                }
            }
        }

        info!(stream = %stream_description, "Внутренний цикл обработки сообщений завершен. Попытка переподключения через {} секунд...", reconnect_delay_secs);
        // Уведомляем о дисконнекте только если это не было сделано ранее (например, при Ok(None))
        // Но лучше всегда отправлять, а получатель разберется.
        if mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await.is_err() {
            info!(stream = %stream_description, "MPSC канал закрыт во время уведомления о дисконнекте, остановка попыток переподключения.");
            break 'reconnect_loop;
        }

        sleep(Duration::from_secs(reconnect_delay_secs)).await;

        // Клонируем subscriptions для передачи, так как они могут понадобиться снова
        match connect_auth_and_subscribe_internal(&base_ws_url, &config, &subscriptions, &stream_description).await {
            Ok((new_reader, new_sender)) => {
                info!(stream = %stream_description, "WebSocket успешно переподключен!");
                ws_reader = new_reader;
                ws_sender = new_sender;
                if mpsc_sender.send(Ok(WebSocketMessage::Connected)).await.is_err() {
                    warn!(stream = %stream_description, "MPSC получатель сброшен после успешного переподключения.");
                    break 'reconnect_loop;
                }
                // Не нужно continue 'reconnect_loop', так как внешний цикл сам это сделает
            }
            Err(e) => {
                 error!(stream = %stream_description, "Попытка переподключения WebSocket не удалась: {}. Повтор после задержки...", e);
                 // Пауза уже была перед попыткой, внешний цикл сам повторит
            }
        }
    }

    info!(stream = %stream_description, "WebSocket read_loop окончательно остановлен.");
}