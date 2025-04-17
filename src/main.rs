mod config;
mod exchange;
mod hedger;
mod notifier;
mod logger;
mod models;
mod utils;
mod storage;

use anyhow::Result;
use tokio::sync::OnceCell;
use teloxide::prelude::*;

static DB: OnceCell<storage::Db> = OnceCell::const_new();

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = config::Config::load()?;
    logger::init(&cfg);
    let db = storage::Db::connect(&cfg.db_url).await?;
    DB.set(db).unwrap();
    let bot = Bot::new(&cfg.telegram_token).auto_send();

    let mut exchange = exchange::Bybit::new(&cfg.bybit_api_key, &cfg.bybit_api_secret);
    match exchange.check_connection().await {
        Ok(_) => println!("Bybit API OK"),
        Err(err) => eprintln!("Bybit API error: {}", err),
    }

    teloxide::commands_repl(bot,
        move |msg, cmd: notifier::Command| async move {
            notifier::handler(msg, cmd, &exchange).await;
        }
    ).await;

    Ok(())
}
