// src/notifier/messages.rs
use crate::exchange::Exchange;
use super::{UserState, StateStorage};
use teloxide::prelude::*;
use teloxide::types::{Message, MessageId, InlineKeyboardButton, InlineKeyboardMarkup, ChatId};
use tracing::{warn, error, info};
use crate::models::{HedgeRequest, UnhedgeRequest};
use crate::hedger::{Hedger, HedgeParams, HedgeProgressUpdate, HedgeProgressCallback};
use futures::future::FutureExt;

// Вспомогательная функция для "чистки" чата
async fn cleanup_chat(bot: &Bot, chat_id: ChatId, user_msg_id: MessageId, bot_msg_id: Option<i32>) {
    if let Some(id_int) = bot_msg_id {
        if let Err(e) = bot.delete_message(chat_id, MessageId(id_int)).await {
            warn!("Failed to delete previous bot message {}: {}", id_int, e);
        }
    }
    if let Err(e) = bot.delete_message(chat_id, user_msg_id).await {
        warn!("Failed to delete user message {}: {}", user_msg_id, e);
    }
}

// --- ИЗМЕНЕНО: Добавляем quote_currency в аргументы ---
pub async fn handle_message<E>(
    bot: Bot,
    msg: Message,
    state_storage: StateStorage,
    exchange: E,
    quote_currency: String, // <-- Добавлено
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
                if sum <= 0.0 {
                    bot.send_message(chat_id, "⚠️ Сумма должна быть положительной.").await?;
                    return Ok(());
                }

                cleanup_chat(&bot, chat_id, message_id, last_bot_message_id).await;

                let kb = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),
                ]]);

                // --- ИЗМЕНЕНО: Используем quote_currency ---
                let bot_msg = bot.send_message(
                    chat_id,
                    format!("Введите ожидаемую волатильность для хеджирования {} {} (%):", sum, quote_currency), // <-- Изменено
                )
                .reply_markup(kb)
                .await?;
                // --- Конец изменений ---

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
                if vol_raw < 0.0 {
                    bot.send_message(chat_id, "⚠️ Волатильность не может быть отрицательной.").await?;
                    return Ok(());
                }
                let vol = vol_raw / 100.0;

                cleanup_chat(&bot, chat_id, message_id, last_bot_message_id).await;

                {
                    let mut state = state_storage
                        .write()
                        .expect("Failed to acquire write lock on state storage");
                    state.insert(chat_id, UserState::None);
                }

                // TODO: Вынести параметры slippage, commission, max_wait в конфиг
                let hedger = Hedger::new(exchange.clone(), 0.005, 0.001, 30);
                let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: vol };
                info!("Starting hedge calculation for chat_id: {}, request: {:?}", chat_id, hedge_request);

                let hedge_params_result = hedger.calculate_hedge_params(&hedge_request).await;

                let waiting_msg = match hedge_params_result {
                    Ok(ref params) => {
                        // --- ИЗМЕНЕНО: Используем quote_currency ---
                        bot.send_message(
                            chat_id,
                            format!(
                                "⏳ Запускаю хеджирование {} {} ({})... \nТекущая цена: {:.2}\nОжидаемая цена покупки: {:.2}",
                                sum, quote_currency, params.symbol, params.current_spot_price, params.initial_limit_price // <-- Изменено
                            ),
                        ).await?
                        // --- Конец изменений ---
                    }
                    Err(ref e) => {
                        error!("Hedge calculation failed for chat_id: {}: {}", chat_id, e);
                        bot.send_message(chat_id, format!("❌ Ошибка расчета параметров хеджирования: {}", e)).await?
                    }
                };

                if let Ok(params) = hedge_params_result {
                    info!("Hedge calculation successful. Running hedge execution for chat_id: {}", chat_id);

                    let bot_clone = bot.clone();
                    let waiting_msg_id = waiting_msg.id;
                    let initial_sum = sum;
                    let initial_symbol = params.symbol.clone();
                    let symbol_for_callback = initial_symbol.clone();
                    let qc_for_callback = quote_currency.clone(); // Клонируем для колбэка

                    let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
                        let bot = bot_clone.clone();
                        let msg_id = waiting_msg_id;
                        let chat_id = chat_id;
                        let sum = initial_sum;
                        let symbol = symbol_for_callback.clone();
                        let qc = qc_for_callback.clone(); // Используем клонированную quote_currency

                        async move {
                            // --- ИЗМЕНЕНО: Используем quote_currency ---
                            let text = format!(
                                "⏳ Хеджирование {} {} ({}) в процессе...\nТекущая цена: {:.2}\nНовая ожидаемая цена покупки: {:.2} (Ордер переставлен)",
                                sum, qc, symbol, update.current_spot_price, update.new_limit_price // <-- Изменено
                            );
                            // --- Конец изменений ---
                            if let Err(e) = bot.edit_message_text(chat_id, msg_id, text).await {
                                warn!("Failed to edit message during hedge progress update: {}", e);
                            }
                            Ok(())
                        }
                        .boxed()
                    });

                    match hedger.run_hedge(params, progress_callback).await
                    {
                        Ok((spot_qty, fut_qty)) => {
                            info!("Hedge execution successful for chat_id: {}. Spot: {}, Fut: {}", chat_id, spot_qty, fut_qty);
                            // --- ИЗМЕНЕНО: Используем quote_currency ---
                            bot.edit_message_text(
                                chat_id,
                                waiting_msg.id,
                                format!(
                                    "✅ Хеджирование {} {} ({}) при V={:.1}% завершено:\n\n🟢 Спот куплено: {:.6}\n🔴 Фьюч продано: {:.6}",
                                    sum, quote_currency, initial_symbol, vol_raw, spot_qty, fut_qty, // <-- Изменено
                                ),
                            )
                            .await?;
                            // --- Конец изменений ---
                        }
                        Err(e) => {
                            error!("Hedge execution failed for chat_id: {}: {}", chat_id, e);
                             bot.edit_message_text(
                                chat_id,
                                waiting_msg.id,
                                format!("❌ Ошибка выполнения хеджирования: {}", e)
                             ).await?;
                        }
                    }
                }
            } else {
                bot.send_message(chat_id, "⚠️ Неверный формат волатильности. Введите число (например, 60 или 60%).").await?;
            }
        }

        // --- Обработка ввода количества для РАСХЕДЖИРОВАНИЯ ---
        Some(UserState::AwaitingUnhedgeQuantity { symbol, last_bot_message_id }) => {
            if let Ok(quantity) = text.parse::<f64>() {
                if quantity <= 0.0 {
                    bot.send_message(chat_id, "⚠️ Количество должно быть положительным.").await?;
                    return Ok(());
                }

                cleanup_chat(&bot, chat_id, message_id, last_bot_message_id).await;

                {
                    let mut state = state_storage
                        .write()
                        .expect("Failed to acquire write lock on state storage");
                    state.insert(chat_id, UserState::None);
                }

                // TODO: Вынести параметры slippage, commission, max_wait в конфиг
                let hedger = crate::hedger::Hedger::new(exchange.clone(), 0.005, 0.001, 30);
                // --- ИЗМЕНЕНО: Используем quote_currency (хотя здесь не обязательно, но для единообразия) ---
                let waiting_msg = bot.send_message(chat_id, format!("⏳ Запускаю расхеджирование {} {}...", quantity, symbol)).await?;
                // --- Конец изменений ---
                info!("Starting unhedge for chat_id: {}, symbol: {}, quantity: {}", chat_id, symbol, quantity);

                match hedger
                    .run_unhedge(UnhedgeRequest {
                        sum: quantity,
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
                                "✅ Расхеджирование {} {} завершено:\n\n🟢 Продано спота: {:.6}\n🔴 Куплено фьюча: {:.6}",
                                quantity, symbol, sold, bought,
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
            if let Err(e) = bot.delete_message(chat_id, message_id).await {
                warn!("Failed to delete unexpected user message {}: {}", message_id, e);
            }
            if let Some(_bot_msg_id_int) = last_bot_message_id {
                 info!("User {} sent text while AwaitingAssetSelection.", chat_id);
            }
        }

        // --- Нет активного состояния ---
        None | Some(UserState::None) => {
            if let Err(e) = bot.delete_message(chat_id, message_id).await {
                warn!("Failed to delete unexpected user message {}: {}", message_id, e);
            }
            bot.send_message(chat_id, "🤖 Сейчас нет активного диалога. Используйте /help для списка команд.").await?;
        }
    }

    Ok(())
}
