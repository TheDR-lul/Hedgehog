// src/notifier/active_ops.rs

use super::{
    RunningOperations, RunningOperationInfo, OperationType, callback_data, navigation, StateStorage // Убраны Command, UserState
};
use crate::config::Config;
use crate::exchange::{Exchange, OrderSide};
use crate::storage::{Db, update_hedge_final_status};
use crate::hedger::ORDER_FILL_TOLERANCE;
use crate::notifier::HashMap; // Используется в RunningOperations
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId,
};
use teloxide::requests::Requester;
use tracing::{info, warn, error};
use tokio::sync::MutexGuard; // Убран неиспользуемый Mutex as TokioMutex

// --- Вспомогательная функция для форматирования списка активных операций ---
async fn format_active_operations(
    running_operations: &RunningOperations,
    chat_id: ChatId,
) -> (String, InlineKeyboardMarkup) {
    let ops_guard = running_operations.lock().await;
    let user_ops: Vec<_> = ops_guard
        .iter()
        .filter(|((op_chat_id, _op_id), _info)| *op_chat_id == chat_id)
        .collect();

    if user_ops.is_empty() {
        let text = "✅ Нет активных операций хеджирования или расхеджирования.".to_string();
        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN),
        ]]);
        return (text, keyboard);
    }

    let mut text = format!("⚡ Активные операции ({} шт.):\n\n", user_ops.len());
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    // Сортируем по ID операции
    let mut sorted_user_ops = user_ops;
    sorted_user_ops.sort_by_key(|((_, op_id), _)| *op_id);

    for ((_op_chat_id, op_id), info) in sorted_user_ops {
        let op_type_str = match info.operation_type {
            OperationType::Hedge => "Хедж",
            OperationType::Unhedge => "Расхедж",
        };
        let filled_qty = *info.total_filled_spot_qty.lock().await;
        // TODO: Получить target_qty?
        text.push_str(&format!(
            "🔹 ID:{} ({}) - {} \n   Прогресс спот: ~{:.6} (?)\n",
            op_id, info.symbol, op_type_str, filled_qty
        ));
        let cancel_data = format!("{}{}", callback_data::PREFIX_CANCEL_ACTIVE_OP, op_id);
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("❌ Отменить ID:{}", op_id),
            cancel_data,
        )]);
    }

    buttons.push(vec![InlineKeyboardButton::callback(
        "⬅️ Назад",
        callback_data::BACK_TO_MAIN,
    )]);

    (text, InlineKeyboardMarkup::new(buttons))
}

// --- Обработчики Команд и Колбэков ---

/// Обработчик команды /active
pub async fn handle_active_command<E>(
    bot: Bot,
    msg: Message,
    _exchange: Arc<E>,
    _state_storage: StateStorage,
    running_operations: RunningOperations,
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    info!("Processing /active command for chat_id: {}", chat_id);

    let (text, keyboard) = format_active_operations(&running_operations, chat_id).await;
    bot.send_message(chat_id, text)
        .reply_markup(keyboard)
        .await?;

    if let Err(e) = bot.delete_message(chat_id, msg.id).await {
        warn!("Failed to delete /active command message: {}", e);
    }
    Ok(())
}

/// Обработчик колбэка кнопки "Активные операции" из главного меню
pub async fn handle_menu_active_ops_callback(
    bot: Bot,
    query: CallbackQuery,
    running_operations: RunningOperations,
    _state_storage: StateStorage,
) -> anyhow::Result<()> {
    if let Some(msg) = query.message {
        let chat_id = msg.chat().id;
        info!(
            "Processing '{}' callback for chat_id: {}",
            callback_data::MENU_ACTIVE_OPS,
            chat_id
        );

        let (text, keyboard) = format_active_operations(&running_operations, chat_id).await;
        bot.edit_message_text(chat_id, msg.id(), text)
            .reply_markup(keyboard)
            .await?;
    } else {
        warn!("CallbackQuery missing message in handle_menu_active_ops_callback");
    }
    bot.answer_callback_query(query.id).await?;
    Ok(())
}

/// Обработчик колбэка отмены активной операции (префикс cancel_op_)
pub async fn handle_cancel_active_op_callback<E>(
    bot: Bot,
    query: CallbackQuery,
    exchange: Arc<E>,
    _state_storage: StateStorage,
    running_operations: RunningOperations,
    _cfg: Arc<Config>, // Переменная cfg не используется, добавлено подчеркивание
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let (Some(data), Some(msg)) = (query.data, query.message) {
        let chat_id = msg.chat().id;
        if let Some(operation_id_str) = data.strip_prefix(callback_data::PREFIX_CANCEL_ACTIVE_OP) {
            if let Ok(operation_id_to_cancel) = operation_id_str.parse::<i64>() {
                info!(
                    "User {} requested cancellation for active operation ID: {}",
                    chat_id, operation_id_to_cancel
                );

                let mut operation_info_opt: Option<RunningOperationInfo> = None;
                let mut current_spot_order_id_to_cancel_opt: Option<String> = None; // Изменено имя для ясности
                let mut filled_spot_qty_in_operation: f64 = 0.0;

                // --- Блок для извлечения информации и удаления из мапы ---
                {
                    let mut ops_guard: MutexGuard<
                        '_,
                        HashMap<(ChatId, i64), RunningOperationInfo>,
                    > = running_operations.lock().await;
                    if let Some(info) = ops_guard.remove(&(chat_id, operation_id_to_cancel)) {
                        current_spot_order_id_to_cancel_opt = // Используем новое имя
                            info.current_spot_order_id.lock().await.clone();
                        filled_spot_qty_in_operation = *info.total_filled_spot_qty.lock().await;
                        operation_info_opt = Some(info);
                        info!(
                            "op_id:{}: Found active operation, removed from map.",
                            operation_id_to_cancel
                        );
                    } else {
                        warn!(
                            "op_id:{}: Active operation not found in map for cancellation request.",
                            operation_id_to_cancel
                        );
                        bot.answer_callback_query(query.id)
                            .text("Операция уже завершена или отменена.")
                            .show_alert(true)
                            .await?;
                        let _ = bot
                            .edit_message_text(chat_id, msg.id(), "ℹ️ Операция уже неактивна.")
                            .reply_markup(navigation::make_main_menu_keyboard())
                            .await;
                        return Ok(());
                    }
                }

                // --- Если информация найдена, выполняем отмену ---
                // Используем operation_info_opt напрямую
                if let Some(operation_info) = operation_info_opt { // Проверка, что информация была извлечена
                    let symbol = operation_info.symbol.clone();
                    let bot_message_id_to_edit = MessageId(operation_info.bot_message_id);
                    let operation_type = operation_info.operation_type;

                    info!("op_id:{}: Aborting task...", operation_id_to_cancel);
                    operation_info.handle.abort();

                    let cancelling_text = format!(
                        "⏳ Отмена операции ID:{} ({}) ...",
                        operation_id_to_cancel, symbol
                    );
                    let _ = bot
                        .edit_message_text(chat_id, bot_message_id_to_edit, cancelling_text)
                        .reply_markup(InlineKeyboardMarkup::new( // Убираем кнопки сразу
                            Vec::<Vec<InlineKeyboardButton>>::new(),
                        ))
                        .await;

                    // --- Логика обработки отмены ---
                    let mut final_error_message: Option<String> = None; // Ошибка для показа пользователю
                    let mut net_spot_change_on_cancel = 0.0;

                    // 1. Отмена текущего активного ордера (если есть)
                    if let Some(ref order_id) = current_spot_order_id_to_cancel_opt { // Используем новое имя
                        info!(
                            "op_id:{}: Cancelling current order {} ({:?})",
                            operation_id_to_cancel, order_id, operation_type
                        );
                        match exchange.cancel_order(&symbol, order_id).await {
                            Ok(_) => info!(
                                "op_id:{}: Order cancel request sent OK.",
                                operation_id_to_cancel
                            ),
                            Err(e) => {
                                warn!(
                                    "op_id:{}: Order cancel FAILED: {}. Might be already filled/cancelled.",
                                    operation_id_to_cancel, e
                                );
                                final_error_message = Some(format!("Failed cancel order: {}", e));
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }

                    // 2. Компенсирующее действие на бирже
                    match operation_type {
                        OperationType::Hedge => {
                            if filled_spot_qty_in_operation > ORDER_FILL_TOLERANCE {
                                info!(
                                    "op_id:{}: Hedge cancelled. Attempting to sell filled spot qty: {}",
                                    operation_id_to_cancel, filled_spot_qty_in_operation
                                );
                                let current_balance = match exchange.get_balance(&symbol).await {
                                    Ok(b) => b.free,
                                    Err(e) => {
                                        error!("op_id:{}: Failed get balance before selling spot: {}", operation_id_to_cancel, e);
                                        if final_error_message.is_none() { // Записываем ошибку, если не было другой
                                            final_error_message = Some(format!("Failed get balance: {}", e));
                                        }
                                        0.0
                                    }
                                };

                                let qty_to_sell = filled_spot_qty_in_operation.min(current_balance);
                                if qty_to_sell > ORDER_FILL_TOLERANCE {
                                     match exchange.place_spot_market_order(&symbol, OrderSide::Sell, qty_to_sell).await {
                                        Ok(order) => {
                                            info!("op_id:{}: Spot Sell OK on hedge cancel: order_id={}, qty={}", operation_id_to_cancel, order.id, qty_to_sell);
                                            net_spot_change_on_cancel = qty_to_sell;
                                        }
                                        Err(e) => {
                                            error!("op_id:{}: Spot Sell FAILED on hedge cancel: {}", operation_id_to_cancel, e);
                                             if final_error_message.is_none() {
                                                final_error_message = Some(format!("Failed sell spot: {}", e));
                                            }
                                        }
                                    }
                                } else {
                                    warn!("op_id:{}: Spot balance ({}) too low to sell filled qty ({}) on hedge cancel.", operation_id_to_cancel, current_balance, filled_spot_qty_in_operation);
                                     if final_error_message.is_none() { // Сообщаем об этом, если не было других ошибок
                                        final_error_message = Some("Balance too low to sell filled spot.".to_string());
                                    }
                                }
                            } else {
                                info!(
                                    "op_id:{}: No significant spot filled ({}) during hedge cancel, skipping sell.",
                                    operation_id_to_cancel, filled_spot_qty_in_operation
                                );
                            }
                        }
                        OperationType::Unhedge => {
                            warn!(
                                "op_id:{}: Unhedge cancellation logic: Assuming futures were not bought yet. No futures action taken.",
                                operation_id_to_cancel
                            );
                            if filled_spot_qty_in_operation > ORDER_FILL_TOLERANCE {
                                warn!(
                                     "op_id:{}: Unhedge cancelled. Spot sell progress was {}. Buy back logic NOT IMPLEMENTED.",
                                     operation_id_to_cancel, filled_spot_qty_in_operation
                                );
                                // TODO: Implement spot buy back if run_unhedge changes
                            }
                        }
                    }

                    // 3. Обновление статуса в БД
                    let final_db_status = "Cancelled";
                    let final_spot_qty_for_db = match operation_type {
                         OperationType::Hedge => net_spot_change_on_cancel,
                         OperationType::Unhedge => 0.0, // TODO: Уточнить
                    };

                    // ---- ИСПРАВЛЕНО: Логика для избежания move error ----
                    let cancel_reason_str = "cancelled by user";
                    // Сначала определяем текст ошибки для БД
                    let final_error_text_for_db: Option<String>;
                    if let Some(err_msg) = &final_error_message {
                        final_error_text_for_db = Some(err_msg.clone());
                    } else {
                        final_error_text_for_db = Some(cancel_reason_str.to_string());
                    }
                    // ---- Конец исправления ----

                    // Вызываем update_hedge_final_status с КОРРЕКТНЫМИ параметрами
                    if let Err(db_err) = update_hedge_final_status(
                        db.as_ref(),
                        operation_id_to_cancel,
                        final_db_status, // "Cancelled"
                        None,          // futures_order_id при отмене обычно None
                        0.0,           // futures_filled_qty при отмене = 0.0 (предположение)
                        final_error_text_for_db.as_deref(),
                    )
                    .await
                    {
                        error!(
                            "op_id:{}: Failed DB update after cancellation: {}",
                            operation_id_to_cancel, db_err
                        );
                         if final_error_message.is_none() {
                            final_error_message = Some(format!("DB update failed: {}", db_err));
                         }
                    } else {
                        // Логируем и количество спота, измененное при отмене, хотя оно не передается в DB в этом параметре
                        info!(
                            "op_id:{}: DB status updated to '{}'. Spot qty changed on cancel: {}",
                            operation_id_to_cancel, final_db_status, final_spot_qty_for_db
                        );
                    }

                    // Примечание: Количество спота, проданное/купленное при отмене (`final_spot_qty_for_db`),
                    // в текущей версии НЕ сохраняется в БД функцией `update_hedge_final_status`.
                    // Если это необходимо, вам нужно будет изменить сигнатуру функции в `storage/db.rs`
                    // и соответствующий SQL-запрос UPDATE, добавив новый параметр/поле.


                    // 4. Финальное сообщение пользователю
                    let mut final_text = format!(
                        "❌ Операция ID:{} ({}, {}) отменена пользователем.",
                        operation_id_to_cancel, symbol, operation_type.as_str()
                    );
                     match operation_type {
                         OperationType::Hedge => {
                              if net_spot_change_on_cancel > ORDER_FILL_TOLERANCE {
                                  final_text.push_str(&format!("\nПродано ~{:.8} {} спота.", net_spot_change_on_cancel, symbol));
                              } else if filled_spot_qty_in_operation > ORDER_FILL_TOLERANCE {
                                   // Показываем ошибку продажи только если она реально была
                                   if final_error_message.as_ref().map_or(false, |s| s.contains("Failed sell spot") || s.contains("Failed get balance") || s.contains("Balance too low")) {
                                        final_text.push_str("\nПопытка продать накопленный спот не удалась.");
                                   }
                              }
                         }
                         OperationType::Unhedge => {
                             // Добавить информацию при необходимости
                         }
                     }
                     // Добавляем сообщение об ошибке, если оно было (final_error_message не был перемещен)
                     if let Some(err_msg) = final_error_message { // Теперь это безопасно
                         final_text.push_str(&format!("\n⚠️ Ошибка при отмене: {}", err_msg));
                     }

                    let _ = bot
                        .edit_message_text(chat_id, bot_message_id_to_edit, final_text)
                        .reply_markup(navigation::make_main_menu_keyboard())
                        .await;
                }
                // Если operation_info_opt был None (т.е. Some не сработал), значит операция не найдена
                // Этот случай обрабатывается в блоке извлечения информации

            } else {
                error!(
                    "Failed to parse operation_id from cancel callback data: {}",
                    operation_id_str
                );
                bot.answer_callback_query(query.id)
                    .text("Ошибка: Неверный ID операции.")
                    .await?;
            }
        } else {
            warn!(
                "Invalid callback data format for cancel active operation: {}",
                data
            );
            bot.answer_callback_query(query.id)
                .text("Неизвестное действие.")
                .await?;
        }
    } else {
        warn!("CallbackQuery missing data or message in handle_cancel_active_op_callback");
        bot.answer_callback_query(query.id).await?;
    }
    Ok(())
}


impl OperationType {
    fn as_str(&self) -> &'static str {
        match self {
            OperationType::Hedge => "Хедж",
            OperationType::Unhedge => "Расхедж",
        }
    }
}