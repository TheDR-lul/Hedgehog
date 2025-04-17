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

use crate::exchange::{Bybit, Exchange};   // импорт трейта!

static DB: OnceCell<storage::Db> = OnceCell::const_new();

#[tokio::main]
async fn main() -> Result<()> {
    // 1) конфиг и логгер
    let cfg = config::Config::load()?;
    logger::init(&cfg);

    // 2) SQLite
    let db = storage::Db::connect(&cfg.sqlite_path).await?;
    DB.set(db).unwrap();

    // 3) Telegram‑бот (нет метода builder в v0.15)
    let bot = Bot::new(&cfg.telegram_token);

    // 4) клиент биржи + ping
    let mut exchange = Bybit::new(&cfg.bybit_api_key, &cfg.bybit_api_secret);
    exchange.check_connection().await?;

    // 5) запускаем диспетчер
    telegram::run(bot, exchange).await;
    Ok(())
}