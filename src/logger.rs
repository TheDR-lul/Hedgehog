// src/logger.rs

use crate::config::Config;
use tracing_subscriber::fmt;
use tracing_subscriber::filter::EnvFilter;

/// Инициализация логирования через tracing
pub fn init(cfg: &Config) {
    // Попробуем получить уровень из RUST_LOG, иначе будем на уровне INFO
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false) // не показывать target (модуль)
        .init();

    tracing::info!("Logger initialized. Default volatility = {}", cfg.default_volatility);
}
