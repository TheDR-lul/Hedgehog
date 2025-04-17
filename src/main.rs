// src/main.rs

mod config;
mod exchange;
mod hedger;
mod notifier;
mod logger;
mod models;
mod utils;
mod storage;
mod telegram;

use anyhow::Result;
use tokio::sync::OnceCell;
use teloxide::Bot;

use crate::config::Config;
use crate::exchange::{Bybit, Exchange};

static DB: OnceCell<storage::Db> = OnceCell::const_new();

#[tokio::main]
async fn main() -> Result<()> {
    // 1) Читаем конфиг и инициализируем логгер
    let cfg = Config::load()?;
    logger::init(&cfg);

    // 2) Подключаемся к локальной SQLite
    let db = storage::Db::connect(&cfg.sqlite_path).await?;
    DB.set(db).unwrap();

    // 3) Инициализируем Telegram Bot
    let bot = Bot::new(&cfg.telegram_token);

    // 4) Собираем base_url для Bybit
    let base_url: &str = cfg
        .bybit_base_url
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if cfg.use_testnet {
                "https://api-testnet.bybit.com"
            } else {
                "https://api.bybit.com"
            }
        });

    println!("Using Bybit base URL: {}", base_url);

    // 5) Создаём клиента биржи
    let mut exchange = Bybit::new(
        &cfg.bybit_api_key,
        &cfg.bybit_api_secret,
        base_url,
    )?;

    // 6) Пингуем Bybit
    exchange.check_connection().await?;

    // 7) Запускаем Telegram‑диспетчер
    telegram::run(bot, exchange).await;
    Ok(())
}
