// src/exchange/bybit_ws/read_loop.rs

use anyhow::{anyhow, Result, Context}; // Добавил Context
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH}; // ИСПРАВЛЕНО: Добавлены SystemTime, UNIX_EPOCH
use tokio::{
    sync::mpsc,
    time::{interval, sleep, timeout},
};
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::exchange::types::{SubscriptionType, WebSocketMessage};
// ИСПРАВЛЕНО: Убедимся, что connect_auth_and_subscribe_internal импортируется правильно, если он нужен здесь.
// Но он вызывается из connection.rs, так что этот импорт может быть не нужен напрямую в read_loop.rs.
// Оставляю для полноты, если он был у тебя. Если нет, то можно убрать.
// use crate::exchange::bybit_ws::connection::connect_auth_and_subscribe_internal; 
// ИСПРАВЛЕНО: connect_auth_and_subscribe_internal теперь в connection.rs, используем относительный путь, если бы он был в том же модуле.
// Но он в connection.rs, так что этот импорт тут некорректен, если он не pub(crate) или pub.
// Для вызова из connection.rs в connection.rs он не нужен.
// Для вызова из read_loop.rs в connection.rs он должен быть импортирован из crate::exchange::bybit_ws::connection
use crate::exchange::bybit_ws::protocol::handle_message;
use crate::exchange::bybit_ws::{WsStream, WsSink, READ_TIMEOUT_SECONDS};

// Определение функции теперь принимает 7 аргументов
pub(super) async fn read_loop(
    mut ws_reader: WsStream,
    mut ws_sender: WsSink,
    mpsc_sender: mpsc::Sender<Result<WebSocketMessage>>,
    config: Config,
    subscriptions: Vec<SubscriptionType>,
    base_ws_url: String,
    stream_description: String, // 7-й аргумент
) {
    info!(stream = %stream_description, "WebSocket read_loop запущен.");
    let ping_interval_secs = config.ws_ping_interval_secs.max(5); // Минимальный интервал пинга 5с
    let reconnect_delay_secs = config.ws_reconnect_delay_secs;

    'reconnect_loop: loop {
        if mpsc_sender.is_closed() {
            info!(stream = %stream_description, "MPSC канал закрыт извне, остановка read_loop.");
            break 'reconnect_loop;
        }

        let mut ping_timer = interval(Duration::from_secs(ping_interval_secs));
        let mut last_pong_received = Instant::now();
        // Таймаут ожидания понга должен быть больше интервала пинга + запас
        let pong_timeout = Duration::from_secs(ping_interval_secs + READ_TIMEOUT_SECONDS.max(15)); // Увеличенный запас

        info!(stream = %stream_description, "Вход во внутренний цикл обработки сообщений.");
        loop {
            tokio::select! {
                biased; // Приоритет чтению сообщений

                maybe_message_result = timeout(pong_timeout + Duration::from_secs(10), ws_reader.next()) => { // Общий таймаут на сообщение
                    match maybe_message_result {
                        Ok(Some(Ok(message))) => {
                            last_pong_received = Instant::now(); // Сброс таймера понга при любом валидном сообщении
                            if message.is_pong() {
                                 debug!(stream = %stream_description, "Получен Pong.");
                                 if mpsc_sender.send(Ok(WebSocketMessage::Pong)).await.is_err() {
                                     warn!(stream = %stream_description, "MPSC получатель сброшен при отправке Pong.");
                                     break 'reconnect_loop;
                                 }
                                 continue;
                            }
                            if let Err(handle_error) = handle_message(message, &mpsc_sender).await {
                                let error_string = handle_error.to_string();
                                warn!(stream = %stream_description, "Ошибка обработки WebSocket сообщения: {}", error_string);
                                
                                let is_mpsc_dropped = error_string.contains("MPSC получатель сброшен");
                                let is_ws_closed_by_remote = error_string.contains("WebSocket закрыт удаленной стороной");

                                if is_mpsc_dropped {
                                    warn!(stream = %stream_description, "MPSC получатель сброшен, выход из read_loop.");
                                    break 'reconnect_loop;
                                }
                                
                                // Отправляем ошибку в канал, если это не ошибка MPSC
                                if mpsc_sender.send(Err(handle_error)).await.is_err() {
                                     warn!(stream = %stream_description, "MPSC получатель сброшен при отправке ошибки обработки (повторно).");
                                     break 'reconnect_loop;
                                }

                                if is_ws_closed_by_remote {
                                     info!(stream = %stream_description, "Разрыв внутреннего цикла из-за ошибки 'WebSocket закрыт удаленной стороной' от handle_message.");
                                     break; 
                                 }
                            }
                        }
                        Ok(Some(Err(protocol_error))) => {
                            error!(stream = %stream_description, "Ошибка протокола WebSocket: {}", protocol_error);
                            if mpsc_sender.send(Err(anyhow!("Ошибка протокола WebSocket ({}) : {}", stream_description, protocol_error))).await.is_err() {
                                break 'reconnect_loop;
                            }
                            break; 
                        }
                        Ok(None) => { 
                            info!(stream = %stream_description, "WebSocket стрим закрыт удаленной стороной (Ok(None)).");
                            if !mpsc_sender.is_closed() {
                                if mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await.is_err() {
                                    warn!(stream = %stream_description, "MPSC получатель сброшен при отправке Disconnected (Ok(None)).");
                                    break 'reconnect_loop;
                                }
                            }
                            break; 
                        }
                         Err(_timeout_elapsed_error) => { 
                            error!(stream = %stream_description, "Таймаут чтения WebSocket сообщения (>{:?}).", pong_timeout + Duration::from_secs(10));
                            if last_pong_received.elapsed() > pong_timeout {
                                error!(stream = %stream_description, "Таймаут ответа Pong WebSocket ({:?} истекли). Попытка переподключения.", pong_timeout);
                                if mpsc_sender.send(Err(anyhow!("Таймаут ответа Pong WebSocket ({})", stream_description))).await.is_err() {
                                     break 'reconnect_loop;
                                }
                                break; 
                            } else {
                                warn!(stream = %stream_description, "Таймаут чтения сообщения, но понг был недавно. Отправляем Ping.");
                                let req_id = format!("timeout_ping_{}_{}", stream_description.to_lowercase().replace(" ", "_"), SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
                                let ping_message = json!({"op": "ping", "req_id": req_id}).to_string();
                                if let Err(e) = ws_sender.send(Message::Text(ping_message.into())).await {
                                    error!(stream = %stream_description, "Не удалось отправить WebSocket Ping после таймаута чтения: {}", e);
                                    if mpsc_sender.send(Err(anyhow!("Не удалось отправить Ping после таймаута ({}) : {}", stream_description, e))).await.is_err() {
                                        break 'reconnect_loop;
                                    }
                                    break; 
                                }
                                last_pong_received = Instant::now(); // Считаем, что пинг сбросил таймер ожидания понга
                            }
                        }
                    }
                }
                _ = ping_timer.tick() => {
                    if last_pong_received.elapsed() > pong_timeout {
                         error!(stream = %stream_description, "Таймаут Pong WebSocket при проверке таймером Ping (elapsed: {:?}).", last_pong_received.elapsed());
                         if mpsc_sender.send(Err(anyhow!("Таймаут Pong WebSocket при проверке Ping ({})", stream_description))).await.is_err() {
                             break 'reconnect_loop;
                         }
                         break; // Реконнект
                    }
                    
                    debug!(stream = %stream_description, "Отправка WebSocket Ping по таймеру");
                    // ИСПРАВЛЕНО: Используем SystemTime и UNIX_EPOCH из std::time
                    let req_id = format!("timer_ping_{}_{}", stream_description.to_lowercase().replace(" ", "_"), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
                    let ping_message = json!({"op": "ping", "req_id": req_id}).to_string();
                    
                    // ИСПРАВЛЕНО: Убираем проверку ws_sender.is_closed()
                    if let Err(e) = ws_sender.send(Message::Text(ping_message.into())).await {
                        error!(stream = %stream_description, "Не удалось отправить WebSocket Ping по таймеру: {}", e);
                        if mpsc_sender.send(Err(anyhow!("Не удалось отправить WebSocket Ping по таймеру ({}) : {}", stream_description, e))).await.is_err() {
                            break 'reconnect_loop;
                        }
                        break; // Реконнект
                    }
                }
                // Ветка для проверки закрытия mpsc_sender, если другие ветки select не активны
                 _ = async { loop { if mpsc_sender.is_closed() { break; } tokio::time::sleep(Duration::from_secs(1)).await; } } => {
                    info!(stream = %stream_description, "MPSC канал закрыт извне (обнаружено в фоновой проверке).");
                    break 'reconnect_loop;
                 }
            } // конец select!
        } // конец внутреннего loop

        info!(stream = %stream_description, "Внутренний цикл обработки сообщений завершен. Попытка переподключения через {} секунд...", reconnect_delay_secs);
        
        if !mpsc_sender.is_closed() {
            if mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await.is_err() {
                info!(stream = %stream_description, "MPSC канал закрыт во время уведомления о дисконнекте, остановка попыток переподключения.");
                break 'reconnect_loop;
            }
        }

        sleep(Duration::from_secs(reconnect_delay_secs)).await;

        let config_for_reconnect = config.clone(); 
        let subscriptions_for_reconnect = subscriptions.clone(); 

        // ИСПРАВЛЕНО: вызов connect_auth_and_subscribe_internal с 4-мя аргументами
        // и это вызов из connection.rs, а не локальный.
        // Мы передаем &str для stream_description, так как функция ожидает &str
        match crate::exchange::bybit_ws::connection::connect_auth_and_subscribe_internal(
            &base_ws_url, 
            &config_for_reconnect, 
            &subscriptions_for_reconnect,
            &stream_description // Передаем &String как &str
        ).await {
            Ok((new_reader, new_sender)) => {
                info!(stream = %stream_description, "WebSocket успешно переподключен!");
                ws_reader = new_reader;
                ws_sender = new_sender;
                if !mpsc_sender.is_closed() {
                    if mpsc_sender.send(Ok(WebSocketMessage::Connected)).await.is_err() {
                        warn!(stream = %stream_description, "MPSC получатель сброшен после успешного переподключения.");
                        break 'reconnect_loop;
                    }
                }
            }
            Err(e) => {
                 error!(stream = %stream_description, "Попытка переподключения WebSocket не удалась: {}. Повтор после задержки...", e);
            }
        }
    }

    info!(stream = %stream_description, "WebSocket read_loop окончательно остановлен.");
}