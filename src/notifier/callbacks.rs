// src/notifier/callbacks.rs

use crate::config::Config;
use crate::exchange::{Exchange, types::OrderSide}; // Объединяем импорты
use crate::storage::{
    Db, get_completed_unhedged_ops_for_symbol, get_hedge_operation_by_id,
    update_hedge_final_status, HedgeOperation, // Добавляем HedgeOperation для типа возвращаемого значения
};
use crate::hedger::Hedger;
use super::{UserState, StateStorage, RunningHedges};
use crate::hedger::ORDER_FILL_TOLERANCE;
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use tracing::{info, warn, error};
use chrono::{DateTime, Utc, TimeZone, LocalResult};
use std::sync::Arc; // Для RwLock

// --- КОНСТАНТЫ ДЛЯ CALLBACK DATA ---
const ACTION_STATUS: &str = "status";
const ACTION_WALLET: &str = "wallet";
const ACTION_BALANCE_CMD: &str = "balance"; // Для кнопки "введите команду"
const ACTION_FUNDING_CMD: &str = "funding"; // Для кнопки "введите команду"
const ACTION_HEDGE: &str = "hedge";
const ACTION_UNHEDGE: &str = "unhedge";
const ACTION_CANCEL_DIALOG: &str = "cancel"; // Общая отмена диалога
const ACTION_CANCEL_HEDGE_ACTIVE: &str = "cancel_hedge_active"; // Отмена активного хеджа
const PAYLOAD_CANCEL_HEDGE: &str = "hedge"; // Payload для cancel_hedge
const PREFIX_UNHEDGE_SELECT: &str = "unhedge_select_";
const PREFIX_CANCEL_HEDGE_ACTIVE: &str = "cancel_hedge_active_";
// --- КОНЕЦ КОНСТАНТ ---

// TODO: Функция cleanup_chat определена, но не используется в handle_callback.
// Возможно, она нужна для обработчиков команд или устарела.
#[allow(dead_code)] // Убираем предупреждение о неиспользуемом коде
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

// --- Вспомогательная функция для получения и форматирования балансов ---
async fn get_formatted_balances<E: Exchange>(exchange: &E, quote_currency: &str) -> Result<(String, Vec<(String, f64, f64)>), anyhow::Error> {
    let balances = exchange.get_all_balances().await?;
    let mut text = "💼 Баланс кошелька:\n".to_string();
    let mut found_assets = false;
    let mut sorted_balances: Vec<_> = balances.into_iter().collect();
    sorted_balances.sort_by_key(|(coin, _)| coin.clone());

    let mut asset_data = Vec::new(); // Для возврата данных об активах

    for (coin, bal) in sorted_balances {
        if bal.free > ORDER_FILL_TOLERANCE || bal.locked > ORDER_FILL_TOLERANCE || coin == quote_currency {
            text.push_str(&format!( "• {}: ️free {:.8}, locked {:.8}\n", coin, bal.free, bal.locked ));
            asset_data.push((coin, bal.free, bal.locked)); // Сохраняем данные
            found_assets = true;
        }
    }
    if !found_assets {
        text = "ℹ️ Ваш кошелек пуст.".to_string();
    }
    Ok((text, asset_data))
}
// --- КОНЕЦ Вспомогательной функции ---


pub async fn handle_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: E,
    state_storage: StateStorage,
    cfg: Config,
    running_hedges: RunningHedges,
    db: &Db,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let callback_id = q.id.clone(); // Клонируем ID сразу
    let q_debug_info = format!("{:?}", q); // Для логирования
    // Используем if let для извлечения данных и сообщения
    if let (Some(data), Some(message)) = (q.data, q.message) {
        let chat_id = message.chat().id;
        let message_id = message.id();
        //let callback_id = q.id.clone();

        // Отвечаем на колбэк сразу, чтобы убрать "часики" у пользователя
        // Логируем ошибку, если не удалось ответить
        bot.answer_callback_query(callback_id.clone()) // Используем клонированный ID
            .await
            .map_err(|e| warn!("Failed to answer callback query {}: {}", callback_id, e))
            .ok(); // Игнорируем результат, но логируем ошибку

        // Используем if let для более безопасного парсинга
        let (action, payload) = if let Some((act, pay)) = data.split_once('_') {
            (act, Some(pay.to_string()))
        } else {
            (data.as_str(), None) // Если разделителя нет, вся строка - это action
        };

        // Блокировки RwLock: .expect() запаникует поток, если лок "отравлен".
        // В production можно рассмотреть .unwrap_or_else(|poisoned| poisoned.into_inner())
        // или более сложную обработку ошибок, если паника потока недопустима.
        // Пока оставляем expect для простоты.

        match action {
            ACTION_STATUS => {
                let mut exchange_clone_for_check = exchange.clone();
                let status_text = match exchange_clone_for_check.check_connection().await {
                    Ok(_) => "✅ Бот запущен и успешно подключен к бирже.".to_string(),
                    Err(e) => format!("⚠️ Бот запущен, но есть проблема с подключением к бирже: {}", e),
                };
                bot.edit_message_text(chat_id, message_id, status_text)
                    .await
                    .map_err(|e| warn!("Failed to edit status message: {}", e))
                    .ok();
            }
            ACTION_WALLET => {
                info!("Fetching wallet balance via callback for chat_id: {}", chat_id);
                match get_formatted_balances(&exchange, &cfg.quote_currency).await {
                    Ok((text, _)) => { // Игнорируем данные об активах здесь
                        bot.edit_message_text(chat_id, message_id, text)
                           .await
                           .map_err(|e| warn!("Failed to edit wallet message: {}", e))
                           .ok();
                    }
                    Err(e) => {
                        warn!("Failed to fetch wallet balance via callback: {}", e);
                        bot.edit_message_text(chat_id, message_id, format!("❌ Не удалось получить баланс кошелька: {}", e))
                           .await
                           .map_err(|e| warn!("Failed to edit wallet error message: {}", e))
                           .ok();
                    }
                }
            }
            ACTION_BALANCE_CMD => {
                bot.edit_message_text(chat_id, message_id, "Введите: /balance <SYMBOL>")
                   .await
                   .map_err(|e| warn!("Failed to edit balance cmd hint: {}", e))
                   .ok();
            }
            ACTION_FUNDING_CMD => {
                bot.edit_message_text(chat_id, message_id, "Введите: /funding <SYMBOL> [days]")
                   .await
                   .map_err(|e| warn!("Failed to edit funding cmd hint: {}", e))
                   .ok();
            }

            act @ (ACTION_HEDGE | ACTION_UNHEDGE) if payload.is_none() => {
                 let action_type = act.to_string(); // act здесь будет "hedge" или "unhedge"
                 let action_text = if action_type == ACTION_HEDGE { "хеджирования" } else { "расхеджирования" };
                 info!("Showing asset selection for '{}' via button for chat_id: {}", action_text, chat_id);

                 let mut buttons: Option<Vec<Vec<InlineKeyboardButton>>> = None;
                 let mut error_message: Option<String> = None;
                 let mut should_set_state = false;

                 match get_formatted_balances(&exchange, &cfg.quote_currency).await {
                     Ok((_, asset_data)) => { // Используем данные об активах
                         let mut btn_list = vec![];
                         for (coin, free, locked) in asset_data {
                             // Не показываем quote_currency для расхеджирования
                             if !(action_type == ACTION_UNHEDGE && coin == cfg.quote_currency) {
                                 let callback_data = format!("{}_{}", action_type, coin); // Используем action_type
                                 btn_list.push(vec![InlineKeyboardButton::callback(
                                     format!("🪙 {} (free: {:.8}, locked: {:.8})", coin, free, locked),
                                     callback_data,
                                 )]);
                             }
                         }

                         if btn_list.is_empty() {
                             error_message = Some(format!("ℹ️ Нет доступных активов для {}.", action_text));
                             let mut state = state_storage.write().expect("Lock failed"); // Паника при отравлении
                             state.insert(chat_id, UserState::None);
                         } else {
                             btn_list.push(vec![InlineKeyboardButton::callback("❌ Отмена", format!("{}_{}", ACTION_CANCEL_DIALOG, PAYLOAD_CANCEL_HEDGE))]);
                             buttons = Some(btn_list);
                             should_set_state = true;
                         }
                     }
                     Err(e) => {
                         warn!("Failed fetch balances for asset selection: {}", e);
                         error_message = Some(format!("❌ Не удалось получить список активов: {}", e));
                         let mut state = state_storage.write().expect("Lock failed"); // Паника при отравлении
                         state.insert(chat_id, UserState::None);
                     }
                 }

                 if let Some(btns) = buttons {
                     let kb = InlineKeyboardMarkup::new(btns);
                     if let Err(e) = bot.edit_message_text(chat_id, message_id, format!("Выберите актив для {}:", action_text)).reply_markup(kb).await {
                         warn!("Failed edit message for asset selection: {}", e);
                         should_set_state = false; // Не устанавливаем состояние, если не удалось обновить сообщение
                         let mut state = state_storage.write().expect("Lock failed"); // Паника при отравлении
                         state.insert(chat_id, UserState::None);
                     }
                 } else if let Some(err_msg) = error_message {
                      bot.edit_message_text(chat_id, message_id, err_msg)
                         .await
                         .map_err(|e| warn!("Failed edit error message for asset selection: {}", e))
                         .ok();
                 }

                 // Устанавливаем состояние только если кнопки были успешно показаны
                 if should_set_state {
                     let mut state = state_storage.write().expect("Lock failed"); // Паника при отравлении
                     // Клонируем action_type один раз здесь
                     state.insert(chat_id, UserState::AwaitingAssetSelection { action_type: action_type.clone(), last_bot_message_id: Some(message_id.0), });
                     info!("User state set to AwaitingAssetSelection for '{}'", action_type);
                 }
            }

            // --- Обработка выбора конкретного актива ---
            act @ (ACTION_HEDGE | ACTION_UNHEDGE) if payload.is_some() => {
                // Проверяем, не является ли это выбором операции для расхеджирования
                if data.starts_with(PREFIX_UNHEDGE_SELECT) {
                    // Этот случай обработается ниже
                } else if let Some(sym) = payload { // Используем if let для payload
                    let mut user_state_valid = false;
                    let mut expected_action = String::new();
                    { // Ограничиваем область видимости блокировки чтения
                        let state_guard = state_storage.read().expect("Lock failed"); // Паника при отравлении
                        if let Some(UserState::AwaitingAssetSelection { action_type, .. }) = state_guard.get(&chat_id) {
                            user_state_valid = true;
                            expected_action = action_type.clone();
                        }
                    }

                    if user_state_valid && expected_action == act {
                        info!("User {} selected asset {} for '{}'", chat_id, sym, act);
                        let mut next_state: Option<UserState> = None;
                        let mut message_text: Option<String> = None;
                        let mut keyboard_for_edit: Option<InlineKeyboardMarkup> = None;
                        let mut new_message_with_kb: Option<(String, InlineKeyboardMarkup)> = None;

                        if act == ACTION_HEDGE {
                            next_state = Some(UserState::AwaitingSum { symbol: sym.clone(), last_bot_message_id: Some(message_id.0) });
                            message_text = Some(format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, sym));
                            keyboard_for_edit = Some(InlineKeyboardMarkup::new(vec![vec![
                                InlineKeyboardButton::callback("❌ Отмена", format!("{}_{}", ACTION_CANCEL_DIALOG, PAYLOAD_CANCEL_HEDGE))
                            ]]));
                            info!("User state for {} set to AwaitingSum", chat_id);
                        } else if act == ACTION_UNHEDGE {
                            info!("Looking for hedges for {} after asset selection for chat_id {}", sym, chat_id);
                            match get_completed_unhedged_ops_for_symbol(db, chat_id.0, &sym).await {
                                Ok(operations) => {
                                    if operations.is_empty() {
                                        message_text = Some(format!("ℹ️ Нет завершенных операций для {}.", sym));
                                        next_state = Some(UserState::None);
                                    } else if operations.len() == 1 {
                                        // Безопасно извлекаем единственный элемент
                                        let op_to_unhedge = operations.into_iter().next().unwrap(); // Безопасно, т.к. len == 1
                                        let op_id = op_to_unhedge.id;
                                        info!("Found single op_id: {}. Starting unhedge directly.", op_id);
                                        message_text = Some(format!("⏳ Запускаю расхеджирование ID:{} ({})...", op_id, sym));
                                        // Запускаем расхеджирование в фоне
                                        spawn_unhedge_task(bot.clone(), exchange.clone(), cfg.clone(), db.clone(), chat_id, message_id, op_to_unhedge);
                                        next_state = Some(UserState::None); // Сбрасываем состояние
                                    } else {
                                        info!("Found {} ops for {}. Prompting selection.", operations.len(), sym);
                                        let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
                                        let mut sorted_ops = operations.clone(); // Клонируем для сохранения в состоянии
                                        sorted_ops.sort_by_key(|op| std::cmp::Reverse(op.id)); // Сортируем для показа

                                        for op in &sorted_ops {
                                            // --- УЛУЧШЕННАЯ Обработка LocalResult ---
                                            let timestamp_dt = match Utc.timestamp_opt(op.start_timestamp, 0) {
                                                LocalResult::Single(dt) => dt,
                                                LocalResult::None => {
                                                    warn!("Invalid timestamp {} found for op_id {}", op.start_timestamp, op.id);
                                                    Utc.timestamp_millis_opt(0).unwrap() // Начало эпохи как fallback
                                                }
                                                LocalResult::Ambiguous(min, max) => {
                                                    warn!("Ambiguous timestamp {} found for op_id {} (between {} and {})", op.start_timestamp, op.id, min, max);
                                                    min // Выбираем минимальное возможное время
                                                }
                                            };
                                            // --- КОНЕЦ УЛУЧШЕНИЯ ---
                                            let date_str = timestamp_dt.format("%Y-%m-%d %H:%M").to_string();
                                            let label = format!("ID:{} {:.6} {} ({})", op.id, op.target_futures_qty, op.base_symbol, date_str);
                                            let callback_data = format!("{}{}", PREFIX_UNHEDGE_SELECT, op.id); // Используем префикс
                                            buttons.push(vec![InlineKeyboardButton::callback(label, callback_data)]);
                                        }
                                        buttons.push(vec![InlineKeyboardButton::callback("❌ Отмена", format!("{}_{}", ACTION_CANCEL_DIALOG, PAYLOAD_CANCEL_HEDGE))]);
                                        let kb = InlineKeyboardMarkup::new(buttons);
                                        // Редактируем старое сообщение, убирая кнопки выбора актива
                                        message_text = Some(format!("Найдено несколько операций для {}. Выберите одну из списка ниже:", sym));
                                        // Отправляем новое сообщение с кнопками выбора операции
                                        new_message_with_kb = Some(("Выберите операцию для расхеджирования:".to_string(), kb));
                                        // Сохраняем операции в состоянии
                                        next_state = Some(UserState::AwaitingUnhedgeSelection { symbol: sym.clone(), operations, last_bot_message_id: None }); // ID нового сообщения запишется ниже
                                        info!("Set state AwaitingUnhedgeSelection for {}", chat_id);
                                    }
                                }
                                Err(e) => {
                                    error!("DB query failed for unhedge ops: {}", e);
                                    message_text = Some(format!("❌ Ошибка получения операций из БД: {}", e));
                                    next_state = Some(UserState::None);
                                }
                            }
                        } else {
                            // Эта ветка не должна достигаться из-за `act @ (ACTION_HEDGE | ACTION_UNHEDGE)`
                            warn!("Invalid action type '{}' in asset selection callback", act);
                            message_text = Some("⚠️ Внутренняя ошибка (неверный тип действия).".to_string());
                            next_state = Some(UserState::None);
                        }

                        // Обновляем состояние пользователя
                        { // Ограничиваем область видимости блокировки записи
                            let mut state_guard = state_storage.write().expect("Lock failed"); // Паника при отравлении
                            if let Some(ref state_to_set) = next_state {
                                state_guard.insert(chat_id, state_to_set.clone());
                            }
                        }

                        // Редактируем исходное сообщение (выбор актива)
                        if let Some(text_to_edit) = message_text {
                            // Убираем кнопки, если это не ожидание суммы
                            let final_kb = keyboard_for_edit.unwrap_or_else(|| InlineKeyboardMarkup::new(vec![vec![]]));
                            bot.edit_message_text(chat_id, message_id, text_to_edit)
                               .reply_markup(final_kb)
                               .await
                               .map_err(|e| warn!("Failed to edit message after asset selection: {}", e))
                               .ok();
                        }

                        // Отправляем новое сообщение с кнопками выбора операции, если нужно
                        if let Some((new_text, new_kb)) = new_message_with_kb {
                            match bot.send_message(chat_id, new_text).reply_markup(new_kb).await {
                                Ok(new_msg) => {
                                    // Обновляем ID сообщения в состоянии, если оно AwaitingUnhedgeSelection
                                    let mut state_guard = state_storage.write().expect("Lock failed"); // Паника при отравлении
                                    if let Some(UserState::AwaitingUnhedgeSelection { last_bot_message_id, .. }) = state_guard.get_mut(&chat_id) {
                                        let last_bot_message_id = last_bot_message_id.as_mut();
                                        if let Some(last_bot_message_id) = last_bot_message_id {
                                            *last_bot_message_id = new_msg.id.0;
                                        }
                                        info!("Updated last_bot_message_id for AwaitingUnhedgeSelection to {}", new_msg.id.0);
                                    } else {
                                        warn!("State changed before updating last_bot_message_id for unhedge selection");
                                    }
                                }
                                Err(e) => {
                                    error!("Failed send unhedge selection message: {}", e);
                                    // Сбрасываем состояние при ошибке отправки
                                    let mut state = state_storage.write().expect("Lock failed"); // Паника при отравлении
                                    state.insert(chat_id, UserState::None);
                                }
                            }
                        }
                    } else {
                        warn!("User {} clicked asset {} but state mismatch (expected '{}', action was '{}') or invalid state.", chat_id, sym, expected_action, act);
                        bot.edit_message_text(chat_id, message_id, "⚠️ Состояние изменилось или кнопка устарела. Попробуйте снова.")
                           .reply_markup(InlineKeyboardMarkup::new(vec![vec![]])) // Убираем кнопки
                           .await
                           .map_err(|e| warn!("Failed to edit state mismatch message: {}", e))
                           .ok();
                        // Сбрасываем состояние
                        let mut state = state_storage.write().expect("Lock failed"); // Паника при отравлении
                        state.insert(chat_id, UserState::None);
                    }
                } else {
                    // payload был None, но это не должно происходить из-за `if payload.is_some()` выше
                     warn!("Callback for action '{}' received with None payload unexpectedly.", act);
                }
            }

            // --- Обработка выбора операции для расхеджирования ---
            // Используем starts_with для проверки префикса
            action if action == ACTION_UNHEDGE && data.starts_with(PREFIX_UNHEDGE_SELECT) => {
                // Безопасно извлекаем ID после префикса
                if let Some(op_id_str) = data.strip_prefix(PREFIX_UNHEDGE_SELECT) {
                    if let Ok(op_id) = op_id_str.parse::<i64>() {
                        info!("User {} selected hedge operation ID {} to unhedge.", chat_id, op_id);

                        // Проверяем состояние пользователя (должно быть AwaitingUnhedgeSelection)
                        let mut operation_to_unhedge: Option<HedgeOperation> = None;
                        let mut state_valid = false;
                        { // Ограничиваем область видимости блокировки чтения
                            let state_guard = state_storage.read().expect("Lock failed");
                            if let Some(UserState::AwaitingUnhedgeSelection { operations, .. }) = state_guard.get(&chat_id) {
                                state_valid = true;
                                // Ищем операцию в сохраненном списке
                                operation_to_unhedge = operations.iter().find(|op| op.id == op_id).cloned();
                            }
                        }

                        if state_valid {
                            // Сбрасываем состояние пользователя
                            { let mut state = state_storage.write().expect("Lock failed"); state.insert(chat_id, UserState::None); }

                            match operation_to_unhedge {
                                Some(op) => {
                                    // Дополнительная проверка, что операция принадлежит пользователю и готова к расхеджированию
                                    // (хотя get_completed_unhedged_ops_for_symbol уже должна была это сделать)
                                    if op.chat_id != chat_id.0 || op.status != "Completed" || op.unhedged_op_id.is_some() {
                                        error!("User {} attempted invalid op {} (status: {}, unhedged: {:?})", chat_id, op_id, op.status, op.unhedged_op_id);
                                        bot.edit_message_text(chat_id, message_id, "❌ Выбранная операция недействительна или уже расхеджирована.")
                                           .reply_markup(InlineKeyboardMarkup::new(vec![vec![]])) // Убираем кнопки
                                           .await
                                           .map_err(|e| warn!("Failed edit invalid unhedge op message: {}", e))
                                           .ok();
                                    } else {
                                        let symbol = op.base_symbol.clone();
                                        // Редактируем сообщение с кнопками выбора
                                        bot.edit_message_text(chat_id, message_id, format!("⏳ Запускаю расхеджирование ID:{} ({})...", op_id, symbol))
                                           .reply_markup(InlineKeyboardMarkup::new(vec![vec![]])) // Убираем кнопки
                                           .await
                                           .map_err(|e| warn!("Failed edit starting unhedge message: {}", e))
                                           .ok();
                                        // Запускаем расхеджирование в фоне
                                        spawn_unhedge_task(bot.clone(), exchange.clone(), cfg.clone(), db.clone(), chat_id, message_id, op);
                                    }
                                }
                                None => {
                                    // Операция не найдена в состоянии пользователя - возможно, состояние устарело
                                    error!("Op ID {} not found in user state for chat_id {}", op_id, chat_id);
                                    bot.edit_message_text(chat_id, message_id, "❌ Операция не найдена в текущем выборе или состояние устарело.")
                                       .reply_markup(InlineKeyboardMarkup::new(vec![vec![]]))
                                       .await
                                       .map_err(|e| warn!("Failed edit op not found in state message: {}", e))
                                       .ok();
                                }
                            }
                        } else {
                             warn!("User {} clicked unhedge selection but state was not AwaitingUnhedgeSelection.", chat_id);
                             bot.edit_message_text(chat_id, message_id, "⚠️ Состояние изменилось. Попробуйте выбрать актив снова.")
                                .reply_markup(InlineKeyboardMarkup::new(vec![vec![]]))
                                .await
                                .map_err(|e| warn!("Failed edit state mismatch message for unhedge selection: {}", e))
                                .ok();
                             // Сбрасываем состояние на всякий случай
                             { let mut state = state_storage.write().expect("Lock failed"); state.insert(chat_id, UserState::None); }
                        }
                    } else {
                        error!("Failed to parse op_id from callback data: {}", data);
                        bot.edit_message_text(chat_id, message_id, "❌ Ошибка: Неверный ID операции в кнопке.")
                           .reply_markup(InlineKeyboardMarkup::new(vec![vec![]]))
                           .await
                           .map_err(|e| warn!("Failed edit invalid op_id message: {}", e))
                           .ok();
                    }
                } else {
                     error!("Callback data '{}' starts with '{}' but strip_prefix failed.", data, PREFIX_UNHEDGE_SELECT);
                }
            }


            // --- Обработка отмены диалога/выбора ---
            ACTION_CANCEL_DIALOG if payload.as_deref() == Some(PAYLOAD_CANCEL_HEDGE) => {
                info!("User {} cancelled dialog.", chat_id);
                { let mut state = state_storage.write().expect("Lock failed"); state.insert(chat_id, UserState::None); } // Сбрасываем состояние
                bot.edit_message_text(chat_id, message_id, "Действие отменено.")
                   .reply_markup(InlineKeyboardMarkup::new(vec![vec![]])) // Убираем кнопки
                   .await
                   .map_err(|e| warn!("Failed edit cancel dialog message: {}", e))
                   .ok();
            }

            // --- Отмена ЗАПУЩЕННОГО хеджирования ---
            // Используем starts_with для проверки префикса
            ACTION_CANCEL_HEDGE_ACTIVE if data.starts_with(PREFIX_CANCEL_HEDGE_ACTIVE) => {
                // Безопасно извлекаем символ после префикса
                if let Some(symbol) = data.strip_prefix(PREFIX_CANCEL_HEDGE_ACTIVE) {
                    if symbol.is_empty() {
                        error!("Could not extract symbol from cancel active hedge callback: {}", data);
                        bot.answer_callback_query(callback_id).text("Ошибка: символ не найден.").await.ok();
                        return Ok(());
                    }
                    let symbol = symbol.to_string(); // Преобразуем в String
                    info!("User {} requested cancellation of active hedge for symbol: {}", chat_id, symbol);

                    let mut hedge_info_opt = None;
                    let mut current_order_id_to_cancel: Option<String> = None;
                    let mut reported_filled_qty: f64 = 0.0;
                    let mut operation_id_to_cancel: Option<i64> = None;

                    // Блокируем мьютекс запущенных хеджирований
                    { // Ограничиваем область видимости блокировки
                        let mut hedges_guard = running_hedges.lock().await;
                        // Пытаемся извлечь информацию о хедже
                        if let Some(info) = hedges_guard.remove(&(chat_id, symbol.clone())) {
                            // Блокируем внутренние мьютексы для получения данных
                            current_order_id_to_cancel = info.current_order_id.lock().await.clone();
                            reported_filled_qty = *info.total_filled_qty.lock().await;
                            operation_id_to_cancel = Some(info.operation_id);
                            hedge_info_opt = Some(info); // Сохраняем всю структуру для abort и message_id
                        }
                    } // Мьютекс running_hedges разблокирован

                    if let (Some(hedge_info), Some(operation_id)) = (hedge_info_opt, operation_id_to_cancel) {
                        info!("Found active hedge op_id: {}. Aborting task...", operation_id);
                        hedge_info.handle.abort(); // Прерываем задачу хеджирования

                        // Редактируем сообщение о статусе хеджа
                        bot.edit_message_text(chat_id, MessageId(hedge_info.bot_message_id), format!("⏳ Отменяю хеджирование ID:{} {}...", operation_id, symbol))
                           .reply_markup(InlineKeyboardMarkup::new(vec![vec![]])) // Убираем кнопку отмены
                           .await
                           .map_err(|e| warn!("op_id:{}: Failed edit cancelling message: {}", operation_id, e))
                           .ok();

                        // Запускаем задачу для обработки отмены (отмена ордера, продажа спота, обновление БД)
                        let exchange_clone = exchange.clone();
                        let bot_clone = bot.clone();
                        let cleanup_chat_id = chat_id;
                        let cleanup_message_id = MessageId(hedge_info.bot_message_id);
                        let cleanup_symbol = hedge_info.symbol.clone();
                        let db_clone = db.clone();

                        tokio::spawn(async move {
                            let mut cancel_order_success = false;
                            let mut sell_success = false;
                            let mut sell_attempted = false;
                            let mut qty_to_actually_sell = 0.0;
                            let mut final_error_msg: Option<String> = None;

                            // 1. Попытка отменить ордер на бирже (если он был)
                            if let Some(ref order_id) = current_order_id_to_cancel {
                                info!("op_id:{}: Cancelling futures order {}", operation_id, order_id);
                                match exchange_clone.cancel_order(&cleanup_symbol, order_id).await {
                                    Ok(_) => {
                                        cancel_order_success = true;
                                        info!("op_id:{}: Cancel futures order OK {}", operation_id, order_id);
                                        // ПОТЕНЦИАЛЬНОЕ УЛУЧШЕНИЕ: После успешной отмены можно запросить статус ордера,
                                        // чтобы получить точное исполненное количество, а не полагаться на reported_filled_qty,
                                        // которое могло быть записано до последнего частичного исполнения перед отменой.
                                        // Это усложнит логику, т.к. нужно будет обработать разные статусы ордера.
                                    }
                                    Err(e) => {
                                        warn!("op_id:{}: Cancel futures order FAILED {}: {}", operation_id, order_id, e);
                                        // Ордер мог уже исполниться или быть отмененным ранее.
                                        // Продолжаем, но будем полагаться на reported_filled_qty.
                                        final_error_msg = Some(format!("Failed to cancel futures order {}: {}", order_id, e));
                                    }
                                }
                            } else {
                                // Активного ордера не было (возможно, хедж завершился между кликом и обработкой)
                                cancel_order_success = true; // Считаем успешным, т.к. отменять нечего
                                info!("op_id:{}: No active futures order ID to cancel.", operation_id);
                            }

                            // 2. Продажа исполненного количества на споте (если что-то было исполнено)
                            if reported_filled_qty > ORDER_FILL_TOLERANCE {
                                sell_attempted = true;
                                info!("op_id:{}: Reported filled qty {}. Checking spot balance...", operation_id, reported_filled_qty);
                                match exchange_clone.get_balance(&cleanup_symbol).await {
                                    Ok(balance) => {
                                        // Продаем минимум из того, что было исполнено по отчету, и того, что реально есть на балансе
                                        qty_to_actually_sell = reported_filled_qty.min(balance.free);
                                        info!("op_id:{}: Spot balance free {}, will try to sell {}", operation_id, balance.free, qty_to_actually_sell);
                                        if qty_to_actually_sell > ORDER_FILL_TOLERANCE {
                                            match exchange_clone.place_spot_market_order(&cleanup_symbol, OrderSide::Sell, qty_to_actually_sell).await {
                                                Ok(order) => {
                                                    sell_success = true;
                                                    info!("op_id:{}: Spot Sell OK: order_id={}", operation_id, order.id);
                                                }
                                                Err(e) => {
                                                    error!("op_id:{}: Spot Sell FAILED: {}", operation_id, e);
                                                    // Записываем ошибку продажи
                                                    final_error_msg = Some(format!("Failed to sell filled spot quantity: {}", e));
                                                }
                                            }
                                        } else {
                                            warn!("op_id:{}: Available spot balance ({}) is too small or less than reported filled qty, skipping sell.", operation_id, balance.free);
                                            qty_to_actually_sell = 0.0; // Не продали ничего
                                        }
                                    }
                                    Err(e) => {
                                        error!("op_id:{}: Failed get spot balance for {}: {}. Skipping sell.", operation_id, cleanup_symbol, e);
                                        sell_attempted = false; // Не смогли проверить баланс, не пытаемся продать
                                        final_error_msg = Some(format!("Failed to get spot balance: {}", e));
                                    }
                                }
                            } else {
                                info!("op_id:{}: No filled qty reported ({}), skipping spot sell.", operation_id, reported_filled_qty);
                            }

                            // 3. Обновление статуса операции в БД
                            let final_db_status = "Cancelled";
                            // Используем импортированную функцию
                            if let Err(e) = update_hedge_final_status(&db_clone, operation_id, final_db_status, None, qty_to_actually_sell, final_error_msg.as_deref()).await {
                                error!("op_id:{}: Failed DB update after cancellation: {}", operation_id, e);
                                // Эта ошибка критична, но мы уже не можем откатить действия на бирже. Логируем.
                            } else {
                                info!("op_id:{}: Updated DB status to '{}', sold_spot_qty: {}", operation_id, final_db_status, qty_to_actually_sell);
                            }

                            // 4. Финальное сообщение пользователю
                            let final_text = if sell_attempted {
                                format!(
                                    "❌ Хеджирование ID:{} {} отменено.\nПопытка продать {:.8} {} на споте {}",
                                    operation_id, cleanup_symbol, qty_to_actually_sell, cleanup_symbol,
                                    if sell_success { "успешна." } else { "не удалась." }
                                )
                            } else {
                                format!("❌ Хеджирование ID:{} {} отменено.", operation_id, cleanup_symbol)
                            };
                            if let Err(e) = bot_clone.edit_message_text(cleanup_chat_id, cleanup_message_id, final_text).await {
                                warn!("op_id:{}: Failed edit final cancel message: {}", operation_id, e);
                            }
                        });
                    } else {
                        // Хедж не найден в `running_hedges` - возможно, он уже завершился или был отменен ранее
                        warn!("No active hedge task found for chat_id: {}, symbol: {} during cancellation request.", chat_id, symbol);
                        bot.edit_message_text(chat_id, message_id, format!("ℹ️ Активное хеджирование для {} не найдено (возможно, уже завершено или отменено).", symbol))
                           .reply_markup(InlineKeyboardMarkup::new(vec![vec![]])) // Убираем кнопку
                           .await
                           .map_err(|e| warn!("Failed edit hedge not found message: {}", e))
                           .ok();
                    }
                } else {
                     error!("Callback data '{}' starts with '{}' but strip_prefix failed.", data, PREFIX_CANCEL_HEDGE_ACTIVE);
                }
            }


            // --- Неизвестный callback ---
            _ => {
                warn!("Received unknown callback action: '{}' with payload: {:?}", action, payload);
                // Можно просто проигнорировать или ответить пользователю
                bot.answer_callback_query(q.id).text("Неизвестное действие.").await.ok();
            }
        } // Конец match action

    } else {
        // Случай, когда нет данных или сообщения в CallbackQuery
        warn!("Callback query without data or message received: {}", q_debug_info);

        // Пытаемся ответить на колбэк, если есть ID
        if !callback_id.is_empty() { // Используем клонированный ID
            bot.answer_callback_query(callback_id).await.ok();
        }
    } // Конец if let (Some(data), Some(message))

    Ok(())
} // Конец handle_callback


// --- Вспомогательная функция для запуска задачи расхеджирования ---
fn spawn_unhedge_task<E>(
    bot: Bot,
    exchange: E,
    cfg: Config,
    db: Db,
    chat_id: ChatId,
    message_id: MessageId, // ID сообщения, которое нужно будет обновить
    op_to_unhedge: HedgeOperation,
) where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let hedger = Hedger::new(exchange, cfg);
    let op_id = op_to_unhedge.id;
    let symbol = op_to_unhedge.base_symbol.clone();

    tokio::spawn(async move {
        match hedger.run_unhedge(op_to_unhedge, &db).await {
            Ok((sold, bought)) => {
                info!("Unhedge OK op_id: {}", op_id);
                let text = format!(
                    "✅ Расхеджирование {} (ID:{}) завершено:\n\n🟢 Спот продано: {:.8}\n🔴 Фьюч куплено: {:.8}",
                    symbol, op_id, sold, bought
                );
                bot.edit_message_text(chat_id, message_id, text)
                   .await
                   .map_err(|e| warn!("op_id:{}: Failed edit success unhedge message: {}", op_id, e))
                   .ok();
            }
            Err(e) => {
                error!("Unhedge FAILED op_id: {}: {}", op_id, e);
                bot.edit_message_text(chat_id, message_id, format!("❌ Ошибка расхеджирования ID:{}: {}", op_id, e))
                   .await
                   .map_err(|e| warn!("op_id:{}: Failed edit error unhedge message: {}", op_id, e))
                   .ok();
            }
        }
    });
}
// --- КОНЕЦ Вспомогательной функции ---
