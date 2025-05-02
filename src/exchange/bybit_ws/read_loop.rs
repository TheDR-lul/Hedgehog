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
    subscriptions: Vec<SubscriptionType>,
    base_ws_url: String,
) {
    info!("WebSocket read_loop started.");
    let ping_interval_secs = config.ws_ping_interval_secs;
    let reconnect_delay_secs = config.ws_reconnect_delay_secs;

    'reconnect_loop: loop {

        if mpsc_sender.is_closed() {
            info!("MPSC channel closed externally, stopping read_loop.");
            break 'reconnect_loop;
        }

        let mut ping_timer = interval(Duration::from_secs(ping_interval_secs));
        let mut last_pong_received = Instant::now();
        let pong_timeout = Duration::from_secs(READ_TIMEOUT_SECONDS);

        info!("Entering inner message processing loop.");
        loop {
            tokio::select! {
                maybe_message = timeout(pong_timeout, ws_reader.next()) => {
                    match maybe_message {
                        Ok(Some(Ok(message))) => {
                            last_pong_received = Instant::now();
                            if message.is_pong() {
                                 debug!("Received Pong.");
                                 if mpsc_sender.send(Ok(WebSocketMessage::Pong)).await.is_err() { break 'reconnect_loop; }
                                 continue;
                            }
                            // --- ИСПРАВЛЕНО: Обработка ошибки handle_error ---
                            if let Err(handle_error) = handle_message(message, &mpsc_sender).await {
                                warn!("Error handling WebSocket message: {}", handle_error);
                                // Сначала проверяем сообщение об ошибке
                                let is_close_error = handle_error.to_string().contains("WebSocket closed");
                                // Затем отправляем ошибку в канал (перемещаем handle_error)
                                 if mpsc_sender.send(Err(handle_error)).await.is_err() {
                                     // Если канал закрыт, выходим из всего
                                     warn!("MPSC receiver dropped while sending handle error.");
                                     break 'reconnect_loop;
                                 }
                                 // Теперь проверяем флаг is_close_error
                                 if is_close_error {
                                     info!("Breaking inner loop due to WebSocket closed error.");
                                     break; // Выход из внутреннего цикла для реконнекта
                                 }
                                 // Если это не ошибка закрытия, просто продолжаем цикл
                            }
                            // --- КОНЕЦ ИСПРАВЛЕНИЯ ---
                        }
                        Ok(Some(Err(protocol_error))) => {
                            error!("WebSocket protocol error: {}", protocol_error);
                            let _ = mpsc_sender.send(Err(anyhow!("WebSocket protocol error: {}", protocol_error))).await;
                            break;
                        }
                        Ok(None) => { info!("WebSocket stream closed by remote."); break; }
                         Err(_) => {
                            error!("WebSocket Pong read timeout ({} seconds).", pong_timeout.as_secs());
                            let _ = mpsc_sender.send(Err(anyhow!("WebSocket Pong read timeout"))).await;
                            break;
                        }
                    }
                }
                _ = ping_timer.tick() => {
                    if last_pong_received.elapsed() > pong_timeout {
                         error!("WebSocket Pong timeout check failed (elapsed: {:?}).", last_pong_received.elapsed());
                         let _ = mpsc_sender.send(Err(anyhow!("WebSocket Pong timeout check failed"))).await;
                         break;
                    }
                    debug!("Sending WebSocket Ping");
                    let ping_message = json!({"op": "ping"}).to_string();
                    if let Err(e) = ws_sender.send(Message::Text(ping_message.into())).await { // Оставляем .into() здесь
                        error!("Failed to send WebSocket Ping: {}", e);
                        let _ = mpsc_sender.send(Err(anyhow!("Failed to send WebSocket Ping: {}", e))).await;
                        break;
                    }
                }
                 else => { if mpsc_sender.is_closed() { info!("MPSC channel closed externally."); break 'reconnect_loop; } }
            } // конец select!
        } // конец внутреннего loop

        // --- Логика переподключения ---
        info!("Inner message loop exited. Attempting reconnect after {} seconds...", reconnect_delay_secs);
        if mpsc_sender.send(Ok(WebSocketMessage::Disconnected)).await.is_err() {
            info!("MPSC channel closed during disconnect notification, stopping reconnect attempts.");
            break 'reconnect_loop;
        }

        sleep(Duration::from_secs(reconnect_delay_secs)).await;

        match connect_auth_and_subscribe_internal(&base_ws_url, &config, &subscriptions).await {
            Ok((new_reader, new_sender)) => {
                info!("WebSocket reconnected successfully!");
                ws_reader = new_reader;
                ws_sender = new_sender;
                if mpsc_sender.send(Ok(WebSocketMessage::Connected)).await.is_err() {
                    warn!("MPSC receiver dropped after successful reconnect.");
                    break 'reconnect_loop;
                }
                continue 'reconnect_loop;
            }
            Err(e) => {
                 error!("WebSocket reconnect attempt failed: {}. Retrying after delay...", e);
                 // Пауза уже была перед попыткой, внешний цикл сам повторит
            }
        }
    } // конец внешнего 'reconnect_loop

    info!("WebSocket read_loop permanently stopped.");
}