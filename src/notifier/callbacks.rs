use crate::exchange::Exchange;
use crate::notifier::{UserState, StateStorage};
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup};

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
        let chat_id = message.chat.id;
        let message_id = message.id.0; // Преобразуем MessageId в i32

        let mut state = state_storage.write().await;

        match data.as_str() {
            "status" => {
                bot.edit_message_text(chat_id, message_id, "✅ Бот запущен и подключён к бирже")
                    .await?;
            }
            "cancel_hedge" => {
                state.insert(chat_id, UserState::None);

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
            _ if data.starts_with("hedge_") => {
                let sym = data.trim_start_matches("hedge_");
                state.insert(
                    chat_id,
                    UserState::AwaitingSum {
                        symbol: sym.to_string(),
                        last_bot_message_id: None,
                    },
                );

                let kb = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),
                ]]);

                bot.edit_message_text(chat_id, message_id, format!("Введите сумму для хеджирования {}:", sym))
                    .reply_markup(kb)
                    .await?;
            }
            _ => {}
        }

        bot.answer_callback_query(q.id).await?;
    }

    Ok(())
}