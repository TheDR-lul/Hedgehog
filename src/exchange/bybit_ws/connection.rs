// src/exchange/bybit_ws/connection.rs

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{info, warn};
use std::time::Duration;
// --- Добавлены/Изменены use ---
use tokio::time::timeout;
use tokio_tungstenite::connect_async; // Используем для connect_async
use futures_util::StreamExt; // Для метода .split()

use crate::config::Config;
use crate::exchange::types::{SubscriptionType, WebSocketMessage};
use crate::exchange::bybit_ws::protocol::{authenticate, subscribe};
use crate::exchange::bybit_ws::read_loop::read_loop;
use crate::exchange::bybit_ws::{WsStream, WsSink, CONNECT_TIMEOUT_SECONDS}; // Импортируем типы и константу из mod.rs

// Сделали pub(super)
pub(super) async fn connect_auth_and_subscribe_internal(
    base_ws_url: &str, // Принимаем &str
    config: &Config,
    subscriptions: &[SubscriptionType],
) -> Result<(WsStream, WsSink)> {
    info!(url = %base_ws_url, "Attempting WS connection...");
    // --- ИСПРАВЛЕНО: Передаем &str напрямую ---
    let connect_future = connect_async(base_ws_url);
    let (ws_stream, response) = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS), connect_future)
        .await
        .context("WebSocket connection timed out")??;
    info!(status = %response.status(), "WebSocket connection established.");
    // --- ИСПРАВЛЕНО: Теперь split() должен найтись ---
    let (mut ws_sender, ws_reader) = ws_stream.split();
    authenticate(&mut ws_sender, &config.bybit_api_key, &config.bybit_api_secret).await?;
    info!("Authentication request sent.");
    let args: Vec<String> = subscriptions.iter().map(|sub| match sub {
        SubscriptionType::Order => "order".to_string(),
        SubscriptionType::PublicTrade { symbol } => format!("publicTrade.{}", symbol),
        SubscriptionType::Orderbook { symbol, depth } => format!("orderbook.{}.{}", depth, symbol),
    }).collect();
    if !args.is_empty() {
        subscribe(&mut ws_sender, args).await?;
        info!("Subscription request sent.");
    } else { warn!("No subscriptions requested."); }
    Ok((ws_reader, ws_sender))
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

    let (ws_reader, ws_sender) = connect_auth_and_subscribe_internal(
        base_ws_url, &config, &subscriptions
    ).await.context("Initial WebSocket connection failed")?;

    let (mpsc_tx, mpsc_rx) = mpsc::channel::<Result<WebSocketMessage>>(100);

    if mpsc_tx.send(Ok(WebSocketMessage::Connected)).await.is_err() {
        warn!("MPSC receiver closed immediately after connection.");
        return Ok(mpsc_rx);
    }

    tokio::spawn(read_loop(
        ws_reader,
        ws_sender,
        mpsc_tx,
        config.clone(),
        subscriptions,
        base_ws_url.to_string(),
    ));

    info!("WebSocket reader task spawned. Returning receiver channel.");
    Ok(mpsc_rx)
}