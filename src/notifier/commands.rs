use crate::exchange::Exchange;
use crate::notifier::{Command, UserState, StateStorage};
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

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
            bot.send_message(chat_id, "✅ Бот запущен и подключен к бирже")
                .await?;
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
        _ => {}
    }

    Ok(())
}