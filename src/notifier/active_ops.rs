// src/notifier/active_ops.rs

use super::{
    RunningOperations, RunningOperationInfo, OperationType, callback_data, navigation, StateStorage
};
use crate::storage::{Db, update_hedge_final_status, get_hedge_operation_by_id};
use crate::config::Config;
use crate::exchange::Exchange;
use crate::hedger::ORDER_FILL_TOLERANCE;
use crate::exchange::types::OrderSide; // <-- ДОБАВЬ ЭТУ СТРОКУ
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId,
};
use teloxide::requests::Requester;
use tracing::{info, warn, error};


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
        // --- ИСПРАВЛЕНО: Обработка ошибки мьютекса (хотя маловероятно) ---
        let filled_qty = match info.total_filled_spot_qty.lock().await {
            guard => *guard,
            // Если мьютекс отравлен, возвращаем 0.0
            // Err(poisoned) => {
            //     error!("Mutex poisoned when reading filled_qty for op_id: {}", op_id);
            //     *poisoned.into_inner() // Можно попытаться получить данные
            // }
        };
        // TODO: Получить target_qty из БД?
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
    _cfg: Arc<Config>,
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
                let mut filled_spot_qty_in_operation: f64 = 0.0;

                // --- Блок для извлечения информации и удаления из мапы ---
                {
                    let mut ops_guard = running_operations.lock().await;
                    if let Some(info) = ops_guard.remove(&(chat_id, operation_id_to_cancel)) {
                        // --- ИСПРАВЛЕНО: Обработка ошибки мьютекса ---
                        filled_spot_qty_in_operation = match info.total_filled_spot_qty.lock().await {
                            guard => *guard,
                            // Err(poisoned) => {
                            //     error!("Mutex poisoned when reading filled_qty for op_id: {}", operation_id_to_cancel);
                            //     *poisoned.into_inner()
                            // }
                        };
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
                } // ops_guard освобождается здесь

                // --- Если информация найдена, выполняем отмену ---
                if let Some(operation_info) = operation_info_opt {
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
                        .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                        .await;

                    // --- Логика обработки отмены ---
                    let mut final_error_message: Option<String> = None;
                    let mut net_spot_change_on_cancel = 0.0;

                    // Получаем ID последнего ордера из БД
                    let last_spot_order_id_from_db = match get_hedge_operation_by_id(db.as_ref(), operation_id_to_cancel).await {
                        // --- ИСПРАВЛЕНО: Используем spot_order_id вместо last_spot_order_id ---
                        Ok(Some(op)) => op.spot_order_id, // Получаем ID из записи операции
                        Ok(None) => {
                            warn!("op_id:{}: Operation not found in DB during cancellation.", operation_id_to_cancel);
                            None
                        }
                        Err(e) => {
                            error!("op_id:{}: Failed to query DB for last order ID during cancellation: {}", operation_id_to_cancel, e);
                            if final_error_message.is_none() {
                                final_error_message = Some(format!("DB query failed: {}", e));
                            }
                            None
                        }
                    };

                    // 1. Отмена текущего активного ордера (если ID известен из БД)
                    if let Some(ref order_id) = last_spot_order_id_from_db {
                        info!(
                            "op_id:{}: Cancelling last known order {} from DB ({:?})",
                            operation_id_to_cancel, order_id, operation_type
                        );
                        let is_spot_order = match operation_type {
                            OperationType::Hedge => true,
                            OperationType::Unhedge => false,
                        };
                        let symbol_for_cancel = if is_spot_order {
                            &symbol
                        } else {
                            warn!("op_id:{}: Cancellation for futures order in Unhedge not fully implemented yet.", operation_id_to_cancel);
                            ""
                        };

                        if !symbol_for_cancel.is_empty() {
                            match cancel_order_generic(exchange.clone(), symbol_for_cancel, order_id, is_spot_order).await {
                                Ok(_) => info!(
                                    "op_id:{}: Order cancel request sent OK.",
                                    operation_id_to_cancel
                                ),
                                Err(e) => {
                                    warn!(
                                        "op_id:{}: Order cancel FAILED: {}. Might be already filled/cancelled.",
                                        operation_id_to_cancel, e
                                    );
                                    if final_error_message.is_none() {
                                        final_error_message = Some(format!("Failed cancel order: {}", e));
                                    }
                                }
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    } else {
                        info!("op_id:{}: No active order ID found in DB to cancel.", operation_id_to_cancel);
                    }

                    // 2. Компенсирующее действие на бирже (логика остается прежней)
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
                                        if final_error_message.is_none() {
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
                                     if final_error_message.is_none() {
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
                         OperationType::Unhedge => 0.0,
                    };

                    let cancel_reason_str = "cancelled by user";
                    let final_error_text_for_db: Option<String>;
                    if let Some(err_msg) = &final_error_message {
                        final_error_text_for_db = Some(err_msg.clone());
                    } else {
                        final_error_text_for_db = Some(cancel_reason_str.to_string());
                    }

                    // Вызываем update_hedge_final_status
                    // --- ИСПРАВЛЕНО: Используем .as_deref() для final_error_text_for_db ---
                    if let Err(db_err) = update_hedge_final_status(
                        db.as_ref(),
                        operation_id_to_cancel,
                        final_db_status,
                        None, // last_spot_order_id - оставляем None при отмене
                        final_spot_qty_for_db,
                        final_error_text_for_db.as_deref(), // <-- ИСПОЛЬЗУЕМ .as_deref()
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
                        info!(
                            "op_id:{}: DB status updated to '{}'. Spot qty changed on cancel: {}",
                            operation_id_to_cancel, final_db_status, final_spot_qty_for_db
                        );
                    }


                    // 4. Финальное сообщение пользователю (логика остается прежней)
                    let mut final_text = format!(
                        "❌ Операция ID:{} ({}, {}) отменена пользователем.",
                        operation_id_to_cancel, symbol, operation_type.as_str()
                    );
                     match operation_type {
                         OperationType::Hedge => {
                              if net_spot_change_on_cancel > ORDER_FILL_TOLERANCE {
                                  final_text.push_str(&format!("\nПродано ~{:.8} {} спота.", net_spot_change_on_cancel, symbol));
                              } else if filled_spot_qty_in_operation > ORDER_FILL_TOLERANCE {
                                   if final_error_message.as_ref().map_or(false, |s| s.contains("Failed sell spot") || s.contains("Failed get balance") || s.contains("Balance too low")) {
                                        final_text.push_str("\nПопытка продать накопленный спот не удалась.");
                                   }
                              }
                         }
                         OperationType::Unhedge => {
                             // Добавить информацию при необходимости
                         }
                     }
                     if let Some(err_msg) = final_error_message {
                         final_text.push_str(&format!("\n⚠️ Ошибка при отмене: {}", err_msg));
                     }

                    let _ = bot
                        .edit_message_text(chat_id, bot_message_id_to_edit, final_text)
                        .reply_markup(navigation::make_main_menu_keyboard())
                        .await;
                }

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

// Общая функция отмены ордера
// --- ИСПРАВЛЕНО: Возвращаемый тип Result ---
async fn cancel_order_generic<E: Exchange>(
    exchange: Arc<E>,
    symbol: &str,
    order_id: &str,
    is_spot: bool,
) -> anyhow::Result<()> { // <-- ИСПРАВЛЕНО
        if is_spot {
        exchange.cancel_spot_order(symbol, order_id).await
    } else {
        exchange.cancel_futures_order(symbol, order_id).await
    }
}


impl OperationType {
    fn as_str(&self) -> &'static str {
        match self {
            OperationType::Hedge => "Хедж",
            OperationType::Unhedge => "Расхедж",
        }
    }
}
