// src/config.rs
use serde::Deserialize;
use std::env;
use anyhow::Result;
use config::{Config as Loader, Environment, File};

// --- ДОБАВЛЕНО: Перечисление для стратегий хеджирования ---
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")] // Чтобы в TOML можно было писать "sequential" или "websocketchunks"
pub enum HedgeStrategy {
    Sequential,
    WebsocketChunks,
}

// --- ДОБАВЛЕНО: Перечисление для стратегии выставления лимитных ордеров в WS ---
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")] // Например, BestAskBid
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
    pub offset_points:      u32, // Пока не используется
    pub quote_currency:     String,
    pub slippage:           f64, // Используется в Sequential и может быть резервным для WS
    pub max_wait_secs:      u64, // Используется в Sequential
    pub max_allowed_leverage: f64,

    // --- ДОБАВЛЕНЫ ПАРАМЕТРЫ ДЛЯ НОВОЙ СТРАТЕГИИ ---
    #[serde(default = "default_hedge_strategy")]
    pub hedge_strategy_default: HedgeStrategy, // Стратегия по умолчанию для выбора в боте

    #[serde(default = "default_ws_auto_chunk_target_count")]
    pub ws_auto_chunk_target_count: u32, // Целевое кол-во чанков для авто-режима

    #[serde(default = "default_ws_order_book_depth")]
    pub ws_order_book_depth: u32, // Глубина стакана для подписки/анализа

    #[serde(default = "default_ws_max_value_imbalance_ratio")]
    pub ws_max_value_imbalance_ratio: f64, // Макс. относительный дисбаланс стоимостей

    #[serde(default = "default_ws_reconnect_delay_secs")]
    pub ws_reconnect_delay_secs: u64, // Задержка переподключения WS

    #[serde(default = "default_ws_ping_interval_secs")]
    pub ws_ping_interval_secs: u64, // Интервал WS Ping

    #[serde(default = "default_ws_limit_order_placement_strategy")]
    pub ws_limit_order_placement_strategy: WsLimitOrderPlacementStrategy, // Стратегия выставления лимиток
    // --- КОНЕЦ ДОБАВЛЕННЫХ ПАРАМЕТРОВ ---
}

// --- Функции для значений по умолчанию ---
fn default_hedge_strategy() -> HedgeStrategy { HedgeStrategy::Sequential }
fn default_ws_auto_chunk_target_count() -> u32 { 15 }
fn default_ws_order_book_depth() -> u32 { 10 }
fn default_ws_max_value_imbalance_ratio() -> f64 { 0.05 }
fn default_ws_reconnect_delay_secs() -> u64 { 5 }
fn default_ws_ping_interval_secs() -> u64 { 20 }
fn default_ws_limit_order_placement_strategy() -> WsLimitOrderPlacementStrategy { WsLimitOrderPlacementStrategy::BestAskBid }
// --- Конец функций ---


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