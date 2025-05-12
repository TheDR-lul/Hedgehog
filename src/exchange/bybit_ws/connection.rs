// src/exchange/bybit_ws/connection.rs

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc;
use tracing::{info, warn, error};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use futures_util::{StreamExt, SinkExt};

use crate::config::Config;
use crate::exchange::types::{SubscriptionType, WebSocketMessage};
use crate::exchange::bybit_ws::protocol::{authenticate, subscribe};
use crate::exchange::bybit_ws::read_loop::read_loop;
use crate::exchange::bybit_ws::{WsStream, WsSink, CONNECT_TIMEOUT_SECONDS};

// ИСПРАВЛЕНО: Определение функции теперь принимает 4 аргумента, включая stream_description
pub(super) async fn connect_auth_and_subscribe_internal(
    base_ws_url: &str,
    config: &Config,
    subscriptions: &[SubscriptionType],
    stream_description: &str, // 4-й аргумент для описания стрима
) -> Result<(WsStream, WsSink)> {
    info!(stream = %stream_description, url = %base_ws_url, "Attempting Authenticated WS connection...");
    let connect_future = connect_async(base_ws_url);

    match timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS), connect_future).await {
        Ok(Ok((ws_stream, response))) => {
            info!(stream = %stream_description, status = %response.status(), "Authenticated WebSocket connection established.");
            let (mut ws_sender, ws_reader) = ws_stream.split();

            if let Err(e) = authenticate(&mut ws_sender, &config.bybit_api_key, &config.bybit_api_secret).await {
                error!(stream = %stream_description, "WS Authentication failed: {}", e);
                return Err(e.context(format!("WS Authentication failed for {}", stream_description)));
            }
            info!(stream = %stream_description, "Authentication request sent and presumably successful.");

            let args: Vec<String> = subscriptions.iter().map(|sub| match sub {
                SubscriptionType::Order => "order".to_string(),
                // Публичные подписки не должны быть здесь для приватного стрима, но для полноты оставим
                SubscriptionType::PublicTrade { symbol } => format!("publicTrade.{}", symbol),
                SubscriptionType::Orderbook { symbol, depth } => format!("orderbook.{}.{}", depth, symbol),
            }).collect();

            if !args.is_empty() {
                // ИСПРАВЛЕНО: Вызываем subscribe с 3-м аргументом stream_description
                if let Err(e) = subscribe(&mut ws_sender, args.clone(), stream_description).await {
                    error!(stream = %stream_description, "WS Subscription (authenticated stream) failed: {}", e);
                    return Err(e.context(format!("WS Subscription (authenticated stream) failed for {}", stream_description)));
                }
                info!(stream = %stream_description, topics = ?args, "Subscription request (authenticated stream) sent and presumably successful.");
            } else {
                warn!(stream = %stream_description, "No subscriptions requested for authenticated stream.");
            }
            Ok((ws_reader, ws_sender))
        }
        Ok(Err(e)) => {
            error!(stream = %stream_description, "WebSocket connection error (from connect_async): {}", e);
            Err(e.into())
        }
        Err(_) => {
            error!(stream = %stream_description, "WebSocket connection timed out after {} seconds.", CONNECT_TIMEOUT_SECONDS);
            Err(anyhow::anyhow!("WebSocket connection timed out for {}", stream_description))
        }
    }
}

// Функция для публичных стримов, определение теперь принимает 4 аргумента
pub(super) async fn connect_subscribe_public_internal(
    public_ws_url: &str,
    config: &Config, // config нужен для read_loop
    subscriptions: &[SubscriptionType],
    stream_description: &str, // 4-й аргумент
) -> Result<(WsStream, WsSink)> {
    info!(stream = %stream_description, url = %public_ws_url, "Attempting Public WS connection...");
    let connect_future = connect_async(public_ws_url);

    match timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS), connect_future).await {
        Ok(Ok((ws_stream, response))) => {
            info!(stream = %stream_description, status = %response.status(), "Public WebSocket connection established.");
            let (mut ws_sender, ws_reader) = ws_stream.split();

            // No authentication needed for public streams

            let args: Vec<String> = subscriptions.iter().map(|sub| match sub {
                SubscriptionType::Order => {
                    warn!(stream = %stream_description, "Attempted to subscribe to 'order' topic on a public stream. Skipping.");
                    String::new() 
                },
                SubscriptionType::PublicTrade { symbol } => format!("publicTrade.{}", symbol),
                SubscriptionType::Orderbook { symbol, depth } => format!("orderbook.{}.{}", depth, symbol),
            }).filter(|s| !s.is_empty()).collect();

            if !args.is_empty() {
                // ИСПРАВЛЕНО: Вызываем subscribe с 3-м аргументом stream_description
                if let Err(e) = subscribe(&mut ws_sender, args.clone(), stream_description).await {
                    error!(stream = %stream_description, "WS Subscription (public stream) failed: {}", e);
                    return Err(e.context(format!("WS Subscription (public stream) failed for {}", stream_description)));
                }
                info!(stream = %stream_description, topics = ?args, "Subscription request (public stream) sent and presumably successful.");
            } else {
                warn!(stream = %stream_description, "No valid subscriptions requested for public stream.");
            }
            Ok((ws_reader, ws_sender))
        }
        Ok(Err(e)) => {
            error!(stream = %stream_description, "Public WebSocket connection error (from connect_async): {}", e);
            Err(e.into())
        }
        Err(_) => {
            error!(stream = %stream_description, "Public WebSocket connection timed out after {} seconds.", CONNECT_TIMEOUT_SECONDS);
            Err(anyhow::anyhow!("Public WebSocket connection timed out for {}", stream_description))
        }
    }
}

// Основная функция для приватного стрима
pub async fn connect_and_subscribe(
    config: Config,
    subscriptions: Vec<SubscriptionType>,
) -> Result<mpsc::Receiver<Result<WebSocketMessage>>> {
    let base_ws_url = if config.use_testnet {
        "wss://stream-testnet.bybit.com/v5/private"
    } else {
        "wss://stream.bybit.com/v5/private"
    };
    let stream_description = "Private"; // Описание для этого стрима

    // ИСПРАВЛЕНО: вызов connect_auth_and_subscribe_internal с 4-мя аргументами
    let (ws_reader, ws_sender) = connect_auth_and_subscribe_internal(
        base_ws_url, &config, &subscriptions, stream_description
    ).await.context("Initial Authenticated WebSocket connection and subscription setup failed")?;

    let buffer_size = config.ws_mpsc_buffer_size.unwrap_or(100).max(1) as usize;
    let (mpsc_tx, mpsc_rx) = mpsc::channel::<Result<WebSocketMessage>>(buffer_size);

    if mpsc_tx.send(Ok(WebSocketMessage::Connected)).await.is_err() {
        warn!(stream = %stream_description, "MPSC receiver closed immediately after successful authenticated connection and setup.");
        return Ok(mpsc_rx);
    }

    let config_clone_for_read_loop = config.clone();
    let subscriptions_for_read_loop = subscriptions; // subscriptions перемещается
    let base_ws_url_for_read_loop = base_ws_url.to_string();
    let stream_description_for_read_loop = stream_description.to_string();

    tokio::spawn(read_loop(
        ws_reader,
        ws_sender,
        mpsc_tx,
        config_clone_for_read_loop,
        subscriptions_for_read_loop,
        base_ws_url_for_read_loop,
        stream_description_for_read_loop, // ИСПРАВЛЕНО: 7-й аргумент добавлен
    ));

    info!(stream = %stream_description, "Authenticated WebSocket reader task spawned. Returning receiver channel.");
    Ok(mpsc_rx)
}

// Основная функция для публичных стримов
pub async fn connect_public_stream(
    config: Config,
    category: &str, 
    subscriptions: Vec<SubscriptionType>,
) -> Result<mpsc::Receiver<Result<WebSocketMessage>>> {
    let public_ws_url_base = if config.use_testnet {
        "wss://stream-testnet.bybit.com/v5/public/"
    } else {
        "wss://stream.bybit.com/v5/public/"
    };

    let public_ws_url = match category.to_lowercase().as_str() {
        "spot" => format!("{}spot", public_ws_url_base),
        "linear" => format!("{}linear", public_ws_url_base),
        "inverse" => format!("{}inverse", public_ws_url_base),
        "option" => format!("{}option", public_ws_url_base),
        _ => return Err(anyhow!("Unsupported public WebSocket category: {}", category)),
    };

    let stream_description = format!("Public {}", category.to_uppercase());

    // ИСПРАВЛЕНО: вызов connect_subscribe_public_internal с 4-мя аргументами
    let (ws_reader, ws_sender) = connect_subscribe_public_internal(
        &public_ws_url, &config, &subscriptions, &stream_description,
    ).await.context(format!("Initial Public WebSocket connection (category: {}) and subscription setup failed", category))?;
    
    let buffer_size = config.ws_mpsc_buffer_size.unwrap_or(100).max(1) as usize;
    let (mpsc_tx, mpsc_rx) = mpsc::channel::<Result<WebSocketMessage>>(buffer_size);

    if mpsc_tx.send(Ok(WebSocketMessage::Connected)).await.is_err() {
        warn!(stream = %stream_description, "MPSC receiver closed immediately after successful public connection and setup.");
        return Ok(mpsc_rx);
    }
    
    let config_clone_for_read_loop = config.clone();
    let subscriptions_for_read_loop = subscriptions; // subscriptions перемещается
    let public_ws_url_for_read_loop = public_ws_url.clone();
    let stream_description_for_read_loop = stream_description.clone();

    tokio::spawn(read_loop(
        ws_reader,
        ws_sender,
        mpsc_tx,
        config_clone_for_read_loop,
        subscriptions_for_read_loop, 
        public_ws_url_for_read_loop,
        stream_description_for_read_loop, // ИСПРАВЛЕНО: 7-й аргумент добавлен
    ));

    info!(stream = %stream_description, "Public WebSocket reader task spawned. Returning receiver channel.");
    Ok(mpsc_rx)
}