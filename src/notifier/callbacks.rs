// src/notifier/callbacks.rs

use crate::config::Config;
use crate::exchange::{Exchange, OrderSide};
// --- ДОБАВЛЕНО: Импортируем Db и функции storage ---
use crate::storage::{Db, update_hedge_final_status};
// --- Конец добавления ---
use super::{UserState, StateStorage, RunningHedges};
use crate::hedger::ORDER_FILL_TOLERANCE;
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use tracing::{info, warn, error};

// --- ИЗМЕНЕНО: Принимаем db: &Db ---
pub async fn handle_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: E,
    state_storage: StateStorage,
    cfg: Config,
    running_hedges: RunningHedges,
    db: &Db, // <-- Добавлено
) -> anyhow::Result<()>
// --- Конец изменений ---
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let Some(data) = q.data {
        let message = q.message.as_ref().expect("Callback query without message");
        let chat_id = message.chat().id; // <-- Исправлено
        let message_id = message.id();   // <-- Исправлено

        let callback_id = q.id.clone();
        let _ = bot.answer_callback_query(callback_id.clone()).await;

        let parts: Vec<&str> = data.splitn(2, '_').collect();
        let action = parts.get(0).unwrap_or(&"");
        let payload = parts.get(1).map(|s| s.to_string());

        match *action {
            "status" => {
                let mut exchange_clone_for_check = exchange.clone();
                match exchange_clone_for_check.check_connection().await {
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
            "cancel" if payload.is_some() && data.starts_with("cancel_hedge_active_") => {
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
                let mut reported_filled_qty: f64 = 0.0;
                // --- ДОБАВЛЕНО: Переменная для ID операции ---
                let mut operation_id_to_cancel: Option<i64> = None;
                // --- Конец добавления ---

                {
                    let mut hedges_guard = running_hedges.lock().await;
                    if let Some(info) = hedges_guard.remove(&(chat_id, symbol.clone())) {
                        current_order_id_to_cancel = info.current_order_id.lock().await.clone();
                        reported_filled_qty = *info.total_filled_qty.lock().await;
                        // --- ДОБАВЛЕНО: Получаем ID операции ---
                        operation_id_to_cancel = Some(info.operation_id);
                        // --- Конец добавления ---
                        hedge_info_opt = Some(info); // info перемещается сюда
                    }
                }

                // --- ИЗМЕНЕНО: Проверяем и hedge_info_opt, и operation_id_to_cancel ---
                if let (Some(hedge_info), Some(operation_id)) = (hedge_info_opt, operation_id_to_cancel) {
                // --- Конец изменений ---
                    info!("Found active hedge task for chat_id: {}, symbol: {}, op_id: {}. Aborting task...", chat_id, symbol, operation_id);
                    hedge_info.handle.abort();

                    let _ = bot.edit_message_text(chat_id, MessageId(hedge_info.bot_message_id), format!("⏳ Отменяю хеджирование ID:{} {}...", operation_id, symbol)) // Добавили ID
                        .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                        .await;

                    let exchange_clone = exchange.clone();
                    let bot_clone = bot.clone();
                    let cleanup_chat_id = chat_id;
                    let cleanup_message_id = MessageId(hedge_info.bot_message_id);
                    let cleanup_symbol = hedge_info.symbol.clone();
                    // --- ДОБАВЛЕНО: Клонируем db для задачи ---
                    let db_clone = db.clone();
                    // --- Конец добавления ---

                    tokio::spawn(async move {
                        let mut _cancel_success = false;
                        let mut sell_success = false;
                        let mut sell_attempted = false;
                        let mut qty_to_actually_sell = 0.0;

                        // Шаг 1: Отмена ордера
                        if let Some(order_id) = current_order_id_to_cancel {
                            info!("op_id:{}: Attempting to cancel exchange order {} for symbol {}", operation_id, order_id, cleanup_symbol);
                            match exchange_clone.cancel_order(&cleanup_symbol, &order_id).await {
                                Ok(_) => {
                                    info!("op_id:{}: Exchange order {} cancellation request sent successfully (or order was inactive).", operation_id, order_id);
                                    _cancel_success = true;
                                }
                                Err(e) => {
                                    warn!("op_id:{}: Failed to send cancellation request for order {}: {}", operation_id, order_id, e);
                                }
                            }
                        } else {
                            info!("op_id:{}: No active order ID found to cancel for symbol {}", operation_id, cleanup_symbol);
                            _cancel_success = true;
                        }

                        // Шаг 2: Продажа купленного
                        if reported_filled_qty > ORDER_FILL_TOLERANCE {
                            sell_attempted = true;
                            info!("op_id:{}: Reported filled quantity is {}. Checking available balance...", operation_id, reported_filled_qty);

                            match exchange_clone.get_balance(&cleanup_symbol).await {
                                Ok(balance) => {
                                    let available_balance = balance.free;
                                    info!("op_id:{}: Available balance for {} is {}", operation_id, cleanup_symbol, available_balance);

                                    qty_to_actually_sell = reported_filled_qty.min(available_balance);

                                    if qty_to_actually_sell > ORDER_FILL_TOLERANCE {
                                        info!("op_id:{}: Attempting to sell actual available quantity {} of {} by market", operation_id, qty_to_actually_sell, cleanup_symbol);
                                        match exchange_clone.place_spot_market_order(&cleanup_symbol, OrderSide::Sell, qty_to_actually_sell).await {
                                            Ok(order) => {
                                                info!("op_id:{}: Spot Market sell order placed successfully for actual quantity: id={}", operation_id, order.id);
                                                sell_success = true;
                                            }
                                            Err(e) => {
                                                error!("op_id:{}: Failed to place spot market sell order for actual quantity {}: {}", operation_id, cleanup_symbol, e);
                                            }
                                        }
                                    } else {
                                        warn!("op_id:{}: Actual available balance ({}) is too small to sell, skipping market sell.", operation_id, available_balance);
                                        qty_to_actually_sell = 0.0;
                                    }
                                }
                                Err(e) => {
                                    error!("op_id:{}: Failed to get balance for {} before selling: {}. Skipping market sell.", operation_id, cleanup_symbol, e);
                                    sell_attempted = false;
                                }
                            }

                        } else {
                            info!("op_id:{}: No significant reported quantity filled ({}), skipping balance check and market sell.", operation_id, reported_filled_qty);
                        }

                        // --- ДОБАВЛЕНО: Обновляем статус в БД ---
                        let final_db_status = "Cancelled";
                        let error_msg = if sell_attempted && !sell_success {
                            Some("Failed to sell filled spot quantity after cancellation.")
                        } else {
                            None
                        };
                        if let Err(e) = update_hedge_final_status(
                            &db_clone,
                            operation_id,
                            final_db_status,
                            None, // Нет ID фьючерсного ордера при отмене
                            qty_to_actually_sell, // Записываем, сколько реально продали (или 0)
                            error_msg,
                        ).await {
                             error!("op_id:{}: Failed to update final status in DB after cancellation: {}", operation_id, e);
                        } else {
                             info!("op_id:{}: Updated final status in DB to '{}'", operation_id, final_db_status);
                        }
                        // --- Конец добавления ---


                        // Шаг 3: Финальное сообщение
                        let final_text = if sell_attempted {
                            format!(
                                "❌ Хеджирование ID:{} {} отменено.\nПопытка продать {:.6} {} по рынку {}",
                                operation_id, cleanup_symbol,
                                qty_to_actually_sell,
                                cleanup_symbol,
                                if sell_success { "успешна." } else { "не удалась." }
                            )
                        } else {
                            format!("❌ Хеджирование ID:{} {} отменено. Продажа не требовалась или не удалась.", operation_id, cleanup_symbol)
                        };

                        if let Err(e) = bot_clone.edit_message_text(cleanup_chat_id, cleanup_message_id, final_text).await {
                             warn!("op_id:{}: Failed to edit final cancellation message: {}", operation_id, e);
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
                            // TODO: Переделать логику unhedge для работы с БД
                            // Здесь можно сразу показать список доступных для расхеджирования операций
                            // или перейти в состояние AwaitingUnhedgeSelection { symbol }
                            next_state = Some(UserState::AwaitingUnhedgeQuantity {
                                symbol: sym.clone(),
                                last_bot_message_id: Some(message_id.0),
                            });
                            message_text = Some(format!("Введите количество {} для расхеджирования:\n(Временно, пока не интегрирована БД)", sym));
                            info!("User state for {} set to AwaitingUnhedgeQuantity for symbol {}", chat_id, sym);
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