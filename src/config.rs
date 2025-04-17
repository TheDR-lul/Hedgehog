use serde::Deserialize;
use std::env;
use anyhow::Result;
use config::{Config as Loader, Environment, File};

#[derive(Deserialize, Debug)]
pub struct Config {
    pub bybit_api_key: String,
    pub bybit_api_secret: String,
    pub sqlite_path: String,
    pub telegram_token: String,
    pub default_volatility: f64,
    pub offset_points: u32,
}

impl Config {
    pub fn load() -> Result<Self> {
        // Файл можно указать через ENV HEDGER_CONFIG, иначе — Config.toml
        let file = env::var("HEDGER_CONFIG").unwrap_or_else(|_| "Config.toml".into());
        let loader = Loader::builder()
            .add_source(File::with_name(&file).required(false))
            .add_source(Environment::with_prefix("HEDGER").separator("__"))
            .build()?;
        let cfg = loader.try_deserialize::<Config>()?;
        Ok(cfg)
    }
}