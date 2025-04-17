// src/notifier.rs

use anyhow::Result;
use teloxide::{prelude::*, utils::command::BotCommands};
use crate::exchange::Exchange;
use crate::hedger::Hedger;
use crate::models::{HedgeRequest, UnhedgeRequest};

/// All of our bot's commands
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Доступные команды:")]
pub enum Command {
    /// This help message
    #[command(description = "показать это сообщение", aliases = ["help", "?"])]
    Help,

    /// Show connection status
    #[command(description = "показать статус")]
    Status,

    /// Hedge a position: /hedge <sum> <symbol> <volatility%>
    #[command(description = "захеджировать: /hedge <sum> <symbol> <volatility %>")]
    Hedge(String),

    /// Unhedge a position: /unhedge <sum> <symbol>
    #[command(description = "расхеджировать: /unhedge <sum> <symbol>")]
    Unhedge(String),
}

pub async fn handler<E>(
    bot: Bot,
    msg: Message,
    cmd: Command,
    exchange: E,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;

    match cmd {
        Command::Help => {
            // send the generated help text
            let text = Command::descriptions().to_string();
            bot.send_message(chat_id, text).await?;
        }

        Command::Status => {
            bot.send_message(chat_id, "✅ Бот запущен и подключён к бирже")
                .await?;
        }

        Command::Hedge(args) => {
            let parts: Vec<_> = args.split_whitespace().collect();
            if parts.len() != 3 {
                bot.send_message(
                    chat_id,
                    "Использование: /hedge <sum> <symbol> <volatility %>",
                )
                .await?;
                return Ok(());
            }
            let sum: f64 = parts[0].parse().unwrap_or(0.0);
            let symbol = parts[1].to_uppercase();
            let volatility: f64 = parts[2]
                .trim_end_matches('%')
                .parse::<f64>()
                .unwrap_or(0.0)
                / 100.0;

            let hedger = Hedger::new(exchange.clone());
            match hedger
                .run_hedge(HedgeRequest { sum, symbol: symbol.clone(), volatility })
                .await
            {
                Ok((spot, fut)) => {
                    let msg = format!(
                        "Хеджирование {} USDT {} при V={:.1}%:\n▸ Спот {:.4}\n▸ Фьючерс {:.4}",
                        sum,
                        symbol,
                        volatility * 100.0,
                        spot,
                        fut,
                    );
                    bot.send_message(chat_id, msg).await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ Ошибка: {}", e)).await?;
                }
            }
        }

        Command::Unhedge(args) => {
            let parts: Vec<_> = args.split_whitespace().collect();
            if parts.len() != 2 {
                bot.send_message(chat_id, "Использование: /unhedge <sum> <symbol>")
                    .await?;
                return Ok(());
            }
            let sum: f64 = parts[0].parse().unwrap_or(0.0);
            let symbol = parts[1].to_uppercase();
            let _req = UnhedgeRequest { sum, symbol };

            bot.send_message(
                chat_id,
                "🚧 Команда /unhedge пока не реализована — следите за обновлениями",
            )
            .await?;
        }
    }

    Ok(())
}