use crate::exchange::Exchange;
use super::{UserState, StateStorage};
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup};

pub async fn handle_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: E,
    state_storage: StateStorage,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let Some(data) = q.data {
        let message = q.message.as_ref().expect("Callback query without message");
        let chat_id = message.chat().id;
        let message_id = message.id();

        match data.as_str() {
            "status" => {
                bot.edit_message_text(chat_id, message_id, "✅ Бот запущен и подключён к бирже").await?;
            }

            "wallet" => {
                let balances = exchange.get_all_balances().await?;
                let mut text = "💼 Баланс кошелька:\n".to_string();
                for (coin, bal) in balances {
                    if bal.free > 0.0 || bal.locked > 0.0 {
                        text.push_str(&format!("• {}: free={:.4}, locked={:.4}\n", coin, bal.free, bal.locked));
                    }
                }
                bot.edit_message_text(chat_id, message_id, text).await?;
            }

            "balance" => {
                bot.edit_message_text(chat_id, message_id, "Введите: /balance <symbol>").await?;
            }

            "funding" => {
                bot.edit_message_text(chat_id, message_id, "Введите: /funding <symbol> [days]").await?;
            }

            "hedge" | "unhedge" => {
                let action = if data == "hedge" { "хеджирования" } else { "расхеджирования" };
                let balances = exchange.get_all_balances().await?;
                let mut buttons = vec![];

                for (coin, bal) in balances {
                    if bal.free > 0.0 || bal.locked > 0.0 {
                        buttons.push(vec![InlineKeyboardButton::callback(
                            format!("🪙 {} (free: {:.4}, locked: {:.4})", coin, bal.free, bal.locked),
                            format!("{}_{}", data, coin),
                        )]);
                    }
                }

                buttons.push(vec![InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge")]);

                let kb = InlineKeyboardMarkup::new(buttons);
                bot.edit_message_text(chat_id, message_id, format!("Выберите актив для {}:", action))
                    .reply_markup(kb)
                    .await?;

                let mut state = state_storage.write().unwrap();
                state.insert(chat_id, UserState::AwaitingAssetSelection {
                    last_bot_message_id: Some(message_id.0),
                });
            }

            "cancel_hedge" => {
                {
                    let mut state = state_storage.write().unwrap();
                    state.insert(chat_id, UserState::None);
                }

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

                bot.edit_message_text(chat_id, message_id, "Действие отменено.")
                    .reply_markup(kb)
                    .await?;
            }

            _ if data.starts_with("hedge_") || data.starts_with("unhedge_") => {
                let sym = data.split('_').nth(1).unwrap_or_default();
                let action = if data.starts_with("hedge_") { "хеджирования" } else { "расхеджирования" };

                {
                    let mut state = state_storage.write().unwrap();
                    state.insert(chat_id, UserState::AwaitingSum {
                        symbol: sym.to_string(),
                        last_bot_message_id: Some(message_id.0),
                    });
                }

                let kb = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),
                ]]);

                bot.edit_message_text(chat_id, message_id, format!("Введите сумму для {} {}:", action, sym))
                    .reply_markup(kb)
                    .await?;
            }

            _ => {}
        }

        bot.answer_callback_query(q.id).await?;
    }

    Ok(())
}
