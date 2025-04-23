// src/notifier/callbacks.rs

use crate::config::Config;
// --- ИЗМЕНЕНО: Убираем лишние use, исправляем импорт TOLERANCE ---
use crate::exchange::{Exchange, OrderSide};
use super::{UserState, StateStorage, RunningHedges}; // Убрали RunningHedgeInfo
// Импортируем константу из hedger
use crate::hedger::ORDER_FILL_TOLERANCE;
// Убрали TokioMutex и Arc
// --- Конец изменений ---
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use tracing::{info, warn, error};

pub async fn handle_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    mut exchange: E,
    state_storage: StateStorage,
    cfg: Config,
    running_hedges: RunningHedges,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let Some(data) = q.data {
        let message = q.message.as_ref().expect("Callback query without message");
        let chat_id = message.chat().id;
        let message_id = message.id(); // message_id() возвращает MessageId

        let callback_id = q.id.clone();
        let _ = bot.answer_callback_query(callback_id.clone()).await;

        let parts: Vec<&str> = data.splitn(2, '_').collect();
        let action = parts.get(0).unwrap_or(&"");
        let payload = parts.get(1).map(|s| s.to_string());

        match *action { // Сравнение &str с &str должно работать
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
            "wallet" => {
                info!("Fetching wallet balance via callback for chat_id: {}", chat_id);
                let balance_result = exchange.get_all_balances().await;

                match balance_result {
                    Ok(balances) => {
                        let mut text = "💼 Баланс кошелька:\n".to_string();
                        let mut found_assets = false;
                        let mut sorted_balances: Vec<_> = balances.into_iter().collect();
                        sorted_balances.sort_by_key(|(coin, _)| coin.clone());

                        for (coin, bal) in sorted_balances {
                            // Используем ORDER_FILL_TOLERANCE для сравнения
                            if bal.free > ORDER_FILL_TOLERANCE || bal.locked > ORDER_FILL_TOLERANCE {
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
            "balance" => {
                let _ = bot.edit_message_text(chat_id, message_id, "Введите: /balance <SYMBOL>").await;
            }
            "funding" => {
                let _ = bot.edit_message_text(chat_id, message_id, "Введите: /funding <SYMBOL> [days]").await;
            }

            "hedge" | "unhedge" if payload.is_none() => {
                let action_text = if *action == "hedge" { "хеджирования" } else { "расхеджирования" };
                info!("Showing asset selection for '{}' for chat_id: {}", action_text, chat_id);

                let mut buttons: Option<Vec<Vec<InlineKeyboardButton>>> = None;
                let mut error_message: Option<String> = None;
                let mut should_set_state = false;

                match exchange.get_all_balances().await {
                    Ok(balances) => {
                        let mut btn_list = vec![];
                        let mut sorted_balances: Vec<_> = balances.into_iter().collect();
                        sorted_balances.sort_by_key(|(coin, _)| coin.clone());

                        for (coin, bal) in sorted_balances {
                            // Используем ORDER_FILL_TOLERANCE для сравнения
                            if bal.free > ORDER_FILL_TOLERANCE || bal.locked > ORDER_FILL_TOLERANCE || coin == cfg.quote_currency {
                                let callback_data = format!("{}_{}", action, coin);
                                btn_list.push(vec![InlineKeyboardButton::callback(
                                    format!("🪙 {} (free: {:.4}, locked: {:.4})", coin, bal.free, bal.locked),
                                    callback_data,
                                )]);
                            }
                        }

                        if btn_list.is_empty() {
                             error_message = Some("ℹ️ Нет доступных активов для выбора.".to_string());
                             let mut state = state_storage.write().expect("Failed to acquire write lock on state storage");
                             state.insert(chat_id, UserState::None);
                        } else {
                            btn_list.push(vec![InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge")]);
                            buttons = Some(btn_list);
                            should_set_state = true;
                        }
                    }
                    Err(e) => {
                        warn!("Failed to fetch balances for asset selection for chat_id: {}: {}", chat_id, e);
                        error_message = Some(format!("❌ Не удалось получить список активов: {}", e));
                        let mut state = state_storage.write().expect("Failed to acquire write lock on state storage");
                        state.insert(chat_id, UserState::None);
                    }
                }

                if let Some(btns) = buttons {
                    let kb = InlineKeyboardMarkup::new(btns);
                    if let Err(e) = bot.edit_message_text(chat_id, message_id, format!("Выберите актив для {}:", action_text))
                        .reply_markup(kb)
                        .await {
                            warn!("Failed to edit message for asset selection: {}", e);
                            should_set_state = false;
                            let mut state = state_storage.write().expect("Failed to acquire write lock on state storage");
                            state.insert(chat_id, UserState::None);
                        }
                } else if let Some(err_msg) = error_message {
                     if let Err(e) = bot.edit_message_text(chat_id, message_id, err_msg).await {
                         warn!("Failed to edit message with error for asset selection: {}", e);
                     }
                }

                if should_set_state {
                    let mut state = state_storage.write().expect("Failed to acquire write lock on state storage");
                    state.insert(chat_id, UserState::AwaitingAssetSelection {
                        last_bot_message_id: Some(message_id.0),
                    });
                    info!("User state for {} set to AwaitingAssetSelection", chat_id);
                }
            }

            // --- Отмена текущего диалога (ДО старта хеджирования) ---
            "cancel" if payload.as_deref() == Some("hedge") => {
                info!("User {} cancelled dialog.", chat_id);
                let reset_state_successful = {
                    let mut state = state_storage.write().expect("Failed to acquire write lock on state storage");
                    state.insert(chat_id, UserState::None);
                    true
                };

                if reset_state_successful {
                    let _ = bot.edit_message_text(chat_id, message_id, "Действие отменено.")
                        .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                        .await;
                }
            }

            // --- Отмена ЗАПУЩЕННОГО хеджирования ---
            // --- ИЗМЕНЕНО: Убираем redundant action == "cancel" ---
            "cancel" if payload.is_some() && data.starts_with("cancel_hedge_active_") => {
            // --- Конец изменений ---
                let symbol = match payload {
                    Some(p) => p.strip_prefix("hedge_active_").unwrap_or("").to_string(),
                    None => "".to_string(),
                };

                if symbol.is_empty() {
                    error!("Could not extract symbol from cancel_hedge_active callback data: {}", data);
                    let _ = bot.answer_callback_query(q.id).text("Ошибка: не удалось извлечь символ.").await;
                    return Ok(());
                }

                info!("User {} requested cancellation of active hedge for symbol: {}", chat_id, symbol);

                let mut hedge_info_opt = None;
                let mut current_order_id_to_cancel: Option<String> = None;
                let mut filled_qty_to_sell: f64 = 0.0;

                {
                    let mut hedges_guard = running_hedges.lock().await;
                    if let Some(info) = hedges_guard.remove(&(chat_id, symbol.clone())) {
                        current_order_id_to_cancel = info.current_order_id.lock().await.clone();
                        filled_qty_to_sell = *info.total_filled_qty.lock().await;
                        hedge_info_opt = Some(info);
                    }
                }

                if let Some(hedge_info) = hedge_info_opt {
                    info!("Found active hedge task for chat_id: {}, symbol: {}. Aborting task...", chat_id, symbol);
                    hedge_info.handle.abort();

                    let _ = bot.edit_message_text(chat_id, MessageId(hedge_info.bot_message_id), format!("⏳ Отменяю хеджирование {}...", symbol))
                        .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                        .await;

                    let exchange_clone = exchange.clone();
                    let bot_clone = bot.clone();
                    let cleanup_chat_id = chat_id;
                    let cleanup_message_id = MessageId(hedge_info.bot_message_id);
                    let cleanup_symbol = hedge_info.symbol.clone();

                    tokio::spawn(async move {
                        let mut _cancel_success = false;
                        let mut sell_success = false;
                        let mut sell_attempted = false;

                        if let Some(order_id) = current_order_id_to_cancel {
                            info!("Attempting to cancel exchange order {} for symbol {}", order_id, cleanup_symbol);
                            match exchange_clone.cancel_order(&cleanup_symbol, &order_id).await {
                                Ok(_) => {
                                    info!("Exchange order {} cancellation request sent successfully (or order was inactive).", order_id);
                                    _cancel_success = true;
                                }
                                Err(e) => {
                                    warn!("Failed to send cancellation request for order {}: {}", order_id, e);
                                }
                            }
                        } else {
                            info!("No active order ID found to cancel for symbol {}", cleanup_symbol);
                            _cancel_success = true;
                        }

                        if filled_qty_to_sell > ORDER_FILL_TOLERANCE {
                            sell_attempted = true;
                            info!("Attempting to sell filled quantity {} of {} by market", filled_qty_to_sell, cleanup_symbol);
                            // ПРИМЕЧАНИЕ: place_market_order сейчас для фьючей. Нужен метод для спота.
                            match exchange_clone.place_market_order(&cleanup_symbol, OrderSide::Sell, filled_qty_to_sell).await {
                                Ok(order) => {
                                    info!("Market sell order placed successfully for filled quantity: id={}", order.id);
                                    sell_success = true;
                                }
                                Err(e) => {
                                    error!("Failed to place market sell order for filled quantity {}: {}", cleanup_symbol, e);
                                    // --- ЗАМЕТКА: Обработка ошибок очистки ---
                                }
                            }
                        } else {
                            info!("No significant quantity filled ({}), skipping market sell.", filled_qty_to_sell);
                        }

                        let final_text = if sell_attempted {
                            format!(
                                "❌ Хеджирование {} отменено.\nПопытка продать {:.6} {} {}",
                                cleanup_symbol,
                                filled_qty_to_sell,
                                cleanup_symbol,
                                if sell_success { "успешно." } else { "не удалась (или функция не реализована)." }
                            )
                        } else {
                            format!("❌ Хеджирование {} отменено. Продажа не требовалась.", cleanup_symbol)
                        };

                        if let Err(e) = bot_clone.edit_message_text(cleanup_chat_id, cleanup_message_id, final_text).await {
                             warn!("Failed to edit final cancellation message: {}", e);
                        }
                    });

                } else {
                    warn!("No active hedge task found for chat_id: {}, symbol: {} upon cancellation request.", chat_id, symbol);
                    let _ = bot.edit_message_text(chat_id, message_id, format!("ℹ️ Процесс хеджирования для {} уже завершен или не найден.", symbol))
                       .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                       .await;
                }
            }


            // --- Обработка выбора конкретного актива ---
            "hedge" | "unhedge" if payload.is_some() => {
                let sym = payload.expect("Payload checked via is_some");
                info!("User {} selected asset {} for '{}'", chat_id, sym, action);

                let mut next_state: Option<UserState> = None;
                let mut message_text: Option<String> = None;
                let mut should_reset_state = false;

                {
                    let mut state_guard = state_storage.write().expect("Failed to acquire write lock on state storage");

                    if matches!(state_guard.get(&chat_id), Some(UserState::AwaitingAssetSelection { .. })) {
                        if *action == "hedge" {
                            next_state = Some(UserState::AwaitingSum {
                                symbol: sym.clone(),
                                last_bot_message_id: Some(message_id.0),
                            });
                            message_text = Some(format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, sym));
                            info!("User state for {} will be set to AwaitingSum for symbol {}", chat_id, sym);
                        } else if *action == "unhedge" {
                            next_state = Some(UserState::AwaitingUnhedgeQuantity {
                                symbol: sym.clone(),
                                last_bot_message_id: Some(message_id.0),
                            });
                            message_text = Some(format!("Введите количество {} для расхеджирования:", sym));
                            info!("User state for {} will be set to AwaitingUnhedgeQuantity for symbol {}", chat_id, sym);
                        }

                        if let Some(ref state_to_set) = next_state {
                            state_guard.insert(chat_id, state_to_set.clone());
                        } else {
                            warn!("Invalid action type '{}' after asset selection.", action);
                            message_text = Some("⚠️ Произошла внутренняя ошибка. Попробуйте снова.".to_string());
                            should_reset_state = true;
                        }

                    } else {
                        warn!("User {} clicked asset selection button, but state was not AwaitingAssetSelection.", chat_id);
                        message_text = Some("⚠️ Похоже, состояние диалога изменилось. Попробуйте снова.".to_string());
                        should_reset_state = true;
                    }
                }

                if should_reset_state {
                     let mut state_guard = state_storage.write().expect("Failed to acquire write lock on state storage");
                     state_guard.insert(chat_id, UserState::None);
                }

                if let Some(text) = message_text {
                    let kb = if !should_reset_state && next_state.is_some() {
                        Some(InlineKeyboardMarkup::new(vec![vec![
                            InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),
                        ]]))
                    } else {
                        None
                    };

                    let mut edit_request = bot.edit_message_text(chat_id, message_id, text);
                    if let Some(keyboard) = kb {
                        edit_request = edit_request.reply_markup(keyboard);
                    } else {
                         edit_request = edit_request.reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()));
                    }

                    if let Err(e) = edit_request.await {
                         warn!("Failed to edit message after asset selection: {}", e);
                         let mut state_guard = state_storage.write().expect("Failed to acquire write lock on state storage");
                         state_guard.insert(chat_id, UserState::None);
                    }
                }
            }

            // --- Неизвестный callback data ---
            _ => {
                warn!("Received unknown callback data: {}", data);
            }
        }
    }

    Ok(())
}