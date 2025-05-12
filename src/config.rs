// src/config.rs
use serde::Deserialize;
use std::env;
use anyhow::Result;
use config::{Config as Loader, Environment, File};

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HedgeStrategy {
    Sequential,
    WebsocketChunks,
}
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum WsLimitOrderPlacementStrategy {
    BestAskBid,
    OneTickInside,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    // Bybit
    pub bybit_api_key:    String,
    pub bybit_api_secret: String,
    pub use_testnet:      bool,
    pub bybit_base_url:   Option<String>,

    // SQLite
    pub sqlite_path:      String,

    // Telegram
    pub telegram_token:   String,

    // Общая Стратегия
    pub default_volatility: f64,
    pub offset_points:      u32,
    pub quote_currency:     String,
    pub slippage:           f64,
    pub max_wait_secs:      u64,
    pub max_allowed_leverage: f64,

    // Параметры WS стратегии
    #[serde(default = "default_hedge_strategy")]
    pub hedge_strategy_default: HedgeStrategy,

    #[serde(default = "default_ws_auto_chunk_target_count")]
    pub ws_auto_chunk_target_count: u32,

    #[serde(default = "default_ws_order_book_depth")]
    pub ws_order_book_depth: u32,

    #[serde(default = "default_ws_max_value_imbalance_ratio")]
    pub ws_max_value_imbalance_ratio: Option<f64>,

    #[serde(default = "default_ws_reconnect_delay_secs")]
    pub ws_reconnect_delay_secs: u64,

    #[serde(default = "default_ws_ping_interval_secs")]
    pub ws_ping_interval_secs: u64,

    #[serde(default = "default_ws_limit_order_placement_strategy")]
    pub ws_limit_order_placement_strategy: WsLimitOrderPlacementStrategy,

    #[serde(default = "default_ws_stale_price_ratio")]
    pub ws_stale_price_ratio: Option<f64>,

    // --- ДОБАВЛЕНО НОВОЕ ПОЛЕ ---
    #[serde(default = "default_ws_mpsc_buffer_size")]
    pub ws_mpsc_buffer_size: Option<u32>, // Размер буфера для MPSC канала WebSocket
}

// --- Функции для значений по умолчанию ---
fn default_hedge_strategy() -> HedgeStrategy { HedgeStrategy::Sequential }
fn default_ws_auto_chunk_target_count() -> u32 { 15 }
fn default_ws_order_book_depth() -> u32 { 50 } // Увеличил для примера, было 10
fn default_ws_max_value_imbalance_ratio() -> Option<f64> { Some(0.05) }
fn default_ws_reconnect_delay_secs() -> u64 { 5 }
fn default_ws_ping_interval_secs() -> u64 { 20 }
fn default_ws_limit_order_placement_strategy() -> WsLimitOrderPlacementStrategy { WsLimitOrderPlacementStrategy::BestAskBid }
fn default_ws_stale_price_ratio() -> Option<f64> { Some(0.01) }

// --- ДОБАВЛЕНА ФУНКЦИЯ ПО УМОЛЧАНИЮ ДЛЯ НОВОГО ПОЛЯ ---
fn default_ws_mpsc_buffer_size() -> Option<u32> { Some(100) } // Значение по умолчанию 100

impl Config {
    pub fn load() -> Result<Self> {
        let file_path = env::var("HEDGER_CONFIG").unwrap_or_else(|_| "Config.toml".into());
        println!("Loading configuration from: {}", file_path); // Для отладки
        let s = Loader::builder()
            .add_source(File::with_name(&file_path).required(false))
            .add_source(Environment::with_prefix("HEDGER").separator("__"));
        
        match s.build() {
            Ok(loader) => {
                match loader.try_deserialize() {
                    Ok(config) => Ok(config),
                    Err(e) => {
                        eprintln!("Failed to deserialize config: {}", e);
                        Err(e.into())
                    }
                }
            },
            Err(e) => {
                 eprintln!("Failed to build config loader (path: {}): {}", file_path, e);
                 Err(e.into())
            }
        }
    }
}