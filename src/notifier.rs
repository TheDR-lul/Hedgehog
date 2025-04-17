// src/notifier.rs

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;

use crate::exchange::Exchange;

/// Команды Telegram‑бота
#[derive(BotCommands, Clone)]
#[command(rename = "lowercase", description = "Хедж‑бот команды:")]
pub enum Command {
    #[command(description = "показать статус")]
    Status,
    #[command(description = "захеджировать: /hedge <sum> <symbol> <volatility %>")]
    Hedge(String),
    #[command(description = "расхеджировать: /unhedge <sum> <symbol>")]
    Unhedge(String),
}

pub async fn handler<E>(
    cx: UpdateWithCx<AutoSend<Bot>, Message>,
    cmd: Command,
    exchange: &E,
) -> Result<()>
where
    E: Exchange + Send + Sync,
{
    match cmd {
        Command::Status => {
            cx.answer("✅ Бот работает и подключен к бирже").await?;
        }

        Command::Hedge(args) => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            if parts.len() != 3 {
                cx.answer("Использование: /hedge <sum> <symbol> <volatility %>").await?;
                return Ok(());
            }

            // Парсим аргументы
            let sum: f64        = parts[0].parse().unwrap_or(0.0);
            let symbol: String  = parts[1].to_string();
            let volatility: f64 = parts[2]
                .trim_end_matches('%')
                .parse::<f64>()
                .unwrap_or(0.0)
                / 100.0;

            // Запрашиваем поддерживающую маржу (MMR) и считаем объёмы
            let mmr  = exchange.get_mmr(&symbol).await?;
            let spot = sum / ((1.0 + volatility) * (1.0 + mmr));
            let fut  = sum - spot;

            cx.answer(format!(
                "Хеджирование {} USDT {} при V={}:\nСпот: {:.4}\nФьючерс: {:.4}",
                sum, symbol, volatility, spot, fut
            ))
            .await?;
        }

        Command::Unhedge(args) => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            if parts.len() != 2 {
                cx.answer("Использование: /unhedge <sum> <symbol>").await?;
                return Ok(());
            }
            let sum     : f64    = parts[0].parse().unwrap_or(0.0);
            let symbol  : String = parts[1].to_string();

            cx.answer(format!(
                "Команда расхеджирования {} USDT {} пока не реализована",
                sum, symbol
            ))
            .await?;
        }
    }
    Ok(())
}