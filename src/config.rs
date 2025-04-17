use serde::Deserialize;
use std::fs;
use anyhow::Result;

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
        let s = fs::read_to_string("Config.toml")?;
        let cfg = toml::from_str(&s)?;
        Ok(cfg)
    }
}
