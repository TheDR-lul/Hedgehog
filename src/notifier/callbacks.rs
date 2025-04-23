// src/notifier/callbacks.rs

use crate::exchange::Exchange;
use super::{UserState, StateStorage};
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use tracing::{info, warn};

pub async fn handle_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    mut exchange: E, // Added mut for check_connection
    state_storage: StateStorage,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let Some(data) = q.data {
        let message = q.message.as_ref().expect("Callback query without message");
        let chat_id = message.chat().id;
        let message_id = message.id(); // ID of the message with buttons

        // Answer the callback query early to remove the "loading" state
        let callback_id = q.id.clone(); // Clone ID for potential reuse
        let _ = bot.answer_callback_query(callback_id.clone()).await; // Answer early

        match data.as_str() {
            // --- Status ---
            "status" => {
                match exchange.check_connection().await {
                    Ok(_) => {
                        let _ = bot.edit_message_text(chat_id, message_id, "✅ Бот запущен и успешно подключен к бирже.").await;
                    }
                    Err(e) => {
                         let _ = bot.edit_message_text(chat_id, message_id, format!("⚠️ Бот запущен, но есть проблема с подключением к бирже: {}", e)).await;
                    }
                }
            }

            // --- Wallet Balance ---
            "wallet" => {
                info!("Fetching wallet balance via callback for chat_id: {}", chat_id);
                let balance_result = exchange.get_all_balances().await; // Fetch balances first

                // Process result and edit message *after* await
                match balance_result {
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
                        let _ = bot.edit_message_text(chat_id, message_id, text).await;
                    }
                    Err(e) => {
                        warn!("Failed to fetch wallet balance via callback for chat_id: {}: {}", chat_id, e);
                        let _ = bot.edit_message_text(chat_id, message_id, format!("❌ Не удалось получить баланс кошелька: {}", e)).await;
                    }
                }
            }

            // --- Command Hints ---
            "balance" => {
                let _ = bot.edit_message_text(chat_id, message_id, "Введите: /balance <SYMBOL>").await;
            }
            "funding" => {
                let _ = bot.edit_message_text(chat_id, message_id, "Введите: /funding <SYMBOL> [days]").await;
            }

            // --- Asset Selection Request (Hedge or Unhedge) ---
            "hedge" | "unhedge" => {
                let action = if data == "hedge" { "хеджирования" } else { "расхеджирования" };
                info!("Showing asset selection for '{}' for chat_id: {}", action, chat_id);

                // --- Refactored to release lock before await ---
                let mut buttons: Option<Vec<Vec<InlineKeyboardButton>>> = None;
                let mut error_message: Option<String> = None;
                let mut should_set_state = false;

                // Fetch balances first
                match exchange.get_all_balances().await {
                    Ok(balances) => {
                        let mut btn_list = vec![];
                        let mut sorted_balances: Vec<_> = balances.into_iter().collect();
                        sorted_balances.sort_by_key(|(coin, _)| coin.clone());

                        for (coin, bal) in sorted_balances {
                            if bal.free > 1e-8 || bal.locked > 1e-8 {
                                let callback_data = format!("{}_{}", data, coin);
                                btn_list.push(vec![InlineKeyboardButton::callback(
                                    format!("🪙 {} (free: {:.4}, locked: {:.4})", coin, bal.free, bal.locked),
                                    callback_data,
                                )]);
                            }
                        }

                        if btn_list.is_empty() {
                             error_message = Some("ℹ️ Нет доступных активов для выбора.".to_string());
                             // Reset state immediately as there's no await needed
                             let mut state = state_storage.write().expect("Failed to acquire write lock on state storage");
                             state.insert(chat_id, UserState::None);
                        } else {
                            btn_list.push(vec![InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge")]);
                            buttons = Some(btn_list);
                            should_set_state = true; // Mark state to be set after potential await
                        }
                    }
                    Err(e) => {
                        warn!("Failed to fetch balances for asset selection for chat_id: {}: {}", chat_id, e);
                        error_message = Some(format!("❌ Не удалось получить список активов: {}", e));
                        // Reset state immediately
                        let mut state = state_storage.write().expect("Failed to acquire write lock on state storage");
                        state.insert(chat_id, UserState::None);
                    }
                }

                // Perform bot actions *after* fetching balances (and releasing potential locks)
                if let Some(btns) = buttons {
                    let kb = InlineKeyboardMarkup::new(btns);
                    // Perform await *outside* the scope where the lock might be held
                    if let Err(e) = bot.edit_message_text(chat_id, message_id, format!("Выберите актив для {}:", action))
                        .reply_markup(kb)
                        .await {
                            warn!("Failed to edit message for asset selection: {}", e);
                            should_set_state = false; // Don't set state if edit failed
                            // Reset state if edit failed
                            let mut state = state_storage.write().expect("Failed to acquire write lock on state storage");
                            state.insert(chat_id, UserState::None);
                        }
                } else if let Some(err_msg) = error_message {
                     // Perform await *outside* the scope where the lock might be held
                     if let Err(e) = bot.edit_message_text(chat_id, message_id, err_msg).await {
                         warn!("Failed to edit message with error for asset selection: {}", e);
                     }
                }

                // Set state *after* await, only if needed and successful
                if should_set_state {
                    let mut state = state_storage.write().expect("Failed to acquire write lock on state storage");
                    state.insert(chat_id, UserState::AwaitingAssetSelection {
                        last_bot_message_id: Some(message_id.0),
                    });
                    info!("User state for {} set to AwaitingAssetSelection", chat_id);
                }
                // --- End Refactor ---
            }

            // --- Cancel Action ---
            "cancel_hedge" => {
                info!("User {} cancelled action.", chat_id);
                let reset_state_successful = { // Scope for write lock
                    let mut state = state_storage.write().expect("Failed to acquire write lock on state storage");
                    state.insert(chat_id, UserState::None);
                    true // Assume success for now
                };

                if reset_state_successful {
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
                    // Perform await *after* lock is released
                    let _ = bot.edit_message_text(chat_id, message_id, "Действие отменено.")
                        .reply_markup(kb)
                        .await;
                }
            }

            // --- Specific Asset Selected ---
            _ if data.starts_with("hedge_") || data.starts_with("unhedge_") => {
                let parts: Vec<&str> = data.splitn(2, '_').collect();
                if parts.len() == 2 {
                    let action_type = parts[0];
                    let sym = parts[1];

                    info!("User {} selected asset {} for '{}'", chat_id, sym, action_type);

                    // --- Refactored to release lock before await ---
                    let mut next_state: Option<UserState> = None;
                    let mut message_text: Option<String> = None;
                    let mut should_reset_state = false;

                    { // Scope for write lock
                        let mut state_guard = state_storage.write().expect("Failed to acquire write lock on state storage");

                        if matches!(state_guard.get(&chat_id), Some(UserState::AwaitingAssetSelection { .. })) {
                            if action_type == "hedge" {
                                next_state = Some(UserState::AwaitingSum {
                                    symbol: sym.to_string(),
                                    last_bot_message_id: Some(message_id.0),
                                });
                                message_text = Some(format!("Введите сумму USDT для хеджирования {}:", sym));
                                info!("User state for {} will be set to AwaitingSum for symbol {}", chat_id, sym);
                            } else if action_type == "unhedge" {
                                next_state = Some(UserState::AwaitingUnhedgeQuantity {
                                    symbol: sym.to_string(),
                                    last_bot_message_id: Some(message_id.0),
                                });
                                message_text = Some(format!("Введите количество {} для расхеджирования:", sym));
                                info!("User state for {} will be set to AwaitingUnhedgeQuantity for symbol {}", chat_id, sym);
                            }

                            // Update state within the lock if defined
                            if let Some(ref state_to_set) = next_state {
                                state_guard.insert(chat_id, state_to_set.clone());
                            }

                        } else {
                            warn!("User {} clicked asset selection button, but state was not AwaitingAssetSelection.", chat_id);
                            message_text = Some("⚠️ Похоже, состояние диалога изменилось. Попробуйте снова.".to_string());
                            should_reset_state = true; // Mark state to be reset outside the lock
                        }
                    } // Write lock `state_guard` is dropped HERE

                    // Perform actions outside the lock
                    if should_reset_state {
                         let mut state_guard = state_storage.write().expect("Failed to acquire write lock on state storage");
                         state_guard.insert(chat_id, UserState::None);
                    }

                    if let Some(text) = message_text {
                        let kb = if !should_reset_state {
                            Some(InlineKeyboardMarkup::new(vec![vec![
                                InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),
                            ]]))
                        } else {
                            None
                        };

                        let mut edit_request = bot.edit_message_text(chat_id, message_id, text);
                        if let Some(keyboard) = kb {
                            edit_request = edit_request.reply_markup(keyboard);
                        }
                        // Perform await *after* lock is released
                        if let Err(e) = edit_request.await {
                             warn!("Failed to edit message after asset selection: {}", e);
                             // Reset state if edit failed
                             let mut state_guard = state_storage.write().expect("Failed to acquire write lock on state storage");
                             state_guard.insert(chat_id, UserState::None);
                        }
                    }
                    // --- End Refactor ---

                } else {
                    warn!("Received invalid asset selection callback data: {}", data);
                }
            }

            // --- Unknown Callback Data ---
            _ => {
                warn!("Received unknown callback data: {}", data);
                // Answer the callback query with text
                let _ = bot.answer_callback_query(callback_id).text("Неизвестное действие").await;
            }
        }
    }

    Ok(())
}
