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
use teloxide::prelude::*;

static DB: OnceCell<storage::Db> = OnceCell::const_new();

#[tokio::main]
async fn main() -> Result<()> {
    // Загружаем настройки (в том числе sqlite_path)
    let cfg = config::Config::load()?;
    logger::init(&cfg);

    // Подключаемся к локальной SQLite (папка и файл будут созданы автоматически)
    let db = storage::Db::connect(&cfg.sqlite_path).await?;
    DB.set(db).unwrap();

    // Инициализируем Telegram-бота и API биржи
    let bot = Bot::new(&cfg.telegram_token).auto_send();
    let mut exchange = exchange::Bybit::new(&cfg.bybit_api_key, &cfg.bybit_api_secret);
    exchange.check_connection().await?;

    // Запускаем Telegram‑логику
    telegram::run(bot, exchange).await;
    Ok(())
}