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
    // 1️⃣ Загрузка конфига и логгера
    let cfg = config::Config::load()?;
    logger::init(&cfg);

    // 2️⃣ Подключение к SQLite
    let db = storage::Db::connect(&cfg.sqlite_path).await?;
    DB.set(db).unwrap();

    // 3️⃣ Инициализация бота
    let bot = Bot::new(&cfg.telegram_token);

    // 4️⃣ Клиент Bybit (prod или testnet по флагу)
    let mut exchange = Bybit::new(
        &cfg.bybit_api_key,
        &cfg.bybit_api_secret,
        cfg.use_testnet,      // <-- флаг из конфига
    );
    exchange.check_connection().await?;

    // 5️⃣ Запуск Telegram‑диспетчера
    telegram::run(bot, exchange).await;
    Ok(())
}
