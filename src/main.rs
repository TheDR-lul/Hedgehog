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
use teloxide::{Bot, prelude::*};
use crate::exchange::Bybit;

static DB: OnceCell<storage::Db> = OnceCell::const_new();

#[tokio::main]
async fn main() -> Result<()> {
    // Загружаем конфиг и инициализируем логгер
    let cfg = config::Config::load()?;
    logger::init(&cfg);

    // Подключаемся к SQLite (создаст файл/папку)
    let db = storage::Db::connect(&cfg.sqlite_path).await?;
    DB.set(db).unwrap();

    // Инициализируем бота и биржевой клиент
    let bot = Bot::builder(&cfg.telegram_token).build().auto_send();
    let mut exchange = Bybit::new(&cfg.bybit_api_key, &cfg.bybit_api_secret);
    exchange.check_connection().await?;

    // Запускаем Telegram‑логику
    telegram::run(bot, exchange).await;
    Ok(())
}
