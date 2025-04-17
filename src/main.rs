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
    // 1) Конфиг и логгер
    let cfg = config::Config::load()?;
    logger::init(&cfg);

    // 2) SQLite
    let db = storage::Db::connect(&cfg.sqlite_path).await?;
    DB.set(db).unwrap();

    // 3) Бот
    let bot = Bot::new(&cfg.telegram_token);

    // 4) Собираем правильный base_url
    let base_url = cfg.bybit_base_url
        .as_deref()
        .unwrap_or_else(|| if cfg.use_testnet {
            "https://api-testnet.bybit.com"
        } else {
            "https://api.bybit.com"
        });

    println!("Using Bybit base URL: {}", base_url);
    let mut exchange = Bybit::new(
        &cfg.bybit_api_key,
        &cfg.bybit_api_secret,
        base_url,
    );

    // Проверяем пинг (с отладочным выводом)
    exchange.check_connection().await?;

    // 5) Запуск Telegram‑логики
    telegram::run(bot, exchange).await;
    Ok(())
}
