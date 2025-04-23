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
use tracing::info; // Добавим info для логирования

use crate::config::Config;
use crate::exchange::{Bybit, Exchange}; // Убедимся, что Exchange импортирован

static DB: OnceCell<storage::Db> = OnceCell::const_new();

#[tokio::main]
async fn main() -> Result<()> {
    // 1) Конфиг и логгер
    let cfg = Config::load()?;
    logger::init(&cfg);
    info!("Logger initialized. Default volatility = {}", cfg.default_volatility); // Логируем параметры

    // 2) Подключение к SQLite
    let db = storage::Db::connect(&cfg.sqlite_path).await?;
    DB.set(db).unwrap();
    info!("Connected to SQLite database: {}", cfg.sqlite_path);

    // 3) Telegram Bot
    let bot = Bot::new(&cfg.telegram_token);
    info!("Telegram bot initialized.");

    // 4) Выбираем base_url
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
    info!("Using Bybit base URL: {}", base_url); // Используем info! вместо println!

    // 5) Создаём клиента биржи
    // --- ИЗМЕНЕНО: Добавляем cfg.quote_currency ---
    let mut exchange = Bybit::new( // <-- Добавляем mut
        &cfg.bybit_api_key,
        &cfg.bybit_api_secret,
        base_url,
        &cfg.quote_currency, // <-- Передаем quote_currency
    ).await?; // <-- Добавляем .await, т.к. new теперь async
    info!("Bybit client created for quote currency: {}", cfg.quote_currency);

    // 6) Пингуем Bybit (check_connection теперь требует &mut self)
    info!("Pinging Bybit...");
    exchange.check_connection().await?; // Вызываем на mut exchange
    // Сообщение об успехе теперь внутри check_connection

    // 7) Стартуем Telegram‑диспетчер
    info!("Starting Telegram dispatcher...");
    telegram::run(bot, exchange).await; // Передаем mut exchange

    Ok(())
}
