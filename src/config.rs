// src/config.rs
use anyhow::Result;
use config::{Config as ConfigLoader, Environment, File};
use serde::Deserialize;
use std::env;

#[derive(Deserialize, Debug)]
pub struct Config {
    pub bybit_api_key: String,
    pub bybit_api_secret: String,
    pub db_url: String,
    pub telegram_token: String,
    pub default_volatility: f64,
    pub offset_points: u32,
}

impl Config {
    pub fn load() -> Result<Self> {
        let file_path = env::var("HEDGER_CONFIG").unwrap_or_else(|_| "Config.toml".into());

        let loader = ConfigLoader::builder()
            .add_source(File::with_name(&file_path).required(false))
            .add_source(Environment::with_prefix("HEDGER").separator("__"))
            .build()?;

        let cfg = loader.try_deserialize::<Config>()?;
        Ok(cfg)
    }
}
