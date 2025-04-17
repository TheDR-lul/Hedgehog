// src/notifier.rs

use anyhow::Result;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, CallbackQuery},
    utils::command::BotCommands,
};
use crate::exchange::Exchange;
use crate::hedger::Hedger;
use crate::models::{HedgeRequest, UnhedgeRequest};

/// Текстовые команды бота
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Доступные команды:")]
pub enum Command {
    #[command(description = "показать это сообщение", aliases = ["help", "?"])]
    Help,
    #[command(description = "показать статус")]
    Status,
    #[command(description = "захеджировать: /hedge <sum> <symbol> <volatility %>")]
    Hedge(String),
    #[command(description = "расхеджировать: /unhedge <sum> <symbol>")]
    Unhedge(String),
}

/// Обработка текстовых команд
pub async fn handle_command<E>(
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
            let keyboard = InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("✅ Статус", "status"),
                    InlineKeyboardButton::callback("⚙️ Хедж",  "hedge"),
                ],
                vec![
                    InlineKeyboardButton::callback("🛠 Расхедж", "unhedge"),
                ],
            ]);
            bot.send_message(chat_id, Command::descriptions().to_string())
                .reply_markup(keyboard)
                .await?;
        }

        Command::Status => {
            bot.send_message(chat_id, "✅ Бот запущен и подключён к бирже")
                .await?;
        }

        Command::Hedge(args) => {
            do_hedge(&bot, chat_id, args, &exchange).await?;
        }

        Command::Unhedge(args) => {
            do_unhedge(&bot, chat_id, args).await?;
        }
    }

    Ok(())
}

/// Обработка inline‑callback событий
pub async fn handle_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: E,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let Some(data) = q.data {
        // берём ChatId из прикреплённого сообщения
        let chat_id = q
            .message
            .unwrap()
            .chat()
            .id;

        match data.as_str() {
            "status" => {
                bot.send_message(chat_id, "✅ Бот запущен и подключён к бирже")
                    .await?;
            }
            "hedge" => {
                bot.send_message(chat_id, "Введите: /hedge <сумма> <символ> <волатильность %>")
                    .await?;
            }
            "unhedge" => {
                bot.send_message(chat_id, "Введите: /unhedge <сумма> <символ>")
                    .await?;
            }
            _ => {}
        }

        bot.answer_callback_query(q.id).await?;
    }
    Ok(())
}

async fn do_hedge<E>(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    exchange: &E,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let parts: Vec<_> = args.split_whitespace().collect();
    if parts.len() != 3 {
        bot.send_message(chat_id, "Использование: /hedge <sum> <symbol> <volatility %>")
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
            let text = format!(
                "Хеджирование {} USDT {} при V={:.1}%:\n▸ Спот {:.4}\n▸ Фьючерс {:.4}",
                sum,
                symbol,
                volatility * 100.0,
                spot,
                fut,
            );
            bot.send_message(chat_id, text).await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Ошибка: {}", e)).await?;
        }
    }
    Ok(())
}

async fn do_unhedge(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
) -> Result<()>
{
    let parts: Vec<_> = args.split_whitespace().collect();
    if parts.len() != 2 {
        bot.send_message(chat_id, "Использование: /unhedge <sum> <symbol>")
            .await?;
        return Ok(());
    }
    let sum: f64 = parts[0].parse().unwrap_or(0.0);
    let symbol = parts[1].to_uppercase();
    let _ = UnhedgeRequest { sum, symbol };

    bot.send_message(
        chat_id,
        "🚧 Команда /unhedge пока не реализована — следите за обновлениями",
    )
    .await?;
    Ok(())
}
