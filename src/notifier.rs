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

/// Все команды бота
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Доступные команды:")]
pub enum Command {
    /// Эта справка
    #[command(description = "показать это сообщение", aliases = ["help", "?"])]
    Help,
    /// Проверить статус
    #[command(description = "показать статус")]
    Status,
    /// Вывести весь баланс кошелька
    #[command(description = "список всего баланса: /wallet")]
    Wallet,
    /// Баланс конкретной монеты
    #[command(description = "баланс монеты: /balance <symbol>")]
    Balance(String),
    /// Хеджировать позицию
    #[command(description = "захеджировать: /hedge <sum> <symbol> <volatility %>")]
    Hedge(String),
    /// Расхеджировать позицию
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
                    InlineKeyboardButton::callback("💼 Баланс", "wallet"),
                ],
                vec![
                    InlineKeyboardButton::callback("🪙 Баланс монеты", "balance"),
                    InlineKeyboardButton::callback("⚙️ Хедж", "hedge"),
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

        Command::Wallet => {
            let list = exchange.get_all_balances().await?;
            let mut text = "💼 Баланс кошелька:\n".to_string();
            for (coin, bal) in list {
                if bal.free > 0.0 || bal.locked > 0.0 {
                    text.push_str(&format!("• {}: free={:.4}, locked={:.4}\n", coin, bal.free, bal.locked));
                }
            }
            bot.send_message(chat_id, text).await?;
        }

        Command::Balance(arg) => {
            let symbol = arg.trim();
            if symbol.is_empty() {
                bot.send_message(chat_id, "Использование: /balance <symbol>").await?;
            } else {
                let coin = symbol.to_uppercase();
                match exchange.get_balance(&coin).await {
                    Ok(bal) => {
                        bot.send_message(
                            chat_id,
                            format!("💰 {}: free={:.4}, locked={:.4}", coin, bal.free, bal.locked),
                        )
                        .await?;
                    }
                    Err(_) => {
                        bot.send_message(chat_id, format!("❌ Баланс {} не найден", coin))
                            .await?;
                    }
                }
            }
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
        let chat_id = q.message.unwrap().chat().id;

        match data.as_str() {
            "status" => {
                bot.send_message(chat_id, "✅ Бот запущен и подключён к бирже").await?;
            }
            "wallet" => {
                let list = exchange.get_all_balances().await?;
                let mut text = "💼 Баланс кошелька:\n".to_string();
                for (coin, bal) in list {
                    if bal.free > 0.0 || bal.locked > 0.0 {
                        text.push_str(&format!("• {}: free={:.4}, locked={:.4}\n", coin, bal.free, bal.locked));
                    }
                }
                bot.send_message(chat_id, text).await?;
            }
            "balance" => {
                bot.send_message(chat_id, "Введите: /balance <symbol>").await?;
            }
            "hedge" => {
                bot.send_message(chat_id, "Введите: /hedge <sum> <symbol> <volatility %>")
                    .await?;
            }
            "unhedge" => {
                bot.send_message(chat_id, "Введите: /unhedge <sum> <symbol>").await?;
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
                "Хеджирование {} USDT {} при V={:.1}%:\n▸ Спот {:.4}\n▸ Фьючерс {:.4}",
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
) -> Result<()> {
    let parts: Vec<_> = args.split_whitespace().collect();
    if parts.len() != 2 {
        bot.send_message(chat_id, "Использование: /unhedge <sum> <symbol>")
            .await?;
        return Ok(());
    }
    let sum: f64 = parts[0].parse().unwrap_or(0.0);
    let symbol = parts[1].to_uppercase();

    bot.send_message(
        chat_id,
        "🚧 Команда /unhedge пока не реализована — следите за обновлениями",
    )
    .await?;
    Ok(())
}