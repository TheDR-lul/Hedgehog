// src/config.rs
use serde::Deserialize;
use std::env;
use anyhow::Result;
use config::{Config as Loader, Environment, File};

// --- ИЗМЕНЕНО: Добавлено Clone и новые поля ---
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

    // Стратегия
    pub default_volatility: f64,
    pub offset_points:      u32, // Пока не используется
    pub quote_currency:     String,
    pub slippage:           f64, // Добавлено
    pub commission:         f64, // Добавлено
    pub max_wait_secs:      u64, // Добавлено
}
// --- Конец изменений ---

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
