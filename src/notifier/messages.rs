use crate::exchange::Exchange;
use super::{UserState, StateStorage};
use teloxide::prelude::*;
use teloxide::types::{Message, MessageId, InlineKeyboardButton, InlineKeyboardMarkup, ChatId};
use tracing::{warn, error, info}; // Используем tracing
use crate::models::{HedgeRequest, UnhedgeRequest}; // Добавили импорты

// Вспомогательная функция для "чистки" чата
async fn cleanup_chat(bot: &Bot, chat_id: ChatId, user_msg_id: MessageId, bot_msg_id: Option<i32>) { // Принимаем i32
    // Удаляем предыдущее сообщение бота (если оно было)
    if let Some(id_int) = bot_msg_id {
        if let Err(e) = bot.delete_message(chat_id, MessageId(id_int)).await { // Конвертируем в MessageId
            warn!("Failed to delete previous bot message {}: {}", id_int, e);
        }
    }
    // Удаляем сообщение пользователя
    if let Err(e) = bot.delete_message(chat_id, user_msg_id).await {
        warn!("Failed to delete user message {}: {}", user_msg_id, e);
    }
}


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
    if text.is_empty() {
        if let Err(e) = bot.delete_message(chat_id, message_id).await {
             warn!("Failed to delete empty user message {}: {}", message_id, e);
        }
        return Ok(());
    }

    let user_state = {
        state_storage
            .read()
            .expect("Failed to acquire read lock on state storage")
            .get(&chat_id)
            .cloned()
    };

    match user_state {
        // --- Обработка ввода суммы для ХЕДЖИРОВАНИЯ ---
        Some(UserState::AwaitingSum { symbol, last_bot_message_id }) => {
            if let Ok(sum) = text.parse::<f64>() {
                // Проверка на положительную сумму
                if sum <= 0.0 {
                    bot.send_message(chat_id, "⚠️ Сумма должна быть положительной.").await?;
                    return Ok(()); // Не удаляем сообщение пользователя
                }

                cleanup_chat(&bot, chat_id, message_id, last_bot_message_id).await;

                let kb = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),
                ]]);

                let bot_msg = bot.send_message(
                    chat_id,
                    format!("Введите ожидаемую волатильность для хеджирования {} (%):", symbol),
                )
                .reply_markup(kb)
                .await?;

                let mut message_to_delete_if_state_changed: Option<MessageId> = None;
                {
                    let mut state = state_storage
                        .write()
                        .expect("Failed to acquire write lock on state storage");

                    if let Some(current_state @ UserState::AwaitingSum { .. }) = state.get_mut(&chat_id) {
                         *current_state = UserState::AwaitingVolatility {
                            symbol: symbol.clone(),
                            sum,
                            last_bot_message_id: Some(bot_msg.id.0),
                        };
                         info!("User state for {} set to AwaitingVolatility for symbol {}", chat_id, symbol);
                    } else {
                         warn!("User state for {} changed unexpectedly while asking for volatility.", chat_id);
                         message_to_delete_if_state_changed = Some(bot_msg.id);
                    }
                }

                if let Some(delete_id) = message_to_delete_if_state_changed {
                    if let Err(e) = bot.delete_message(chat_id, delete_id).await {
                        warn!("Failed to delete obsolete bot message {}: {}", delete_id, e);
                    }
                }

            } else {
                bot.send_message(chat_id, "⚠️ Неверный формат суммы. Введите число (например, 1000 или 1000.5).").await?;
            }
        }

        // --- Обработка ввода волатильности для ХЕДЖИРОВАНИЯ ---
        Some(UserState::AwaitingVolatility { symbol, sum, last_bot_message_id }) => {
            if let Ok(vol_raw) = text.trim_end_matches('%').parse::<f64>() {
                 // Проверка на неотрицательную волатильность
                if vol_raw < 0.0 {
                    bot.send_message(chat_id, "⚠️ Волатильность не может быть отрицательной.").await?;
                    return Ok(()); // Не удаляем сообщение пользователя
                }
                let vol = vol_raw / 100.0;

                cleanup_chat(&bot, chat_id, message_id, last_bot_message_id).await;

                // Сбрасываем состояние ПЕРЕД запуском долгой операции
                {
                    let mut state = state_storage
                        .write()
                        .expect("Failed to acquire write lock on state storage");
                    state.insert(chat_id, UserState::None);
                }

                // Создаем Hedger (параметры все еще хардкод)
                let hedger = crate::hedger::Hedger::new(exchange.clone(), 0.005, 0.001, 30);
                let waiting_msg = bot.send_message(chat_id, "⏳ Запускаю процесс хеджирования...").await?;
                info!("Starting hedge for chat_id: {}, symbol: {}, sum: {}, vol: {}", chat_id, symbol, sum, vol);

                // Выполняем хеджирование
                match hedger
                    .run_hedge(HedgeRequest { // Используем HedgeRequest
                        sum,
                        symbol: symbol.clone(),
                        volatility: vol,
                    })
                    .await
                {
                    Ok((spot, fut)) => {
                        info!("Hedge successful for chat_id: {}. Spot: {}, Fut: {}", chat_id, spot, fut);
                        bot.edit_message_text(
                            chat_id,
                            waiting_msg.id,
                            format!(
                                "✅ Хеджирование {} USDT {} при V={:.1}% завершено:\n\n🟢 Спот: {:.4}\n🔴 Фьючерс: {:.4}",
                                sum, symbol, vol_raw, spot, fut,
                            ),
                        )
                        .await?;
                    }
                    Err(e) => {
                        error!("Hedge failed for chat_id: {}: {}", chat_id, e);
                         bot.edit_message_text(
                            chat_id,
                            waiting_msg.id,
                            format!("❌ Ошибка хеджирования: {}", e)
                         ).await?;
                    }
                }

            } else {
                bot.send_message(chat_id, "⚠️ Неверный формат волатильности. Введите число (например, 60 или 60%).").await?;
            }
        }

        // --- НОВАЯ ВЕТКА: Обработка ввода количества для РАСХЕДЖИРОВАНИЯ ---
        Some(UserState::AwaitingUnhedgeQuantity { symbol, last_bot_message_id }) => {
            if let Ok(quantity) = text.parse::<f64>() {
                 // Проверка на положительное количество
                if quantity <= 0.0 {
                    bot.send_message(chat_id, "⚠️ Количество должно быть положительным.").await?;
                    return Ok(()); // Не удаляем сообщение пользователя
                }

                cleanup_chat(&bot, chat_id, message_id, last_bot_message_id).await;

                // Сбрасываем состояние ПЕРЕД запуском долгой операции
                {
                    let mut state = state_storage
                        .write()
                        .expect("Failed to acquire write lock on state storage");
                    state.insert(chat_id, UserState::None);
                }

                // Создаем Hedger (параметры все еще хардкод)
                let hedger = crate::hedger::Hedger::new(exchange.clone(), 0.005, 0.001, 30);
                let waiting_msg = bot.send_message(chat_id, "⏳ Запускаю процесс расхеджирования...").await?;
                info!("Starting unhedge for chat_id: {}, symbol: {}, quantity: {}", chat_id, symbol, quantity);

                // Выполняем расхеджирование
                match hedger
                    .run_unhedge(UnhedgeRequest { // Используем UnhedgeRequest
                        sum: quantity, // Передаем количество как 'sum' в UnhedgeRequest
                        symbol: symbol.clone(),
                    })
                    .await
                {
                    Ok((sold, bought)) => {
                        info!("Unhedge successful for chat_id: {}. Sold spot: {}, Bought fut: {}", chat_id, sold, bought);
                        bot.edit_message_text(
                            chat_id,
                            waiting_msg.id,
                            format!(
                                "✅ Расхеджирование {} {} завершено:\n\n🟢 Продано спота: {:.4}\n🔴 Куплено фьюча: {:.4}",
                                quantity, symbol, sold, bought, // Используем quantity в тексте
                            ),
                        )
                        .await?;
                    }
                    Err(e) => {
                        error!("Unhedge failed for chat_id: {}: {}", chat_id, e);
                         bot.edit_message_text(
                            chat_id,
                            waiting_msg.id,
                            format!("❌ Ошибка расхеджирования: {}", e)
                         ).await?;
                    }
                }

            } else {
                bot.send_message(chat_id, "⚠️ Неверный формат количества. Введите число (например, 10.5).").await?;
            }
        }


        // --- Обработка других состояний ---
        Some(UserState::AwaitingAssetSelection { last_bot_message_id }) => {
            // Пользователь ввел текст вместо нажатия кнопки выбора актива
            if let Err(e) = bot.delete_message(chat_id, message_id).await {
                warn!("Failed to delete unexpected user message {}: {}", message_id, e);
            }
            // Напоминаем нажать кнопку
            if let Some(bot_msg_id_int) = last_bot_message_id {
                 // Можно отправить временное сообщение или ничего не делать
                 // bot.send_message(chat_id, "Пожалуйста, выберите актив кнопкой выше.").await?;
                 // Или просто игнорируем
                 info!("User {} sent text while AwaitingAssetSelection.", chat_id);
            }
        }

        // --- Нет активного состояния ---
        None | Some(UserState::None) => { // Объединяем None и UserState::None
            if let Err(e) = bot.delete_message(chat_id, message_id).await {
                warn!("Failed to delete unexpected user message {}: {}", message_id, e);
            }
            bot.send_message(chat_id, "🤖 Сейчас нет активного диалога. Используйте /help для списка команд.").await?;
        }
    }

    Ok(())
}
