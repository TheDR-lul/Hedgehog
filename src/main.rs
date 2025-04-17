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

use crate::exchange::{Bybit, Exchange};

static DB: OnceCell<storage::Db> = OnceCell::const_new();

#[tokio::main]
async fn main() -> Result<()> {
    // 1) Конфиг + логгер
    let cfg = config::Config::load()?;
    logger::init(&cfg);

    // 2) SQLite
    let db = storage::Db::connect(&cfg.sqlite_path).await?;
    DB.set(db).unwrap();

    // 3) Telegram‑бот
    let bot = Bot::new(&cfg.telegram_token);

    // 4) Клиент Bybit (prod/testnet)
    let mut exchange = Bybit::new(
        &cfg.bybit_api_key,
        &cfg.bybit_api_secret,
        cfg.use_testnet,   // <-- здесь флаг из Config
    );
    exchange.check_connection().await?;

    // 5) Запуск диспетчера
    telegram::run(bot, exchange).await;
    Ok(())
}