use anyhow::Result;
use teloxide::{prelude::*, utils::command::BotCommands};

use crate::exchange::Exchange;
use crate::hedger::Hedger;
use crate::models::{HedgeRequest, UnhedgeRequest};

/// Все команды бота
#[derive(BotCommands, Clone)]
#[command(description = "Хедж‑бот команды:")]
pub enum Command {
    /// Показать статус бота и API
    #[command(description = "показать статус")]
    Status,

    /// Захеджировать позицию: /hedge 1000 MNT 60
    #[command(description = "захеджировать: /hedge <sum> <symbol> <volatility %>")]
    Hedge(String),

    /// Расхеджировать позицию: /unhedge 500 MNT
    #[command(description = "расхеджировать: /unhedge <sum> <symbol>")]
    Unhedge(String),
}

/// Центральный обработчик
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
        Command::Status => {
            bot.send_message(chat_id, "✅ Бот запущен и подключён к бирже")
                .await?;
        }

        Command::Hedge(args) => {
            // /hedge <sum> <symbol> <vol%>
            let parts: Vec<&str> = args.split_whitespace().collect();
            if parts.len() != 3 {
                bot.send_message(
                    chat_id,
                    "Использование: /hedge <sum> <symbol> <volatility %>",
                )
                .await?;
                return Ok(());
            }

            // Разбор аргументов
            let sum        : f64    = parts[0].parse().unwrap_or(0.0);
            let symbol     : String = parts[1].to_uppercase();
            let volatility : f64    = parts[2]
                .trim_end_matches('%')
                .parse::<f64>()
                .unwrap_or(0.0) / 100.0;

            // Создаём Hedger на клонированной бирже
            let hedger = Hedger::new(exchange.clone());

            // Расчёт
            match hedger
                .run_hedge(HedgeRequest { sum, symbol: symbol.clone(), volatility })
                .await
            {
                Ok((spot, fut)) => {
                    bot.send_message(
                        chat_id,
                        format!(
                            "Хеджирование {sum} USDT {symbol} при V={:.2}%:\n\
                             ▸ Спот  {spot:.4}\n▸ Фьючерс {fut:.4}",
                            volatility * 100.0
                        ),
                    )
                    .await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ Ошибка хеджирования: {e}"))
                        .await?;
                }
            }
        }

        Command::Unhedge(args) => {
            // /unhedge <sum> <symbol>
            let parts: Vec<&str> = args.split_whitespace().collect();
            if parts.len() != 2 {
                bot.send_message(chat_id, "Использование: /unhedge <sum> <symbol>")
                    .await?;
                return Ok(());
            }

            let sum    : f64    = parts[0].parse().unwrap_or(0.0);
            let symbol : String = parts[1].to_uppercase();

            // Заглушка на будущее (UnhedgeRequest подготовлен)
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