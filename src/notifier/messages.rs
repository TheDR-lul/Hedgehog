use crate::exchange::Exchange;
use crate::notifier::{UserState, StateStorage};
use teloxide::prelude::*;
use teloxide::types::Message;

pub async fn handle_message<E>(
    bot: Bot,
    msg: Message,
    state_storage: StateStorage,
    exchange: E,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let message_id = msg.id;
    let text = msg.text().unwrap_or("").trim();

    let mut state = state_storage.write().await;

    if let Some(user_state) = state.get_mut(&chat_id) {
        match user_state.clone() {
            UserState::AwaitingSum { symbol, .. } => {
                if let Ok(sum) = text.parse::<f64>() {
                    *user_state = UserState::AwaitingVolatility {
                        symbol: symbol.clone(),
                        sum,
                        last_bot_message_id: None,
                    };

                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),
                    ]]);

                    bot.delete_message(chat_id, message_id).await?; // Удаляем сообщение пользователя
                    bot.send_message(
                        chat_id,
                        format!("Введите волатильность для хеджирования {} (%):", symbol),
                    )
                    .reply_markup(kb)
                    .await?;
                } else {
                    bot.send_message(chat_id, "Неверный формат суммы. Введите число.")
                        .await?;
                }
            }
            _ => {}
        }
    } else {
        bot.send_message(chat_id, "Сейчас нет активного диалога. Используйте меню.")
            .await?;
    }

    Ok(())
}