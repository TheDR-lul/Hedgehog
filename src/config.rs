use serde::Deserialize;
use std::env;
use anyhow::Result;
use config::{Config as Loader, Environment, File};

fn default_offset_points() -> u32 { 10 }

#[derive(Deserialize, Debug)]
pub struct Config {
    pub bybit_api_key:    String,
    pub bybit_api_secret: String,

    /// Приоритетный URL. Если задан, мы его используем.
    pub bybit_base_url:   Option<String>,

    /// Если bybit_base_url = None и use_testnet = true, то тестнет.
    #[serde(default)]
    pub use_testnet:      bool,

    pub sqlite_path:      String,
    pub telegram_token:   String,
    pub default_volatility: f64,
    #[serde(default = "default_offset_points")]
    pub offset_points:     u32,
}

impl Config {
    pub fn load() -> Result<Self> {
        dotenv::dotenv().ok();
        let file = env::var("HEDGER_CONFIG").unwrap_or_else(|_| "Config.toml".into());

        let loader = Loader::builder()
            .add_source(File::with_name(&file).required(false))
            .add_source(Environment::with_prefix("HEDGER").separator("__"))
            .build()?;
        let cfg = loader.try_deserialize::<Config>()?;
        Ok(cfg)
    }
}
