// src/config.rs
use serde::Deserialize;
use std::env;
use anyhow::Result;
use config::{Config as Loader, Environment, File};

// ... ( HedgeStrategy и WsLimitOrderPlacementStrategy без изменений ) ...
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
// --- КОНЕЦ ДОБАВЛЕНИЙ ---

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

    // --- Параметры WS стратегии ---
    #[serde(default = "default_hedge_strategy")]
    pub hedge_strategy_default: HedgeStrategy,

    #[serde(default = "default_ws_auto_chunk_target_count")]
    pub ws_auto_chunk_target_count: u32,

    #[serde(default = "default_ws_order_book_depth")]
    pub ws_order_book_depth: u32,

    // --- ИЗМЕНЕНО ТУТ ---
    #[serde(default = "default_ws_max_value_imbalance_ratio")]
    pub ws_max_value_imbalance_ratio: Option<f64>, // <-- Сделали Option<f64>

    #[serde(default = "default_ws_reconnect_delay_secs")]
    pub ws_reconnect_delay_secs: u64,

    #[serde(default = "default_ws_ping_interval_secs")]
    pub ws_ping_interval_secs: u64,

    #[serde(default = "default_ws_limit_order_placement_strategy")]
    pub ws_limit_order_placement_strategy: WsLimitOrderPlacementStrategy,

    // --- Добавим недостающий параметр из ТЗ ---
    #[serde(default = "default_ws_stale_price_ratio")]
    pub ws_stale_price_ratio: Option<f64>, // <-- Добавили и сделали Option<f64>
}

// --- Функции для значений по умолчанию ---
fn default_hedge_strategy() -> HedgeStrategy { HedgeStrategy::Sequential }
fn default_ws_auto_chunk_target_count() -> u32 { 15 }
fn default_ws_order_book_depth() -> u32 { 10 }
// --- ИЗМЕНЕНО ТУТ ---
fn default_ws_max_value_imbalance_ratio() -> Option<f64> { Some(0.05) } // <-- Возвращаем Some(...)
fn default_ws_reconnect_delay_secs() -> u64 { 5 }
fn default_ws_ping_interval_secs() -> u64 { 20 }
fn default_ws_limit_order_placement_strategy() -> WsLimitOrderPlacementStrategy { WsLimitOrderPlacementStrategy::BestAskBid }
// --- Добавим default для нового параметра ---
fn default_ws_stale_price_ratio() -> Option<f64> { Some(0.01) } // <-- Добавили

impl Config {
    pub fn load() -> Result<Self> {
        let file = env::var("HEDGER_CONFIG").unwrap_or_else(|_| "Config.toml".into());
        let loader = Loader::builder()
            .add_source(File::with_name(&file).required(false))
            .add_source(Environment::with_prefix("HEDGER").separator("__"))
            .build()?;
        Ok(loader.try_deserialize()?)
    }
}