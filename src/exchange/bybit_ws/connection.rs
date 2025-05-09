// src/exchange/bybit_ws/connection.rs

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{info, warn, error}; // Добавил error для логгирования
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use futures_util::{StreamExt, SinkExt}; // Добавил SinkExt для ws_sender

use crate::config::Config;
use crate::exchange::types::{SubscriptionType, WebSocketMessage};
use crate::exchange::bybit_ws::protocol::{authenticate, subscribe};
use crate::exchange::bybit_ws::read_loop::read_loop;
use crate::exchange::bybit_ws::{WsStream, WsSink, CONNECT_TIMEOUT_SECONDS};

// Сделали pub(super)
pub(super) async fn connect_auth_and_subscribe_internal(
    base_ws_url: &str,
    config: &Config,
    subscriptions: &[SubscriptionType],
) -> Result<(WsStream, WsSink)> {
    info!(url = %base_ws_url, "Attempting WS connection...");
    let connect_future = connect_async(base_ws_url); // base_ws_url уже &str

    // Оборачиваем connect_future в timeout
    match timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS), connect_future).await {
        Ok(Ok((ws_stream, response))) => { // Успешное соединение внутри таймаута
            info!(status = %response.status(), "WebSocket connection established.");
            let (mut ws_sender, ws_reader) = ws_stream.split();

            // Аутентификация
            if let Err(e) = authenticate(&mut ws_sender, &config.bybit_api_key, &config.bybit_api_secret).await {
                error!("WS Authentication failed: {}", e);
                return Err(e.context("WS Authentication failed")); // Возвращаем ошибку, если аутентификация не удалась
            }
            info!("Authentication request sent and presumably successful."); // Успех, если нет ошибки

            // Подписки
            let args: Vec<String> = subscriptions.iter().map(|sub| match sub {
                SubscriptionType::Order => "order".to_string(),
                SubscriptionType::PublicTrade { symbol } => format!("publicTrade.{}", symbol),
                SubscriptionType::Orderbook { symbol, depth } => format!("orderbook.{}.{}", depth, symbol),
            }).collect();

            if !args.is_empty() {
                if let Err(e) = subscribe(&mut ws_sender, args).await {
                    error!("WS Subscription failed: {}", e);
                    return Err(e.context("WS Subscription failed")); // Возвращаем ошибку, если подписка не удалась
                }
                info!("Subscription request sent and presumably successful."); // Успех, если нет ошибки
            } else {
                warn!("No subscriptions requested.");
            }
            Ok((ws_reader, ws_sender))
        }
        Ok(Err(e)) => { // Ошибка от connect_async
            error!("WebSocket connection error (from connect_async): {}", e);
            Err(e.into()) // Преобразуем tungstenite::Error в anyhow::Error
        }
        Err(_) => { // Таймаут соединения
            error!("WebSocket connection timed out after {} seconds.", CONNECT_TIMEOUT_SECONDS);
            Err(anyhow::anyhow!("WebSocket connection timed out"))
        }
    }
}

// Основная публичная функция
pub async fn connect_and_subscribe(
    config: Config,
    subscriptions: Vec<SubscriptionType>,
) -> Result<mpsc::Receiver<Result<WebSocketMessage>>> {
    let base_ws_url = if config.use_testnet {
        "wss://stream-testnet.bybit.com/v5/private"
    } else {
        "wss://stream.bybit.com/v5/private"
    };

    // Вызываем обновленный connect_auth_and_subscribe_internal
    let (ws_reader, ws_sender) = connect_auth_and_subscribe_internal(
        base_ws_url, &config, &subscriptions
    ).await.context("Initial WebSocket connection and subscription setup failed")?; // Более детальное сообщение

    let (mpsc_tx, mpsc_rx) = mpsc::channel::<Result<WebSocketMessage>>(100);

    // Сообщение Connected отправляется только после успешного соединения, аутентификации и подписки
    if mpsc_tx.send(Ok(WebSocketMessage::Connected)).await.is_err() {
        warn!("MPSC receiver closed immediately after successful connection and setup.");
        // В этом случае нет смысла запускать read_loop, так как никто не слушает
        return Ok(mpsc_rx); // Возвращаем приемник, но он, вероятно, будет бесполезен
    }

    tokio::spawn(read_loop(
        ws_reader,
        ws_sender,
        mpsc_tx,
        config.clone(),
        subscriptions, // subscriptions уже Vec<...>
        base_ws_url.to_string(), // передаем как String
    ));

    info!("WebSocket reader task spawned. Returning receiver channel.");
    Ok(mpsc_rx)
}