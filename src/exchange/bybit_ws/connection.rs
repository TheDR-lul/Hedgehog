// src/exchange/bybit_ws/connection.rs

use anyhow::{anyhow, Context, Result};
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

// ИСПРАВЛЕНО: Возвращает Result<(WsStream, WsSink, Option<String>)> где Option<String> это req_id для подписок
pub(super) async fn connect_auth_and_subscribe_internal(
    base_ws_url: &str,
    config: &Config,
    subscriptions: &[SubscriptionType],
    stream_description: &str,
) -> Result<(WsStream, WsSink, Option<String>)> { // Возвращаем Option<String> для req_id подписки
    info!(stream = %stream_description, url = %base_ws_url, "Attempting Authenticated WS connection...");
    let connect_future = connect_async(base_ws_url);

    let (ws_stream, response) = match timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS), connect_future).await {
        Ok(Ok(connection_result)) => connection_result,
        Ok(Err(e)) => {
            error!(stream = %stream_description, "WebSocket connection error (from connect_async): {}", e);
            return Err(e.into());
        }
        Err(_) => {
            error!(stream = %stream_description, "WebSocket connection timed out after {} seconds.", CONNECT_TIMEOUT_SECONDS);
            return Err(anyhow::anyhow!("WebSocket connection timed out for {}", stream_description));
        }
    };
    
    info!(stream = %stream_description, status = %response.status(), "Authenticated WebSocket connection established.");
    let (mut ws_sender, ws_reader) = ws_stream.split();

    // Аутентификация
    let auth_req_id = authenticate(&mut ws_sender, &config.bybit_api_key, &config.bybit_api_secret).await
        .context(format!("WS Authentication failed for {}", stream_description))?;
    info!(stream = %stream_description, request_id = %auth_req_id, "Authentication request sent.");
    // TODO: Здесь можно добавить цикл ожидания ответа на аутентификацию, если Bybit его шлет и это нужно.
    // Обычно после успешного auth можно сразу слать subscribe.

    let args: Vec<String> = subscriptions.iter().filter_map(|sub| match sub {
        SubscriptionType::Order => Some("order".to_string()),
        _ => {
            warn!(stream = %stream_description, "Attempted to subscribe to non-private topic ({:?}) on a private stream. Skipping.", sub);
            None
        }
    }).collect();

    let mut subscription_req_id: Option<String> = None;
    if !args.is_empty() {
        match subscribe(&mut ws_sender, args.clone(), stream_description).await { // subscribe теперь возвращает Result<String>
            Ok(req_id) => {
                subscription_req_id = Some(req_id.clone());
                info!(stream = %stream_description, topics = ?args, request_id = %req_id, "Subscription request (authenticated stream) sent.");
            }
            Err(e) => {
                error!(stream = %stream_description, "WS Subscription (authenticated stream) failed: {}", e);
                return Err(e.context(format!("WS Subscription (authenticated stream) failed for {}", stream_description)));
            }
        }
    } else {
        warn!(stream = %stream_description, "No valid subscriptions requested for authenticated stream.");
    }
    Ok((ws_reader, ws_sender, subscription_req_id))
}

// ИСПРАВЛЕНО: Возвращает Result<(WsStream, WsSink, Option<String>)>
pub(super) async fn connect_subscribe_public_internal(
    public_ws_url: &str,
    _config: &Config, // config может понадобиться для read_loop, если он вызывается отсюда
    subscriptions: &[SubscriptionType],
    stream_description: &str,
) -> Result<(WsStream, WsSink, Option<String>)> {
    info!(stream = %stream_description, url = %public_ws_url, "Attempting Public WS connection...");
    let connect_future = connect_async(public_ws_url);

     let (ws_stream, response) = match timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS), connect_future).await {
        Ok(Ok(connection_result)) => connection_result,
        Ok(Err(e)) => {
            error!(stream = %stream_description, "Public WebSocket connection error (from connect_async): {}", e);
            return Err(e.into());
        }
        Err(_) => {
            error!(stream = %stream_description, "Public WebSocket connection timed out after {} seconds.", CONNECT_TIMEOUT_SECONDS);
            return Err(anyhow::anyhow!("Public WebSocket connection timed out for {}", stream_description));
        }
    };

    info!(stream = %stream_description, status = %response.status(), "Public WebSocket connection established.");
    let (mut ws_sender, ws_reader) = ws_stream.split();
    
    let args: Vec<String> = subscriptions.iter().filter_map(|sub| match sub {
        SubscriptionType::Order => {
            warn!(stream = %stream_description, "Attempted to subscribe to 'order' topic on a public stream. Skipping.");
            None
        },
        SubscriptionType::PublicTrade { symbol } => Some(format!("publicTrade.{}", symbol)),
        SubscriptionType::Orderbook { symbol, depth } => Some(format!("orderbook.{}.{}", depth, symbol)),
    }).collect();

    let mut subscription_req_id: Option<String> = None;
    if !args.is_empty() {
        match subscribe(&mut ws_sender, args.clone(), stream_description).await {
            Ok(req_id) => {
                subscription_req_id = Some(req_id.clone());
                info!(stream = %stream_description, topics = ?args, request_id = %req_id, "Subscription request (public stream) sent.");
            }
            Err(e) => {
                error!(stream = %stream_description, "WS Subscription (public stream) failed: {}", e);
                return Err(e.context(format!("WS Subscription (public stream) failed for {}", stream_description)));
            }
        }
    } else {
        warn!(stream = %stream_description, "No valid subscriptions requested for public stream.");
    }
    Ok((ws_reader, ws_sender, subscription_req_id))
}

// ИСПРАВЛЕНО: Возвращает Result<(mpsc::Receiver<...>, Option<String>)> для req_id
pub async fn connect_and_subscribe(
    config: Config,
    subscriptions: Vec<SubscriptionType>,
) -> Result<(mpsc::Receiver<Result<WebSocketMessage>>, Option<String>)> {
    let base_ws_url = if config.use_testnet {
        "wss://stream-testnet.bybit.com/v5/private"
    } else {
        "wss://stream.bybit.com/v5/private"
    };
    let stream_description = "Private";

    let (ws_reader, ws_sender, req_id) = connect_auth_and_subscribe_internal(
        base_ws_url, &config, &subscriptions, stream_description
    ).await.context("Initial Authenticated WebSocket connection and subscription setup failed")?;

    let buffer_size = config.ws_mpsc_buffer_size.unwrap_or(100).max(1) as usize;
    let (mpsc_tx, mpsc_rx) = mpsc::channel::<Result<WebSocketMessage>>(buffer_size);

    if mpsc_tx.send(Ok(WebSocketMessage::Connected)).await.is_err() {
        warn!(stream = %stream_description, "MPSC receiver closed immediately after successful authenticated connection.");
        // Возвращаем req_id даже если отправка Connected не удалась, т.к. соединение могло быть установлено
        return Ok((mpsc_rx, req_id)); 
    }

    let config_clone_for_read_loop = config.clone();
    let subscriptions_for_read_loop = subscriptions; 
    let base_ws_url_for_read_loop = base_ws_url.to_string();
    let stream_description_for_read_loop = stream_description.to_string();

    tokio::spawn(read_loop(
        ws_reader,
        ws_sender,
        mpsc_tx,
        config_clone_for_read_loop,
        subscriptions_for_read_loop,
        base_ws_url_for_read_loop,
        stream_description_for_read_loop,
    ));

    info!(stream = %stream_description, request_id = req_id.as_deref().unwrap_or("N/A"), "Authenticated WebSocket reader task spawned.");
    Ok((mpsc_rx, req_id))
}

// ИСПРАВЛЕНО: Возвращает Result<(mpsc::Receiver<...>, Option<String>)> для req_id
pub async fn connect_public_stream(
    config: Config,
    category: &str, 
    subscriptions: Vec<SubscriptionType>,
) -> Result<(mpsc::Receiver<Result<WebSocketMessage>>, Option<String>)> {
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
    let stream_description = format!("Public_{}", category.to_uppercase());

    let (ws_reader, ws_sender, req_id) = connect_subscribe_public_internal(
        &public_ws_url, &config, &subscriptions, &stream_description,
    ).await.context(format!("Initial Public WebSocket connection (category: {}) failed", category))?;
    
    let buffer_size = config.ws_mpsc_buffer_size.unwrap_or(100).max(1) as usize;
    let (mpsc_tx, mpsc_rx) = mpsc::channel::<Result<WebSocketMessage>>(buffer_size);
    
    if mpsc_tx.send(Ok(WebSocketMessage::Connected)).await.is_err() {
        warn!(stream = %stream_description, "MPSC receiver closed immediately after successful public connection.");
        return Ok((mpsc_rx, req_id));
    }
    
    let config_clone_for_read_loop = config.clone();
    let subscriptions_for_read_loop = subscriptions; 
    let public_ws_url_for_read_loop = public_ws_url.clone(); 
    let stream_description_for_read_loop = stream_description.clone();

    tokio::spawn(read_loop(
        ws_reader,
        ws_sender,
        mpsc_tx,
        config_clone_for_read_loop,
        subscriptions_for_read_loop, 
        public_ws_url_for_read_loop,
        stream_description_for_read_loop,
    ));

    info!(stream = %stream_description, request_id = req_id.as_deref().unwrap_or("N/A"), "Public WebSocket reader task spawned.");
    Ok((mpsc_rx, req_id))
}