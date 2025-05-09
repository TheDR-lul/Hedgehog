// src/exchange/bybit_ws/connection.rs

use anyhow::{Context, Result, anyhow}; // Added anyhow
use tokio::sync::mpsc;
use tracing::{info, warn, error}; 
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use futures_util::{StreamExt, SinkExt}; 

use crate::config::Config;
use crate::exchange::types::{SubscriptionType, WebSocketMessage};
use crate::exchange::bybit_ws::protocol::{authenticate, subscribe}; // subscribe is still needed for public
use crate::exchange::bybit_ws::read_loop::read_loop;
use crate::exchange::bybit_ws::{WsStream, WsSink, CONNECT_TIMEOUT_SECONDS};

// Existing function for private, authenticated streams
pub(super) async fn connect_auth_and_subscribe_internal(
    base_ws_url: &str, // This will be the private URL
    config: &Config,
    subscriptions: &[SubscriptionType],
) -> Result<(WsStream, WsSink)> {
    info!(url = %base_ws_url, "Attempting Authenticated WS connection...");
    let connect_future = connect_async(base_ws_url); 

    match timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS), connect_future).await {
        Ok(Ok((ws_stream, response))) => { 
            info!(status = %response.status(), "Authenticated WebSocket connection established.");
            let (mut ws_sender, ws_reader) = ws_stream.split();

            if let Err(e) = authenticate(&mut ws_sender, &config.bybit_api_key, &config.bybit_api_secret).await {
                error!("WS Authentication failed: {}", e);
                return Err(e.context("WS Authentication failed")); 
            }
            info!("Authentication request sent and presumably successful."); 

            let args: Vec<String> = subscriptions.iter().map(|sub| match sub {
                SubscriptionType::Order => "order".to_string(),
                SubscriptionType::PublicTrade { symbol } => format!("publicTrade.{}", symbol),
                SubscriptionType::Orderbook { symbol, depth } => format!("orderbook.{}.{}", depth, symbol),
            }).collect();

            if !args.is_empty() {
                if let Err(e) = subscribe(&mut ws_sender, args).await {
                    error!("WS Subscription (authenticated stream) failed: {}", e);
                    return Err(e.context("WS Subscription (authenticated stream) failed")); 
                }
                info!("Subscription request (authenticated stream) sent and presumably successful for: {:?}", subscriptions);
            } else {
                warn!("No subscriptions requested for authenticated stream.");
            }
            Ok((ws_reader, ws_sender))
        }
        Ok(Err(e)) => { 
            error!("WebSocket connection error (from connect_async): {}", e);
            Err(e.into()) 
        }
        Err(_) => { 
            error!("WebSocket connection timed out after {} seconds.", CONNECT_TIMEOUT_SECONDS);
            Err(anyhow::anyhow!("WebSocket connection timed out"))
        }
    }
}

// New function for public, non-authenticated streams
pub(super) async fn connect_subscribe_public_internal(
    public_ws_url: &str,
    config: &Config, // Still need config for ping intervals etc. in read_loop
    subscriptions: &[SubscriptionType],
) -> Result<(WsStream, WsSink)> {
    info!(url = %public_ws_url, "Attempting Public WS connection...");
    let connect_future = connect_async(public_ws_url);

    match timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS), connect_future).await {
        Ok(Ok((ws_stream, response))) => {
            info!(status = %response.status(), "Public WebSocket connection established.");
            let (mut ws_sender, ws_reader) = ws_stream.split();

            // No authentication needed for public streams

            let args: Vec<String> = subscriptions.iter().map(|sub| match sub {
                SubscriptionType::Order => {
                    // This should not happen for a public stream, but handle defensively
                    warn!("Attempted to subscribe to 'order' topic on a public stream. Skipping.");
                    String::new() // Return an empty string to filter out later
                },
                SubscriptionType::PublicTrade { symbol } => format!("publicTrade.{}", symbol),
                SubscriptionType::Orderbook { symbol, depth } => format!("orderbook.{}.{}", depth, symbol),
            }).filter(|s| !s.is_empty()).collect();

            if !args.is_empty() {
                if let Err(e) = subscribe(&mut ws_sender, args).await {
                    error!("WS Subscription (public stream) failed: {}", e);
                    return Err(e.context("WS Subscription (public stream) failed"));
                }
                info!("Subscription request (public stream) sent and presumably successful for: {:?}", subscriptions);
            } else {
                warn!("No valid subscriptions requested for public stream.");
            }
            Ok((ws_reader, ws_sender))
        }
        Ok(Err(e)) => {
            error!("Public WebSocket connection error (from connect_async): {}", e);
            Err(e.into())
        }
        Err(_) => {
            error!("Public WebSocket connection timed out after {} seconds.", CONNECT_TIMEOUT_SECONDS);
            Err(anyhow::anyhow!("Public WebSocket connection timed out"))
        }
    }
}


// Existing main function for private, authenticated stream
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
    ).await.context("Initial Authenticated WebSocket connection and subscription setup failed")?;

    let (mpsc_tx, mpsc_rx) = mpsc::channel::<Result<WebSocketMessage>>(100);

    if mpsc_tx.send(Ok(WebSocketMessage::Connected)).await.is_err() {
        warn!("MPSC receiver closed immediately after successful authenticated connection and setup.");
        return Ok(mpsc_rx); 
    }

    let read_loop_subscriptions = subscriptions.clone(); // Clone subscriptions for the read_loop
    tokio::spawn(read_loop(
        ws_reader,
        ws_sender,
        mpsc_tx,
        config.clone(),
        read_loop_subscriptions, 
        base_ws_url.to_string(), 
    ));

    info!("Authenticated WebSocket reader task spawned. Returning receiver channel.");
    Ok(mpsc_rx)
}

// New main function for public, non-authenticated stream
pub async fn connect_public_stream(
    config: Config,        // For ws_ping_interval_secs, ws_reconnect_delay_secs
    category: &str,        // "spot", "linear", or "option"
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
        "inverse" => format!("{}inverse", public_ws_url_base), // Though not used currently
        "option" => format!("{}option", public_ws_url_base), // Though not used currently
        _ => return Err(anyhow!("Unsupported public WebSocket category: {}", category)),
    };

    let (ws_reader, ws_sender) = connect_subscribe_public_internal(
        &public_ws_url, &config, &subscriptions,
    ).await.context(format!("Initial Public WebSocket connection (category: {}) and subscription setup failed", category))?;

    let (mpsc_tx, mpsc_rx) = mpsc::channel::<Result<WebSocketMessage>>(100);

    if mpsc_tx.send(Ok(WebSocketMessage::Connected)).await.is_err() {
        warn!("MPSC receiver closed immediately after successful public connection and setup.");
        return Ok(mpsc_rx);
    }
    
    let read_loop_subscriptions = subscriptions.clone(); // Clone subscriptions for the read_loop
    tokio::spawn(read_loop(
        ws_reader,
        ws_sender,
        mpsc_tx,
        config.clone(), // config is cloned for read_loop
        read_loop_subscriptions,    // subscriptions is cloned for read_loop
        public_ws_url.clone(), // public_ws_url is cloned for read_loop
    ));

    info!("Public WebSocket reader task (category: {}) spawned. Returning receiver channel.", category);
    Ok(mpsc_rx)
}