// src/notifier/messages.rs
use crate::config::Config;
use crate::exchange::Exchange;
// --- ИЗМЕНЕНО: Исправляем импорт storage ---
use crate::storage::{Db, insert_hedge_operation};
// --- Конец изменений ---
use super::{UserState, StateStorage, RunningHedges, RunningHedgeInfo};
use tokio::sync::Mutex as TokioMutex;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{Message, MessageId, InlineKeyboardButton, InlineKeyboardMarkup, ChatId};
use tracing::{warn, error, info};
use crate::models::{HedgeRequest, UnhedgeRequest};
use crate::hedger::{Hedger, HedgeProgressUpdate, HedgeProgressCallback}; // Убираем ненужный импорт HedgeParams
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

// Обработчик текстовых сообщений
pub async fn handle_message<E>(
    bot: Bot,
    msg: Message,
    state_storage: StateStorage,
    exchange: E,
    cfg: Config,
    running_hedges: RunningHedges,
    db: &Db, // Принимаем ссылку на пул БД
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

                let bot_msg = bot.send_message(
                    chat_id,
                    format!("Введите ожидаемую волатильность для хеджирования {} {} (%):", sum, cfg.quote_currency),
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
                    state.insert(chat_id, UserState::None); // Сбрасываем состояние
                }

                // Проверяем, не запущен ли уже хедж для этого пользователя и символа
                {
                    let hedges_guard = running_hedges.lock().await;
                    if hedges_guard.contains_key(&(chat_id, symbol.clone())) {
                        warn!("Hedge process already running for chat_id: {}, symbol: {}", chat_id, symbol);
                        bot.send_message(chat_id, format!("⚠️ Хеджирование для {} уже запущено.", symbol)).await?;
                        return Ok(());
                    }
                }

                // Создаем Hedger
                let hedger = Hedger::new(exchange.clone(), cfg.clone());
                let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: vol };
                info!("Starting hedge calculation for chat_id: {}, request: {:?}", chat_id, hedge_request);

                // Вычисляем параметры
                let hedge_params_result = hedger.calculate_hedge_params(&hedge_request).await;

                let cancel_callback_data = format!("cancel_hedge_active_{}", symbol);
                let cancel_button = InlineKeyboardButton::callback("❌ Отмена", cancel_callback_data);
                let initial_kb = InlineKeyboardMarkup::new(vec![vec![cancel_button.clone()]]);

                // Отправляем начальное сообщение "Ожидайте"
                let waiting_msg = match hedge_params_result {
                    Ok(ref params) => {
                        bot.send_message(
                            chat_id,
                            format!(
                                "⏳ Запускаю хеджирование {} {} ({})... \nРыночная цена: {:.2}\nОжидаемая цена покупки: {:.2}",
                                sum, cfg.quote_currency, params.symbol, params.current_spot_price, params.initial_limit_price
                            ),
                        )
                        .reply_markup(initial_kb.clone())
                        .await?
                    }
                    Err(ref e) => {
                        // Если ошибка уже на этапе расчета - сообщаем и выходим
                        error!("Hedge calculation failed for chat_id: {}: {}", chat_id, e);
                        bot.send_message(chat_id, format!("❌ Ошибка расчета параметров хеджирования: {}", e)).await?;
                        // Выходим из функции handle_message, так как продолжать нельзя
                        return Ok(());
                    }
                };

                // Если расчет успешен, запускаем сам хедж в отдельной задаче
                if let Ok(params) = hedge_params_result {
                    info!("Hedge calculation successful. Running hedge execution for chat_id: {}", chat_id);

                    // Вставляем запись в БД ДО запуска задачи
                    let operation_id = match insert_hedge_operation(
                        db,
                        chat_id.0,
                        &params.symbol,
                        &cfg.quote_currency,
                        sum,
                        vol,
                        params.spot_order_qty, // Передаем округленное брутто
                        params.fut_order_qty,  // Передаем округленное нетто
                    ).await {
                        Ok(id) => { info!("Created hedge operation record in DB with id: {}", id); id }
                        Err(e) => {
                            error!("Failed to insert hedge operation into DB: {}", e);
                            bot.edit_message_text(
                                chat_id, waiting_msg.id,
                                format!("❌ Критическая ошибка: не удалось создать запись в БД: {}", e)
                            ).reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())).await?;
                            return Ok(());
                        }
                    };


                    let current_order_id_storage = Arc::new(TokioMutex::new(None::<String>));
                    let total_filled_qty_storage = Arc::new(TokioMutex::new(0.0f64));

                    // Клонируем все необходимые переменные для передачи в задачу
                    let bot_for_task = bot.clone(); // <-- Клон для всей задачи
                    let waiting_msg_id = waiting_msg.id;
                    // initial_sum больше не нужен для финального сообщения, но нужен для колбэка
                    let initial_sum_for_callback = sum;
                    let initial_symbol = params.symbol.clone();
                    let symbol_for_remove = initial_symbol.clone();
                    let symbol_for_messages = initial_symbol.clone();
                    let running_hedges_clone = running_hedges.clone();
                    let hedger_clone = hedger.clone();
                    let vol_raw_clone = vol_raw; // Для финального сообщения
                    let current_order_id_storage_clone = current_order_id_storage.clone();
                    let total_filled_qty_storage_clone = total_filled_qty_storage.clone();
                    let cfg_clone = cfg.clone();
                    let db_clone = db.clone(); // Клонируем пул БД


                    // Запускаем асинхронную задачу для хеджирования
                    let task = tokio::spawn(async move {
                        // --- Создаем отдельный клон бота и данных для колбэка ---
                        let bot_for_callback = bot_for_task.clone();
                        let symbol_for_button_callback = symbol_for_messages.clone();
                        let qc_for_callback_inner = cfg_clone.quote_currency.clone();
                        // --- Конец ---

                        // --- Колбэк для обновления прогресса ---
                        let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
                            // --- Используем клоны для колбэка ---
                            let bot = bot_for_callback.clone();
                            let msg_id = waiting_msg_id;
                            let chat_id = chat_id;
                            let sum = initial_sum_for_callback; // Используем исходную сумму в статусе
                            let symbol = symbol_for_button_callback.clone();
                            let qc = qc_for_callback_inner.clone();
                            // --- Конец ---

                            async move {
                                let cancel_callback_data = format!("cancel_hedge_active_{}", symbol);
                                let cancel_button = InlineKeyboardButton::callback("❌ Отмена", cancel_callback_data);
                                let kb = InlineKeyboardMarkup::new(vec![vec![cancel_button]]);

                                let status_text = if update.is_replacement { "(Ордер переставлен)" } else { "" };

                                // Показываем прогресс текущего ордера
                                let filled_percent = if update.target_qty > 0.0 {
                                    (update.filled_qty / update.target_qty) * 100.0
                                } else { 0.0 };
                                let fill_status = format!(
                                    "\nИсполнено: {:.6}/{:.6} ({:.1}%)",
                                    update.filled_qty, update.target_qty, filled_percent
                                );

                                let text = format!(
                                    "⏳ Хеджирование {} {} ({}) в процессе...\nРыночная цена: {:.2}\nОрдер на покупку: {:.2} {}{}",
                                    sum, qc, symbol, update.current_spot_price, update.new_limit_price, status_text, fill_status
                                );

                                if let Err(e) = bot.edit_message_text(chat_id, msg_id, text).reply_markup(kb).await {
                                    if !e.to_string().contains("message is not modified") {
                                        warn!("Failed to edit message during hedge progress update: {}", e);
                                    }
                                }
                                Ok(())
                            }.boxed()
                        });

                        // --- Вызов run_hedge ---
                        let result = hedger_clone.run_hedge(
                            params, // Передаем рассчитанные параметры
                            progress_callback, // Передаем колбэк, который захватил bot_for_callback
                            current_order_id_storage_clone,
                            total_filled_qty_storage_clone,
                            operation_id,
                            &db_clone,
                        ).await; // Ожидаем Result<(f64, f64, f64)>

                        // Проверяем, была ли отмена
                        let is_cancelled = result.is_err() && result.as_ref().err().map_or(false, |e| e.to_string().contains("cancelled"));

                        // Удаляем информацию о запущенном хедже, если он не был отменен
                        if !is_cancelled {
                            running_hedges_clone.lock().await.remove(&(chat_id, symbol_for_remove));
                            info!("Removed running hedge info for chat_id: {}, symbol: {}", chat_id, symbol_for_messages);
                        }

                        // --- Обработка результата хеджирования ---
                        match result {
                            // --- Принимаем 3 значения ---
                            Ok((spot_qty_gross, fut_qty_net, final_spot_value_gross)) => {
                                info!(
                                    "Hedge execution successful for chat_id: {}. Spot Gross: {}, Fut Net: {}, Final Spot Gross Value: {:.2}",
                                    chat_id, spot_qty_gross, fut_qty_net, final_spot_value_gross
                                );
                                // --- Используем bot_for_task и обновленный формат ---
                                let _ = bot_for_task.edit_message_text(
                                    chat_id,
                                    waiting_msg_id,
                                    format!(
                                        // Отображаем фактические брутто спота и нетто фьюча, и стоимость брутто спота
                                        "✅ Хеджирование ID:{} ~{:.2} {} ({}) при V={:.1}% завершено:\n\n🟢 Спот (брутто): {:.6}\n🔴 Фьюч (нетто): {:.6}\n(Баланс спота будет чуть меньше из-за комиссии)",
                                        operation_id, final_spot_value_gross, cfg_clone.quote_currency, symbol_for_messages, vol_raw_clone, spot_qty_gross, fut_qty_net,
                                    ),
                                )
                                .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())) // Убираем кнопку
                                .await;
                                // --- Конец изменений ---
                            }
                            Err(e) => {
                                // Обработка ошибок (включая отмену)
                                if is_cancelled {
                                     info!("Hedge task for chat {}, symbol {} (op_id: {}) was cancelled.", chat_id, symbol_for_messages, operation_id);
                                     // Сообщение об отмене уже отправлено в handle_callback
                                } else {
                                    error!("Hedge execution failed for chat_id: {}, symbol: {}, op_id: {}: {}", chat_id, symbol_for_messages, operation_id, e);
                                     // --- Используем bot_for_task ---
                                     let _ = bot_for_task.edit_message_text(
                                        chat_id,
                                        waiting_msg_id,
                                        format!("❌ Ошибка выполнения хеджирования ID:{}: {}", operation_id, e)
                                     )
                                     .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                                     .await;
                                     // --- Конец изменений ---
                                }
                            }
                        } // Конец match result
                    }); // Конец tokio::spawn

                    // Сохраняем информацию о запущенной задаче
                    {
                        let mut hedges_guard = running_hedges.lock().await;
                        hedges_guard.insert((chat_id, initial_symbol.clone()), RunningHedgeInfo {
                            handle: task.abort_handle(),
                            current_order_id: current_order_id_storage,
                            total_filled_qty: total_filled_qty_storage,
                            symbol: initial_symbol.clone(),
                            bot_message_id: waiting_msg.id.0,
                            operation_id,
                        });
                        info!("Stored running hedge info for chat_id: {}, symbol: {}, op_id: {}", chat_id, initial_symbol, operation_id);
                    }

                } // Конец if let Ok(params)
            } else {
                bot.send_message(chat_id, "⚠️ Неверный формат волатильности. Введите число (например, 60 или 60%).").await?;
            }
        } // Конец UserState::AwaitingVolatility

        // --- Обработка ввода количества для РАСХЕДЖИРОВАНИЯ ---
        Some(UserState::AwaitingUnhedgeQuantity { symbol, last_bot_message_id }) => {
            if let Ok(quantity) = text.parse::<f64>() {
                if quantity <= 0.0 {
                    bot.send_message(chat_id, "⚠️ Количество должно быть положительным.").await?;
                    return Ok(());
                }

                cleanup_chat(&bot, chat_id, message_id, last_bot_message_id).await;

                {
                    let mut state = state_storage.write().expect("Lock failed");
                    state.insert(chat_id, UserState::None); // Сбрасываем состояние
                }

                // Создаем Hedger (используя новый конструктор)
                let hedger = Hedger::new(exchange.clone(), cfg.clone());
                let waiting_msg = bot.send_message(chat_id, format!("⏳ Запускаю расхеджирование {} {}...", quantity, symbol)).await?;
                info!("Starting unhedge for chat_id: {}, symbol: {}, quantity: {}", chat_id, symbol, quantity);

                let bot_clone = bot.clone(); // Клон для задачи
                let waiting_msg_id = waiting_msg.id;
                let symbol_clone = symbol.clone();
                let quantity_clone = quantity;
                let hedger_clone = hedger.clone();
                let _db_clone = db.clone(); // Пока не используется в run_unhedge

                // Запускаем задачу расхеджирования
                tokio::spawn(async move {
                    match hedger_clone.run_unhedge(
                        UnhedgeRequest { quantity: quantity_clone, symbol: symbol_clone.clone() },
                        // TODO: Передать ID операции хеджа и БД, когда логика будет готова
                        // 0,
                        // &db_clone,
                    ).await {
                        Ok((sold, bought)) => {
                            info!("Unhedge successful for chat_id: {}. Sold spot: {}, Bought fut: {}", chat_id, sold, bought);
                            let _ = bot_clone.edit_message_text( // Используем bot_clone
                                chat_id,
                                waiting_msg_id,
                                format!(
                                    "✅ Расхеджирование {} {} завершено:\n\n🟢 Продано спота: {:.6}\n🔴 Куплено фьюча: {:.6}",
                                    quantity_clone, symbol_clone, sold, bought,
                                ),
                            )
                            .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())) // Убираем кнопку
                            .await;
                        }
                        Err(e) => {
                            error!("Unhedge failed for chat_id: {}: {}", chat_id, e);
                             let _ = bot_clone.edit_message_text( // Используем bot_clone
                                chat_id,
                                waiting_msg_id,
                                format!("❌ Ошибка расхеджирования {}: {}", symbol_clone, e)
                             )
                             .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())) // Убираем кнопку
                             .await;
                        }
                    }
                }); // Конец tokio::spawn для unhedge

            } else {
                bot.send_message(chat_id, "⚠️ Неверный формат количества. Введите число (например, 10.5).").await?;
            }
        }

        // --- Обработка других состояний ---
        Some(UserState::AwaitingAssetSelection { last_bot_message_id: _ }) => {
            // Пользователь должен нажать кнопку, а не писать текст
            if let Err(e) = bot.delete_message(chat_id, message_id).await {
                warn!("Failed to delete unexpected user message {}: {}", message_id, e);
            }
            info!("User {} sent text while AwaitingAssetSelection.", chat_id);
            // Можно отправить подсказку, если нужно
            // bot.send_message(chat_id, "Пожалуйста, выберите актив кнопкой выше.").await?;
        }

        // --- Нет активного состояния ---
        None | Some(UserState::None) => {
            // Игнорируем сообщения, если нет активного диалога
            if let Err(e) = bot.delete_message(chat_id, message_id).await {
                warn!("Failed to delete unexpected user message {}: {}", message_id, e);
            }
            // Можно не отправлять сообщение или отправить подсказку про /help
            // bot.send_message(chat_id, "🤖 Сейчас нет активного диалога. Используйте /help для списка команд.").await?;
        }
    } // Конец match user_state

    Ok(())
}