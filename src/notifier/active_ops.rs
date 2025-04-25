// src/notifier/active_ops.rs

use super::{Command, StateStorage, UserState, RunningOperations, RunningOperationInfo, OperationType, callback_data, navigation};
use crate::config::Config;
use crate::exchange::{Exchange, OrderSide}; // Добавили OrderSide
use crate::storage::{Db, update_hedge_final_status}; // Добавили update_hedge_final_status
use crate::hedger::ORDER_FILL_TOLERANCE;
use crate::notifier::HashMap;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId,
};
use teloxide::requests::Requester; // Для answer_callback_query
use tracing::{info, warn, error};
use tokio::sync::MutexGuard; // Для работы с MutexGuard

// --- Вспомогательная функция для форматирования списка активных операций ---
async fn format_active_operations(
    running_operations: &RunningOperations,
    chat_id: ChatId,
) -> (String, InlineKeyboardMarkup) {
    let ops_guard = running_operations.lock().await;
    let user_ops: Vec<_> = ops_guard.iter()
        .filter(|((op_chat_id, _op_id), _info)| *op_chat_id == chat_id)
        .collect();

    if user_ops.is_empty() {
        let text = "✅ Нет активных операций хеджирования или расхеджирования.".to_string();
        let kb = InlineKeyboardMarkup::new(vec![vec![
             InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
        ]]);
        return (text, kb);
    }

    let mut text = format!("⚡ Активные операции ({} шт.):\n\n", user_ops.len());
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    // Сортируем по ID операции (или можно по времени старта, если хранить)
    let mut sorted_user_ops = user_ops;
    sorted_user_ops.sort_by_key(|((_, op_id), _)| *op_id);

    for ((_op_chat_id, op_id), info) in sorted_user_ops {
        let op_type_str = match info.operation_type {
            OperationType::Hedge => "Хедж",
            OperationType::Unhedge => "Расхедж",
        };
        // Получаем примерный прогресс (требует блокировки мьютекса)
        let filled_qty = *info.total_filled_spot_qty.lock().await;
        // TODO: Получить target_qty из БД или хранить в RunningOperationInfo?
        // Пока просто показываем символ и тип
        text.push_str(&format!(
            "🔹 ID:{} ({}) - {} \n   Прогресс спот: ~{:.6} (?)\n",
            op_id, info.symbol, op_type_str, filled_qty
        ));
        // Добавляем кнопку отмены для этой операции
        let cancel_data = format!("{}{}", callback_data::PREFIX_CANCEL_ACTIVE_OP, op_id);
        buttons.push(vec![
            InlineKeyboardButton::callback(format!("❌ Отменить ID:{}", op_id), cancel_data)
        ]);
    }

    // Добавляем кнопку Назад
    buttons.push(vec![InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)]);

    (text, InlineKeyboardMarkup::new(buttons))
}


// --- Обработчики Команд и Колбэков ---

/// Обработчик команды /active
pub async fn handle_active_command<E>(
    bot: Bot,
    msg: Message,
    _exchange: Arc<E>, // Не используется напрямую здесь
    _state_storage: StateStorage, // Не используется напрямую здесь
    running_operations: RunningOperations,
    _cfg: Arc<Config>, // Не используется напрямую здесь
    _db: Arc<Db>, // Не используется напрямую здесь
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    info!("Processing /active command for chat_id: {}", chat_id);

    // Сразу показываем список
    let (text, kb) = format_active_operations(&running_operations, chat_id).await;
    bot.send_message(chat_id, text).reply_markup(kb).await?;

    // Удаляем сообщение с командой
    if let Err(e) = bot.delete_message(chat_id, msg.id).await {
        warn!("Failed to delete /active command message: {}", e);
    }
    Ok(())
}

/// Обработчик колбэка кнопки "Активные операции" из главного меню
pub async fn handle_menu_active_ops_callback(
    bot: Bot,
    q: CallbackQuery,
    running_operations: RunningOperations,
    _state_storage: StateStorage, // Не используется
) -> anyhow::Result<()> {
     if let Some(msg) = q.message {
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::MENU_ACTIVE_OPS, chat_id);

        // Редактируем сообщение, показывая список активных операций
        let (text, kb) = format_active_operations(&running_operations, chat_id).await;
        bot.edit_message_text(chat_id, msg.id(), text).reply_markup(kb).await?;
     } else {
         warn!("CallbackQuery missing message in handle_menu_active_ops_callback");
     }
    bot.answer_callback_query(q.id).await?;
    Ok(())
}


/// Обработчик колбэка отмены активной операции (префикс cancel_op_)
pub async fn handle_cancel_active_op_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: Arc<E>, // Нужен для отмены ордера и продажи спота
    _state_storage: StateStorage, // Не нужен здесь
    running_operations: RunningOperations,
    _cfg: Arc<Config>, // Может понадобиться для QuoteCurrency
    db: Arc<Db>,
) -> anyhow::Result<()>
 where
     E: Exchange + Clone + Send + Sync + 'static,
 {
    if let (Some(data), Some(msg)) = (q.data, q.message) {
        let chat_id = msg.chat().id;
        if let Some(op_id_str) = data.strip_prefix(callback_data::PREFIX_CANCEL_ACTIVE_OP) {
            if let Ok(operation_id_to_cancel) = op_id_str.parse::<i64>() {
                 info!("User {} requested cancellation for active operation ID: {}", chat_id, operation_id_to_cancel);

                 let mut operation_info_opt: Option<RunningOperationInfo> = None;
                 let mut current_spot_order_id_to_cancel: Option<String> = None;
                 let mut filled_spot_qty_to_sell: f64 = 0.0;

                 // --- Блок для извлечения информации и удаления из мапы ---
                 {
                     let mut ops_guard: MutexGuard<'_, HashMap<(ChatId, i64), RunningOperationInfo>> = running_operations.lock().await;
                     // Пытаемся ИЗВЛЕЧЬ информацию об операции
                     if let Some(info) = ops_guard.remove(&(chat_id, operation_id_to_cancel)) {
                         // Получаем данные ПЕРЕД тем, как отпустить info
                         current_spot_order_id_to_cancel = info.current_spot_order_id.lock().await.clone();
                         filled_spot_qty_to_sell = *info.total_filled_spot_qty.lock().await;
                         operation_info_opt = Some(info); // info перемещается сюда
                         info!("op_id:{}: Found active operation, removed from map.", operation_id_to_cancel);
                     } else {
                          warn!("op_id:{}: Active operation not found in map for cancellation request.", operation_id_to_cancel);
                           bot.answer_callback_query(q.id).text("Операция уже завершена или отменена.").show_alert(true).await?;
                          // Можно обновить сообщение msg.id, если оно еще существует
                           let _ = bot.edit_message_text(chat_id, msg.id(), "ℹ️ Операция уже неактивна.")
                                      .reply_markup(navigation::make_main_menu_keyboard()) // Или клавиатуру со списком активных?
                                      .await;
                          return Ok(()); // Выходим
                     }
                 } // ops_guard разблокирован

                 // --- Если информация найдена, выполняем отмену ---
                 if let Some(op_info) = operation_info_opt {
                     let symbol = op_info.symbol.clone();
                     let bot_message_id_to_edit = MessageId(op_info.bot_message_id);

                      info!("op_id:{}: Aborting task...", operation_id_to_cancel);
                      op_info.handle.abort(); // Прерываем задачу hedger'a

                     // Редактируем сообщение об отмене
                     let cancelling_text = format!("⏳ Отмена операции ID:{} ({}) ...", operation_id_to_cancel, symbol);
                     let _ = bot.edit_message_text(chat_id, bot_message_id_to_edit, cancelling_text)
                                .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                                .await;

                     // --- Логика обработки отмены (упрощенная) ---
                     // TODO: Реализовать полную логику отмены ордеров и продажи спота, как в callbacks.old.txt
                     let mut final_error_msg: Option<String> = None;
                     let mut actual_sold_qty = 0.0;

                     // 1. Отмена спотового ордера (если есть)
                     if let Some(ref order_id) = current_spot_order_id_to_cancel {
                         info!("op_id:{}: Cancelling spot order {}", operation_id_to_cancel, order_id);
                         match exchange.cancel_order(&symbol, order_id).await {
                             Ok(_) => info!("op_id:{}: Spot order cancel request sent OK.", operation_id_to_cancel),
                             Err(e) => {
                                 warn!("op_id:{}: Spot order cancel FAILED: {}", operation_id_to_cancel, e);
                                 final_error_msg = Some(format!("Failed cancel spot order: {}", e));
                             }
                         }
                     }

                    // 2. Продажа исполненного спота (если это был хедж и что-то исполнено)
                    // ВАЖНО: Для Unhedge логика должна быть обратной - покупка фьюча!
                    if op_info.operation_type == OperationType::Hedge && filled_spot_qty_to_sell > ORDER_FILL_TOLERANCE {
                         warn!("op_id:{}: Spot sell logic upon cancellation is NOT IMPLEMENTED yet. Filled qty: {}", operation_id_to_cancel, filled_spot_qty_to_sell);
                         // TODO: Проверить баланс, продать по рынку `filled_spot_qty_to_sell`, обновить `actual_sold_qty`
                         // final_error_msg = Some("Spot sell on cancel not implemented".to_string());
                         actual_sold_qty = filled_spot_qty_to_sell; // Пока просто записываем то, что было исполнено
                    } else if op_info.operation_type == OperationType::Unhedge {
                         warn!("op_id:{}: Unhedge cancellation logic (buy futures?) NOT IMPLEMENTED yet.", operation_id_to_cancel);
                         // TODO: Нужна ли здесь покупка фьючерса для отмены расхеджа? Зависит от ТЗ.
                    }


                     // 3. Обновление статуса в БД
                     let final_db_status = "Cancelled";
                     // Определяем, какое количество записать в БД (зависит от типа операции и реализации продажи/покупки при отмене)
                     let qty_for_db = if op_info.operation_type == OperationType::Hedge { actual_sold_qty } else { 0.0 }; // Пример

                     // Устанавливаем специфичное сообщение об ошибке для случая отмены
                     let cancel_reason = Some("cancelled by user".to_string());
                     let final_error_text = final_error_msg.or(cancel_reason);


                     if let Err(e) = update_hedge_final_status(
                         db.as_ref(),
                         operation_id_to_cancel,
                         final_db_status,
                         None, // futures_order_id - нет при отмене
                         qty_for_db, // Исполненное кол-во (спота при хедже)
                         final_error_text.as_deref(),
                     ).await {
                         error!("op_id:{}: Failed DB update after cancellation: {}", operation_id_to_cancel, e);
                     } else {
                         info!("op_id:{}: DB status updated to '{}'", operation_id_to_cancel, final_db_status);
                     }

                     // 4. Финальное сообщение пользователю
                      let final_text = format!(
                         "❌ Операция ID:{} ({}) отменена пользователем.",
                         operation_id_to_cancel, symbol
                      );
                      let _ = bot.edit_message_text(chat_id, bot_message_id_to_edit, final_text)
                                 .reply_markup(navigation::make_main_menu_keyboard()) // Возврат в меню
                                 .await;
                 }
                 // Если op_info был None, мы уже вышли раньше

            } else {
                 error!("Failed to parse operation_id from cancel callback data: {}", op_id_str);
                 bot.answer_callback_query(q.id).text("Ошибка: Неверный ID операции.").await?;
            }
        } else {
             warn!("Invalid callback data format for cancel active operation: {}", data);
             bot.answer_callback_query(q.id).text("Неизвестное действие.").await?;
        }
    } else {
         warn!("CallbackQuery missing data or message in handle_cancel_active_op_callback");
         bot.answer_callback_query(q.id).await?;
    }
    Ok(())
}