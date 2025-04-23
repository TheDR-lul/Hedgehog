use crate::exchange::Exchange;
use super::{UserState, StateStorage};
use teloxide::prelude::*;
use teloxide::types::{Message, InlineKeyboardButton, InlineKeyboardMarkup};

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

    // Получаем текущее состояние пользователя без блокировки на await
    let user_state = {
        let state_guard = state_storage.read().unwrap();
        state_guard.get(&chat_id).cloned()
    };

    match user_state {
        Some(UserState::AwaitingSum { symbol, .. }) => {
            if let Ok(sum) = text.parse::<f64>() {
                {
                    let mut state = state_storage.write().unwrap();
                    if let Some(user_state) = state.get_mut(&chat_id) {
                        *user_state = UserState::AwaitingVolatility {
                            symbol: symbol.clone(),
                            sum,
                            last_bot_message_id: None,
                        };
                    }
                }

                let kb = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),
                ]]);

                bot.delete_message(chat_id, message_id).await?;
                bot.send_message(
                    chat_id,
                    format!("Введите волатильность для хеджирования {} (%):", symbol),
                )
                .reply_markup(kb)
                .await?;
            } else {
                bot.send_message(chat_id, "Неверный формат суммы. Введите число.").await?;
            }
        }

        Some(UserState::AwaitingVolatility { symbol, sum, .. }) => {
            if let Ok(vol_raw) = text.trim_end_matches('%').parse::<f64>() {
                let vol = vol_raw / 100.0;

                {
                    let mut state = state_storage.write().unwrap();
                    state.insert(chat_id, UserState::None);
                }

                let hedger = crate::hedger::Hedger::new(exchange.clone(), 0.005, 0.001);
                match hedger
                    .run_hedge(crate::models::HedgeRequest {
                        sum,
                        symbol: symbol.clone(),
                        volatility: vol,
                    })
                    .await
                {
                    Ok((spot, fut)) => {
                        bot.send_message(
                            chat_id,
                            format!(
                                "Хеджирование {} USDT {} при V={:.1}%:\n▸ Спот {:+.4}\n▸ Фьючерс {:+.4}",
                                sum, symbol, vol_raw, spot, fut,
                            ),
                        )
                        .await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("❌ Ошибка: {}", e)).await?;
                    }
                }

                bot.delete_message(chat_id, message_id).await?;
            } else {
                bot.send_message(chat_id, "Неверный формат волатильности. Введите число (%).").await?;
            }
        }

        Some(_) => {
            bot.send_message(chat_id, "Ожидается другое действие. Используйте кнопки или введите значение.").await?;
        }

        None => {
            bot.send_message(chat_id, "Сейчас нет активного диалога. Используйте меню.").await?;
        }
    }

    Ok(())
}
