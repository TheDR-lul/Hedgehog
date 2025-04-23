use serde::Deserialize;
use std::env;
use anyhow::Result;
use config::{Config as Loader, Environment, File};

#[derive(Deserialize, Debug)]
pub struct Config {
    // Bybit
    pub bybit_api_key:    String,
    pub bybit_api_secret: String,
    /// Если true — используем тестнет, иначе — прод
    pub use_testnet:      bool,
    /// Явно указать любой endpoint, вместо авто‑выбора
    pub bybit_base_url:   Option<String>,

    // SQLite
    pub sqlite_path:      String,

    // Telegram
    pub telegram_token:   String,

    // Стратегия
    pub default_volatility: f64,
    pub offset_points:      u32,
    pub quote_currency:     String,
}

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