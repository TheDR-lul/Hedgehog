use crate::exchange::Exchange;
use crate::hedger::Hedger;
use crate::models::{HedgeRequest, UnhedgeRequest};
use super::{Command, StateStorage};
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use teloxide::utils::command::BotCommands;

pub async fn handle_command<E>(
    bot: Bot,
    msg: Message,
    cmd: Command,
    exchange: E,
    state_storage: StateStorage,
) -> anyhow::Result<()>
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
            bot.send_message(chat_id, "✅ Бот запущен и подключен к бирже").await?;
        }

        Command::Wallet => {
            let balances = exchange.get_all_balances().await?;
            let mut text = "💼 Баланс кошелька:\n".to_string();
            for (coin, bal) in balances {
                if bal.free > 0.0 || bal.locked > 0.0 {
                    text.push_str(&format!(
                        "• {}: free={:.4}, locked={:.4}\n",
                        coin, bal.free, bal.locked
                    ));
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
                        ).await?;
                    }
                    Err(_) => {
                        bot.send_message(chat_id, format!("❌ Баланса {} нет", sym)).await?;
                    }
                }
            }
        }

        Command::Hedge(arg) => {
            let parts: Vec<_> = arg.split_whitespace().collect();
            if parts.len() != 3 {
                bot.send_message(chat_id, "Использование: /hedge <sum> <symbol> <volatility %>").await?;
            } else {
                let sum: f64 = parts[0].parse().unwrap_or(0.0);
                let sym = parts[1].to_uppercase();
                let vol = parts[2].trim_end_matches('%').parse::<f64>().unwrap_or(0.0) / 100.0;

                let hedger = Hedger::new(exchange.clone(), 0.005, 0.001);
                match hedger.run_hedge(HedgeRequest {
                    sum,
                    symbol: sym.clone(),
                    volatility: vol,
                }).await {
                    Ok((spot, fut)) => {
                        bot.send_message(chat_id, format!(
                            "Хеджирование {} USDT {} при V={:.1}%:\n▸ Спот {:+.4}\n▸ Фьючерс {:+.4}",
                            sum, sym, vol * 100.0, spot, fut,
                        )).await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("❌ Ошибка: {}", e)).await?;
                    }
                }
            }
        }

        Command::Unhedge(arg) => {
            let parts: Vec<_> = arg.split_whitespace().collect();
            if parts.len() != 2 {
                bot.send_message(chat_id, "Использование: /unhedge <sum> <symbol>").await?;
            } else {
                let sum: f64 = parts[0].parse().unwrap_or(0.0);
                let sym = parts[1].to_uppercase();

                let hedger = Hedger::new(exchange.clone(), 0.005, 0.001);
                match hedger.run_unhedge(UnhedgeRequest {
                    sum,
                    symbol: sym.clone(),
                }).await {
                    Ok((sold, bought)) => {
                        bot.send_message(chat_id, format!(
                            "Расхеджирование {} USDT {}:\n▸ Продано спота {:+.4}\n▸ Куплено фьюча {:+.4}",
                            sum, sym, sold, bought,
                        )).await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("❌ Ошибка unhedge: {}", e)).await?;
                    }
                }
            }
        }

        Command::Funding(arg) => {
            let parts: Vec<_> = arg.split_whitespace().collect();
            if parts.is_empty() {
                bot.send_message(chat_id, "Использование: /funding <symbol> [days]").await?;
            } else {
                let sym = parts[0].to_uppercase();
                let days = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
                match exchange.get_funding_rate(&sym, days).await {
                    Ok(rate) => {
                        bot.send_message(chat_id, format!(
                            "Средняя ставка финансирования {} за {} дн: {:.4}%",
                            sym, days, rate * 100.0,
                        )).await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("❌ Ошибка funding: {}", e)).await?;
                    }
                }
            }
        }
    }

    Ok(())
}
