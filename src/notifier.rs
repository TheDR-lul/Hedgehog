// src/notifier.rs

use anyhow::Result;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, CallbackQuery,ChatId},
    utils::command::BotCommands,
};
use crate::exchange::Exchange;
use crate::hedger::Hedger;
use crate::models::{HedgeRequest, UnhedgeRequest};

/// Все команды бота
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Доступные команды:")]
pub enum Command {
    #[command(description = "показать это сообщение", aliases = ["help", "?"])]
    Help,
    #[command(description = "проверить статус")]
    Status,
    #[command(description = "список всего баланса: /wallet")]
    Wallet,
    #[command(description = "баланс монеты: /balance <symbol>")]
    Balance(String),
    #[command(description = "захеджировать: /hedge <sum> <symbol> <volatility %>")]
    Hedge(String),
    #[command(description = "расхеджировать: /unhedge <sum> <symbol>")]
    Unhedge(String),
    #[command(description = "средняя ставка финансирования: /funding <symbol> [days]")]
    Funding(String),
}

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
            let kb = InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("✅ Статус", "status"),
                    InlineKeyboardButton::callback("💼 Баланс", "wallet"),
                ],
                vec![
                    InlineKeyboardButton::callback("🪙 Баланс монеты", "balance"),
                    InlineKeyboardButton::callback("⚙️ Хедж", "hedge"),
                    InlineKeyboardButton::callback("🛠 Расхедж", "unhedge"),
                    InlineKeyboardButton::callback("📈 Funding", "funding"),
                ],
            ]);
            bot.send_message(chat_id, Command::descriptions().to_string())
                .reply_markup(kb)
                .await?;
        }
        Command::Status => {
            bot.send_message(chat_id, "✅ Бот запущен и подключён к бирже")
                .await?;
        }
        Command::Wallet => {
            let list = exchange.get_all_balances().await?;
            let mut text = "💼 Баланс кошелька:\n".to_string();
            for (c, b) in list.into_iter() {
                if b.free > 0.0 || b.locked > 0.0 {
                    text.push_str(&format!("• {}: free={:.4}, locked={:.4}\n", c, b.free, b.locked));
                }
            }
            bot.send_message(chat_id, text).await?;
        }
        Command::Balance(arg) => {
            let sym = arg.trim().to_uppercase();
            if sym.is_empty() {
                bot.send_message(chat_id, "Использование: /balance <symbol>").await?;
            } else {
                match exchange.get_balance(&sym).await {
                    Ok(b) => {
                        bot.send_message(
                            chat_id,
                            format!("💰 {}: free={:.4}, locked={:.4}", sym, b.free, b.locked),
                        )
                        .await?;
                    }
                    Err(_) => {
                        bot.send_message(chat_id, format!("❌ Баланса {} нет", sym))
                            .await?;
                    }
                }
            }
        }
        Command::Hedge(args) => {
            do_hedge(&bot, chat_id, args, &exchange).await?;
        }
        Command::Unhedge(args) => {
            do_unhedge(&bot, chat_id, args, &exchange).await?;
        }
        Command::Funding(arg) => {
            let parts: Vec<_> = arg.split_whitespace().collect();
            if parts.is_empty() {
                bot.send_message(chat_id, "Использование: /funding <symbol> [days]").await?;
            } else {
                let sym = parts[0].to_uppercase();
                let days = parts.get(1).and_then(|d| d.parse().ok()).unwrap_or(30);
                match exchange.get_funding_rate(&sym, days).await {
                    Ok(r) => {
                        bot.send_message(
                            chat_id,
                            format!(
                                "Средняя ставка финансирования {} за {} дн: {:.4}%",
                                sym,
                                days,
                                r * 100.0
                            ),
                        )
                        .await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("❌ Ошибка funding: {}", e))
                            .await?;
                    }
                }
            }
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
    let sym = parts[1].to_uppercase();
    let vol = parts[2].trim_end_matches('%').parse::<f64>().unwrap_or(0.0) / 100.0;

    // slippage 0.5%, commission 0.1%
    let hedger = Hedger::new(exchange.clone(), 0.005, 0.001);
    match hedger.run_hedge(HedgeRequest { sum, symbol: sym.clone(), volatility: vol }).await {
        Ok((spot, fut)) => {
            bot.send_message(
                chat_id,
                format!(
                    "Хеджирование {} USDT {} при V={:.1}%:\n▸ Спот {:+.4}\n▸ Фьючерс {:+.4}",
                    sum,
                    sym,
                    vol * 100.0,
                    spot,
                    fut,
                ),
            )
            .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Ошибка: {}", e)).await?;
        }
    }
    Ok(())
}

async fn do_unhedge<E>(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    exchange: &E, // Используем переданный exchange
) -> Result<()> 
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let parts: Vec<_> = args.split_whitespace().collect();
    if parts.len() != 2 {
        bot.send_message(chat_id, "Использование: /unhedge <sum> <symbol>").await?;
        return Ok(());
    }
    let sum: f64 = parts[0].parse().unwrap_or(0.0);
    let sym = parts[1].to_uppercase();

    // slippage 0.5%, commission 0.1%
    let hedger = Hedger::new(exchange.clone(), 0.005, 0.001); // Используем переданный exchange
    match hedger.run_unhedge(UnhedgeRequest { sum, symbol: sym.clone() }).await {
        Ok((sold, bought)) => {
            bot.send_message(
                chat_id,
                format!(
                    "Расхеджирование {} USDT {}:\n▸ Продано спота {:+.4}\n▸ Куплено фьюча {:+.4}",
                    sum,
                    sym,
                    sold,
                    bought
                ),
            )
            .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Ошибка unhedge: {}", e)).await?;
        }
    }
    Ok(())
}