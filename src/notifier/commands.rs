// src/notifier/commands.rs

use crate::exchange::Exchange;
use crate::models::UnhedgeRequest;
// Добавляем UserState и MessageId
use super::{Command, StateStorage, UserState};
use teloxide::prelude::*;
// Добавляем InlineKeyboardButton, InlineKeyboardMarkup, MessageId
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use teloxide::utils::command::BotCommands;

use tracing::{warn, error, info}; // Используем tracing для логов

// Вспомогательная функция для "чистки" чата (дублируется из messages.rs, можно вынести)
async fn cleanup_chat(bot: &Bot, chat_id: ChatId, user_msg_id: MessageId, bot_msg_id: Option<i32>) {
    if let Some(id_int) = bot_msg_id {
        if let Err(e) = bot.delete_message(chat_id, MessageId(id_int)).await {
            warn!("Failed to delete previous bot message {}: {}", id_int, e);
        }
    }
    if let Err(e) = bot.delete_message(chat_id, user_msg_id).await {
        warn!("Failed to delete user command message {}: {}", user_msg_id, e);
    }
}

// Основной обработчик команд
pub async fn handle_command<E>(
    bot: Bot,
    msg: Message,
    cmd: Command,
    mut exchange: E, // Делаем mut, т.к. check_connection требует &mut
    state_storage: StateStorage,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let message_id = msg.id; // ID сообщения с командой

    // --- Сброс состояния и удаление предыдущего сообщения бота при новой команде ---
    let mut previous_bot_message_id: Option<i32> = None;
    {
        let mut state_guard = state_storage
            .write()
            .expect("Failed to acquire write lock on state storage");
        // Получаем ID предыдущего сообщения бота, если оно было сохранено в состоянии
        if let Some(old_state) = state_guard.get(&chat_id) {
            previous_bot_message_id = match old_state {
                UserState::AwaitingAssetSelection { last_bot_message_id } => *last_bot_message_id,
                UserState::AwaitingSum { last_bot_message_id, .. } => *last_bot_message_id,
                UserState::AwaitingVolatility { last_bot_message_id, .. } => *last_bot_message_id,
                UserState::AwaitingUnhedgeQuantity { last_bot_message_id, .. } => *last_bot_message_id,
                UserState::None => None,
            };
        }
        // Сбрасываем состояние, если оно не None
        if !matches!(state_guard.get(&chat_id), Some(UserState::None) | None) {
             info!("Resetting user state for {} due to new command: {:?}", chat_id, cmd);
             state_guard.insert(chat_id, UserState::None);
        }
    }

    // Удаляем сообщение пользователя с командой и предыдущее сообщение бота (если было)
    cleanup_chat(&bot, chat_id, message_id, previous_bot_message_id).await;
    // --- Конец сброса состояния и очистки ---


    match cmd {
        Command::Help => {
            // Сообщение пользователя уже удалено
            let kb = InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("✅ Статус", "status"),
                    InlineKeyboardButton::callback("💼 Баланс", "wallet"),
                ],
                vec![
                    InlineKeyboardButton::callback("🪙 Баланс монеты", "balance"),
                    InlineKeyboardButton::callback("⚙️ Хедж", "hedge"), // Запускает выбор актива
                    InlineKeyboardButton::callback("🛠 Расхедж", "unhedge"), // Запускает выбор актива
                    InlineKeyboardButton::callback("📈 Funding", "funding"),
                ],
            ]);
            bot.send_message(chat_id, Command::descriptions().to_string())
                .reply_markup(kb)
                .await?;
        }

        Command::Status => {
            // Сообщение пользователя уже удалено
            match exchange.check_connection().await { // Вызываем на mut exchange
                Ok(_) => {
                    bot.send_message(chat_id, "✅ Бот запущен и успешно подключен к бирже.").await?;
                }
                Err(e) => {
                     bot.send_message(chat_id, format!("⚠️ Бот запущен, но есть проблема с подключением к бирже: {}", e)).await?;
                }
            }
        }
        Command::Wallet => {
            // Сообщение пользователя уже удалено
             info!("Fetching wallet balance for chat_id: {}", chat_id);
            match exchange.get_all_balances().await {
                Ok(balances) => {
                    let mut text = "💼 Баланс кошелька:\n".to_string();
                    let mut found_assets = false;
                    let mut sorted_balances: Vec<_> = balances.into_iter().collect();
                    sorted_balances.sort_by_key(|(coin, _)| coin.clone());

                    for (coin, bal) in sorted_balances {
                        if bal.free > 1e-8 || bal.locked > 1e-8 {
                            text.push_str(&format!(
                                "• {}: ️free {:.4}, locked {:.4}\n",
                                coin, bal.free, bal.locked
                            ));
                            found_assets = true;
                        }
                    }
                    if !found_assets {
                        text = "ℹ️ Ваш кошелек пуст.".to_string();
                    }
                    bot.send_message(chat_id, text).await?;
                }
                Err(e) => {
                    error!("Failed to fetch wallet balance for chat_id: {}: {}", chat_id, e);
                    bot.send_message(chat_id, format!("❌ Не удалось получить баланс кошелька: {}", e)).await?;
                }
            }
        }
        Command::Balance(arg) => {
            // Сообщение пользователя уже удалено
            let sym = arg.trim().to_uppercase();
            if sym.is_empty() {
                bot.send_message(chat_id, "Использование: /balance <SYMBOL>").await?;
            } else {
                info!("Fetching balance for {} for chat_id: {}", sym, chat_id);
                match exchange.get_balance(&sym).await {
                    Ok(b) => {
                        bot.send_message(
                            chat_id,
                            format!("💰 {}: free {:.4}, locked {:.4}", sym, b.free, b.locked),
                        ).await?;
                    }
                    Err(e) => {
                         error!("Failed to fetch balance for {} for chat_id: {}: {}", sym, chat_id, e);
                        bot.send_message(chat_id, format!("❌ Не удалось получить баланс {}: {}", sym, e)).await?;
                    }
                }
            }
        }

        Command::Hedge(arg) => {
            // Сообщение пользователя уже удалено
            let symbol = arg.trim().to_uppercase();
            if symbol.is_empty() {
                bot.send_message(chat_id, "Использование: /hedge <SYMBOL>\nИли используйте кнопку 'Хедж' из /help.").await?;
            } else {
                // Начинаем диалог запроса суммы
                info!("Starting hedge dialog via command for chat_id: {}, symbol: {}", chat_id, symbol);
                let kb = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),
                ]]);
                let bot_msg = bot.send_message(
                    chat_id,
                    format!("Введите сумму USDT для хеджирования {}:", symbol),
                )
                .reply_markup(kb)
                .await?;
                // Устанавливаем состояние
                {
                    let mut state = state_storage
                        .write()
                        .expect("Failed to acquire write lock on state storage");
                    state.insert(chat_id, UserState::AwaitingSum {
                        symbol: symbol.clone(),
                        last_bot_message_id: Some(bot_msg.id.0), // Сохраняем ID сообщения бота
                    });
                    info!("User state for {} set to AwaitingSum for symbol {}", chat_id, symbol);
                }
            }
        }

        Command::Unhedge(arg) => {
            // Сообщение пользователя уже удалено
            let parts: Vec<_> = arg.split_whitespace().collect();
             if parts.len() != 2 {
                bot.send_message(chat_id, "Использование: /unhedge <QUANTITY> <SYMBOL>").await?;
                 return Ok(());
            }
            let quantity_res = parts[0].parse::<f64>();
            let sym = parts[1].to_uppercase();
            match quantity_res {
                Ok(quantity) if quantity > 0.0 => {
                    // Выполняем расхеджирование напрямую, без диалога
                    info!("Processing /unhedge command for chat_id: {}, quantity: {}, symbol: {}", chat_id, quantity, sym);
                    // TODO: Вынести параметры slippage, commission, max_wait в конфиг
                    let hedger = crate::hedger::Hedger::new(exchange.clone(), 0.005, 0.001, 30);
                    let waiting_msg = bot.send_message(chat_id, format!("⏳ Запускаю расхеджирование {} {}...", quantity, sym)).await?;
                    match hedger.run_unhedge(UnhedgeRequest {
                        sum: quantity, // В UnhedgeRequest 'sum' используется для quantity
                        symbol: sym.clone(),
                    }).await { // Пока без колбэка
                        Ok((sold, bought)) => {
                            info!("Unhedge successful for chat_id: {}. Sold spot: {}, Bought fut: {}", chat_id, sold, bought);
                            bot.edit_message_text(
                                waiting_msg.chat.id,
                                waiting_msg.id,
                                format!(
                                    "✅ Расхеджирование {} {} завершено:\n\n🟢 Продано спота: {:.6}\n🔴 Куплено фьюча: {:.6}",
                                    quantity, sym, sold, bought,
                                )
                            ).await?;
                        }
                        Err(e) => {
                            error!("Unhedge failed for chat_id: {}: {}", chat_id, e);
                            bot.edit_message_text(
                                waiting_msg.chat.id,
                                waiting_msg.id,
                                format!("❌ Ошибка расхеджирования {}: {}", sym, e)
                            ).await?;
                        }
                    }
                }
                 _ => {
                    bot.send_message(chat_id, "⚠️ Неверный формат количества. Должно быть положительное число.").await?;
                }
            }
        }

         Command::Funding(arg) => {
            // Сообщение пользователя уже удалено
            let parts: Vec<_> = arg.split_whitespace().collect();
            if parts.is_empty() {
                bot.send_message(chat_id, "Использование: /funding <SYMBOL> [days]").await?;
            } else {
                let sym = parts[0].to_uppercase();
                let days_u32 = parts.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(30);
                if days_u32 == 0 {
                     bot.send_message(chat_id, "⚠️ Количество дней должно быть больше нуля.").await?;
                     return Ok(());
                }
                let days_u16 = days_u32 as u16;

                info!("Fetching funding rate for {} ({} days) for chat_id: {}", sym, days_u16, chat_id);
                match exchange.get_funding_rate(&sym, days_u16).await {
                    Ok(rate) => {
                        bot.send_message(chat_id, format!(
                            "📈 Средняя ставка финансирования {} за {} дн: {:.4}%",
                            sym, days_u16, rate * 100.0,
                        )).await?;
                    }
                    Err(e) => {
                         error!("Failed to fetch funding rate for {} for chat_id: {}: {}", sym, chat_id, e);
                        bot.send_message(chat_id, format!("❌ Не удалось получить ставку финансирования {}: {}", sym, e)).await?;
                    }
                }
            }
        }
    }
    Ok(())
}
