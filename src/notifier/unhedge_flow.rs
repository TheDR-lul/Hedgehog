// src/notifier/unhedge_flow.rs

use super::{
    Command, StateStorage, UserState, RunningOperations, RunningOperationInfo, OperationType, callback_data, // Импорт общих типов
    navigation, // Для кнопок Назад/Отмена
    wallet_info, // Может понадобиться для получения баланса при выборе актива
    progress, // Для колбэка прогресса (если будем делать для unhedge)
    utils,    // Для общих утилит
};
use crate::config::Config;
use crate::exchange::Exchange;
use crate::storage::{Db, HedgeOperation, get_completed_unhedged_ops_for_symbol, mark_hedge_as_unhedged}; // Функции из storage
use crate::hedger::{Hedger, ORDER_FILL_TOLERANCE}; // Hedger и константа
use std::sync::Arc;
use std::time::Duration; // Для sleep
use chrono::{Utc, TimeZone, LocalResult}; // Для форматирования даты
use tokio::sync::Mutex as TokioMutex;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId, ParseMode,
};
use teloxide::utils::command::BotCommands;
use tracing::{info, warn, error};


// --- Вспомогательные функции ---

/// Создает клавиатуру для выбора операции расхеджирования
fn make_unhedge_selection_keyboard(operations: &[HedgeOperation]) -> InlineKeyboardMarkup {
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut sorted_ops = operations.to_vec(); // Клонируем для сортировки
    sorted_ops.sort_by_key(|op| std::cmp::Reverse(op.id)); // Сортируем (новые сверху)

    for op in &sorted_ops {
        let timestamp_dt = match Utc.timestamp_opt(op.start_timestamp, 0) {
            LocalResult::Single(dt) => dt,
            _ => Utc::now(), // Fallback
        };
        let date_str = timestamp_dt.format("%y-%m-%d %H:%M").to_string(); // Краткий формат
        // Используем target_futures_qty (нетто)
        let label = format!(
            "ID:{} {:.4} {} ({})", // Меньше знаков после запятой
            op.id, op.target_futures_qty, op.base_symbol, date_str
        );
        let callback_data = format!("{}{}", callback_data::PREFIX_UNHEDGE_OP_SELECT, op.id);
        buttons.push(vec![InlineKeyboardButton::callback(label, callback_data)]);
    }
    buttons.push(vec![InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)]); // Или назад к выбору актива? Пока в меню.
    buttons.push(vec![InlineKeyboardButton::callback("❌ Отмена", callback_data::CANCEL_DIALOG)]);
    InlineKeyboardMarkup::new(buttons)
}

/// Создает клавиатуру для подтверждения расхеджирования
fn make_unhedge_confirmation_keyboard(operation_id: i64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("✅ Да, расхеджировать", format!("{}{}", callback_data::PREFIX_UNHEDGE_CONFIRM, "yes")),
            InlineKeyboardButton::callback("❌ Нет, отмена", callback_data::CANCEL_DIALOG),
        ],
        // Можно добавить кнопку Назад к выбору операции
        // vec![InlineKeyboardButton::callback("⬅️ Назад", callback_data::???)]
    ])
}


/// Показывает пользователю список операций для расхеджирования
async fn prompt_operation_selection(
    bot: &Bot,
    chat_id: ChatId,
    symbol: &str,
    operations: Vec<HedgeOperation>, // Не &Vec, т.к. будем хранить в состоянии
    state_storage: StateStorage,
    message_id_to_edit: Option<MessageId>,
) -> anyhow::Result<()> {

     let text = format!(
         "Найдено несколько ({}) завершенных операций хеджирования для {}. Выберите одну для расхеджирования:",
         operations.len(), symbol
     );
     let kb = make_unhedge_selection_keyboard(&operations);

     let bot_msg_id = if let Some(msg_id) = message_id_to_edit {
         bot.edit_message_text(chat_id, msg_id, text).reply_markup(kb).await?;
         msg_id
     } else {
         bot.send_message(chat_id, text).reply_markup(kb).await?.id
     };

     // Сохраняем состояние ожидания выбора операции
     {
         let mut state_guard = state_storage.write().expect("Lock failed");
         state_guard.insert(chat_id, UserState::AwaitingUnhedgeOperationSelection {
             symbol: symbol.to_string(),
             operations, // Перемещаем вектор в состояние
             last_bot_message_id: Some(bot_msg_id.0),
         });
         info!("User state for {} set to AwaitingUnhedgeOperationSelection for symbol {}", chat_id, symbol);
     }
     Ok(())
}


/// Запускает фоновую задачу расхеджирования
async fn spawn_unhedge_task<E>(
    bot: Bot,
    exchange: Arc<E>,
    cfg: Arc<Config>,
    db: Arc<Db>,
    running_operations: RunningOperations, // Добавили для отслеживания
    chat_id: ChatId,
    op_to_unhedge: HedgeOperation, // Исходная операция хеджа
    message_id_to_edit: MessageId, // Сообщение "Запускаю..." или "Подтвердите" для редактирования
)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let hedger = Hedger::new((*exchange).clone(), (*cfg).clone());
    let original_op_id = op_to_unhedge.id;
    let symbol = op_to_unhedge.base_symbol.clone();
    let bot_clone = bot.clone(); // Клонируем для использования в задаче

    // TODO: Реализовать механизм отслеживания и отмены задачи расхеджирования
    // let task_handle = ...;
    // let info = RunningOperationInfo { ... type: Unhedge ... };
    // running_operations.lock().await.insert((chat_id, original_op_id), info); // Нужен уникальный ID для unhedge?
    // Пока просто запускаем

    tokio::spawn(async move {
        match hedger.run_unhedge(op_to_unhedge, db.as_ref()).await {
            Ok((sold_spot_qty, bought_fut_qty)) => {
                info!("Unhedge OK for original op_id: {}", original_op_id);
                let text = format!(
                    "✅ Расхеджирование {} (из операции ID:{}) завершено:\n\n🟢 Спот продано: {:.8}\n🔴 Фьюч куплено: {:.8}",
                    symbol, original_op_id, sold_spot_qty, bought_fut_qty
                );
                // Редактируем сообщение об успехе и показываем главное меню
                let _ = bot_clone.edit_message_text(chat_id, message_id_to_edit, text)
                           .reply_markup(navigation::make_main_menu_keyboard())
                           .await
                           .map_err(|e| warn!("op_id:{}: Failed edit success unhedge message: {}", original_op_id, e));
            }
            Err(e) => {
                error!("Unhedge FAILED for original op_id: {}: {}", original_op_id, e);
                // Статус исходной операции в БД не меняем (она остается Completed),
                // но сообщаем об ошибке пользователю
                let error_text = format!("❌ Ошибка расхеджирования операции ID:{}: {}", original_op_id, e);
                let _ = bot_clone.edit_message_text(chat_id, message_id_to_edit, error_text)
                           .reply_markup(navigation::make_main_menu_keyboard())
                           .await
                           .map_err(|e| warn!("op_id:{}: Failed edit error unhedge message: {}", original_op_id, e));
            }
        }
        // TODO: Удалить из running_operations после завершения
        // running_operations.lock().await.remove(&(chat_id, original_op_id));
    });
}


// --- Обработчики команд и колбэков ---

/// Обработчик команды /unhedge [SYMBOL]
pub async fn handle_unhedge_command<E>(
    bot: Bot,
    msg: Message,
    symbol_arg: String, // Может быть пустой
    exchange: Arc<E>,
    state_storage: StateStorage,
    running_operations: RunningOperations, // TODO: Использовать для проверки конфликтов?
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let symbol = symbol_arg.trim().to_uppercase();

    // Сброс состояния и чистка чата (аналогично /hedge)
    // TODO: Вынести в утилиту
    let mut previous_bot_message_id: Option<i32> = None;
     {
        let mut state_guard = state_storage.write().expect("Lock failed");
        if let Some(old_state) = state_guard.get(&chat_id) {
             previous_bot_message_id = match old_state {
                 UserState::AwaitingUnhedgeAssetSelection { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingUnhedgeOperationSelection { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingUnhedgeConfirmation { last_bot_message_id, .. } => *last_bot_message_id,
                 // ... другие состояния ...
                 _ => None,
             };
        }
        if !matches!(state_guard.get(&chat_id), Some(UserState::None) | None) {
            info!("Resetting state for {} due to /unhedge command", chat_id);
            state_guard.insert(chat_id, UserState::None);
        }
    }
    if let Some(bot_msg_id) = previous_bot_message_id {
        if let Err(e) = bot.delete_message(chat_id, MessageId(bot_msg_id)).await { warn!("Failed delete prev bot msg: {}", e); }
    }
    let user_msg_id = msg.id;
    if let Err(e) = bot.delete_message(chat_id, user_msg_id).await { warn!("Failed delete user command msg: {}", e); }


    if symbol.is_empty() {
        // Если символ не указан, запускаем выбор актива
        info!("Processing /unhedge command without symbol for chat_id: {}", chat_id);
        prompt_unhedge_asset_selection(bot, chat_id, state_storage, exchange, cfg, db, None).await?;
    } else {
        // Если символ указан, ищем операции для него
        info!("Processing /unhedge command for chat_id: {}, symbol: {}", chat_id, symbol);
        find_and_process_unhedge_operations(bot, chat_id, symbol, state_storage, exchange, cfg, db, running_operations, None).await?;
    }

    Ok(())
}

/// Обработчик колбэка кнопки "Расхеджировать" из главного меню
pub async fn handle_start_unhedge_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: Arc<E>,
    state_storage: StateStorage,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
     if let Some(msg) = q.message {
        let chat_id = msg.chat.id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::START_UNHEDGE, chat_id);
        // Запускаем выбор актива, редактируя текущее сообщение
        prompt_unhedge_asset_selection(bot, chat_id, state_storage, exchange, cfg, db, Some(msg.id)).await?;
    } else {
        warn!("CallbackQuery missing message in handle_start_unhedge_callback");
    }
    bot.answer_callback_query(q.id).await?;
    Ok(())
}


/// Запрашивает выбор актива для расхеджирования (если их несколько)
async fn prompt_unhedge_asset_selection<E>(
    bot: Bot,
    chat_id: ChatId,
    state_storage: StateStorage,
    exchange: Arc<E>,
    cfg: Arc<Config>,
    db: Arc<Db>,
    message_id_to_edit: Option<MessageId>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    info!("Prompting asset selection for unhedge, chat_id: {}", chat_id);
    // TODO: Реализовать логику определения активов, по которым ЕСТЬ завершенные хеджи
    // 1. Получить все HedgeOperation с status='Completed' и unhedged_op_id IS NULL для chat_id
    // 2. Сгруппировать их по base_symbol
    // 3. Если символ один - сразу перейти к prompt_operation_selection
    // 4. Если символов несколько - показать кнопки с этими символами

    // --- Заглушка ---
    warn!("Asset selection for unhedge is not fully implemented yet. Proceeding as if BTC was selected.");
    let symbol = "BTC".to_string(); // Пример
    find_and_process_unhedge_operations(
        bot, chat_id, symbol, state_storage, exchange, cfg, db,
        Arc::new(TokioMutex::new(Default::default())), // Передаем пустой RunningOperations
        message_id_to_edit
    ).await?;
    // --- Конец Заглушки ---

    Ok(())
}

/// Обработчик колбэка выбора актива для расхеджа (префикс u_asset_)
pub async fn handle_unhedge_asset_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: Arc<E>,
    state_storage: StateStorage,
    cfg: Arc<Config>,
    db: Arc<Db>,
    // running_operations: RunningOperations, // Не нужен здесь, нужен при запуске
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let (Some(data), Some(msg)) = (q.data, q.message) {
        let chat_id = msg.chat.id;
        if let Some(symbol) = data.strip_prefix(callback_data::PREFIX_UNHEDGE_ASSET) {
             info!("User {} selected asset {} for unhedge via callback", chat_id, symbol);

            // Проверяем состояние
             let is_correct_state = matches!(
                 state_storage.read().expect("Lock failed").get(&chat_id),
                 Some(UserState::AwaitingUnhedgeAssetSelection { .. })
             );

             if is_correct_state {
                 // Ищем операции для выбранного символа
                 find_and_process_unhedge_operations(
                     bot, chat_id, symbol.to_string(), state_storage, exchange, cfg, db,
                     Arc::new(TokioMutex::new(Default::default())), // Передаем пустой RunningOperations
                     Some(msg.id)
                 ).await?;
             } else {
                 warn!("User {} clicked unhedge asset button but was in wrong state", chat_id);
                  { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
                  let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id)).await;
                  bot.answer_callback_query(q.id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                  return Ok(());
             }

        } else {
             warn!("Invalid callback data format for unhedge asset selection: {}", data);
        }
    } else {
         warn!("CallbackQuery missing data or message in handle_unhedge_asset_callback");
    }
     bot.answer_callback_query(q.id).await?;
     Ok(())
}


/// Ищет операции для символа и либо запускает одну, либо предлагает выбор
async fn find_and_process_unhedge_operations<E>(
    bot: Bot,
    chat_id: ChatId,
    symbol: String,
    state_storage: StateStorage,
    exchange: Arc<E>,
    cfg: Arc<Config>,
    db: Arc<Db>,
    running_operations: RunningOperations,
    message_id_to_edit: Option<MessageId>,
) -> anyhow::Result<()>
where
     E: Exchange + Clone + Send + Sync + 'static,
{
    info!("Looking for completed hedges for symbol {} for chat_id {}", symbol, chat_id);
    let loading_text = format!("⏳ Поиск завершенных операций для {}...", symbol);
    let mut bot_msg_id_opt = message_id_to_edit;

    if let Some(msg_id) = bot_msg_id_opt {
         let _ = bot.edit_message_text(chat_id, msg_id, loading_text).await;
    } else {
         bot_msg_id_opt = Some(bot.send_message(chat_id, loading_text).await?.id);
    }
    let bot_msg_id = bot_msg_id_opt.ok_or_else(|| anyhow::anyhow!("Failed to get bot message ID for unhedge status"))?;

    match get_completed_unhedged_ops_for_symbol(db.as_ref(), chat_id.0, &symbol).await {
        Ok(operations) => {
            if operations.is_empty() {
                let text = format!("ℹ️ Не найдено завершенных операций хеджирования для {}, которые можно было бы расхеджировать.", symbol);
                 let kb = InlineKeyboardMarkup::new(vec![vec![
                     InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
                 ]]);
                bot.edit_message_text(chat_id, bot_msg_id, text).reply_markup(kb).await?;
                { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); } // Сброс состояния
            } else if operations.len() == 1 {
                 // Если найдена ровно одна операция, переходим к подтверждению
                 let op_to_unhedge = operations.into_iter().next().unwrap(); // Безопасно
                 prompt_unhedge_confirmation(bot, chat_id, op_to_unhedge, state_storage, Some(bot_msg_id)).await?;
            } else {
                 // Если найдено несколько операций, предлагаем выбрать
                 prompt_operation_selection(bot, chat_id, &symbol, operations, state_storage, Some(bot_msg_id)).await?;
            }
        }
        Err(e) => {
            error!("Failed to query hedge operations for {}: {}", symbol, e);
            let error_text = format!("❌ Ошибка при поиске операций хеджирования в БД: {}", e);
            let kb = InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
            ]]);
            bot.edit_message_text(chat_id, bot_msg_id, error_text).reply_markup(kb).await?;
            { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); } // Сброс состояния
            return Err(e.into()); // Возвращаем ошибку
        }
    }
    Ok(())
}

/// Обработчик колбэка выбора операции для расхеджа (префикс u_opsel_)
pub async fn handle_unhedge_select_op_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    _exchange: Arc<E>, // Не нужен здесь
    state_storage: StateStorage,
    _running_operations: RunningOperations, // Не нужен здесь
    _cfg: Arc<Config>, // Не нужен здесь
    _db: Arc<Db>, // Не нужен здесь
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let (Some(data), Some(msg)) = (q.data, q.message) {
        let chat_id = msg.chat.id;
        if let Some(op_id_str) = data.strip_prefix(callback_data::PREFIX_UNHEDGE_OP_SELECT) {
            if let Ok(operation_id) = op_id_str.parse::<i64>() {
                 info!("User {} selected operation ID {} to unhedge", chat_id, operation_id);

                 // Получаем операцию из состояния
                 let mut operation_to_confirm: Option<HedgeOperation> = None;
                 { // Блок для чтения состояния
                     let state_guard = state_storage.read().expect("Lock failed");
                     if let Some(UserState::AwaitingUnhedgeOperationSelection { operations, .. }) = state_guard.get(&chat_id) {
                         // Ищем операцию в списке из состояния
                         operation_to_confirm = operations.iter().find(|op| op.id == operation_id).cloned();
                     } else {
                         warn!("User {} clicked unhedge operation selection but was in wrong state", chat_id);
                         { drop(state_guard); state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
                         let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id)).await;
                         bot.answer_callback_query(q.id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                         return Ok(());
                     }
                 } // Блокировка чтения снята

                 if let Some(op) = operation_to_confirm {
                     // Переходим к подтверждению этой операции
                     prompt_unhedge_confirmation(bot, chat_id, op, state_storage, Some(msg.id)).await?;
                 } else {
                     error!("Operation ID {} not found in state for chat_id {}", operation_id, chat_id);
                     // Это не должно происходить, если состояние верное
                      { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
                      let _ = bot.edit_message_text(chat_id, msg.id, "❌ Ошибка: Выбранная операция не найдена. Попробуйте снова.")
                                 .reply_markup(navigation::make_main_menu_keyboard())
                                 .await;
                 }

            } else {
                 error!("Failed to parse operation_id from callback data: {}", data);
                  let _ = bot.edit_message_text(chat_id, msg.id, "❌ Ошибка: Неверный ID операции в кнопке.")
                             .reply_markup(navigation::make_main_menu_keyboard())
                             .await;
                   { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
            }
        } else {
            warn!("Invalid callback data format for unhedge operation selection: {}", data);
        }
    } else {
         warn!("CallbackQuery missing data or message in handle_unhedge_select_op_callback");
    }
    bot.answer_callback_query(q.id).await?;
    Ok(())
}


/// Запрашивает подтверждение перед расхеджированием
async fn prompt_unhedge_confirmation(
    bot: Bot,
    chat_id: ChatId,
    operation_to_unhedge: HedgeOperation, // Не &HedgeOperation, т.к. ID нужен для состояния
    state_storage: StateStorage,
    message_id_to_edit: Option<MessageId>,
) -> anyhow::Result<()> {
    let operation_id = operation_to_unhedge.id;
    let symbol = operation_to_unhedge.base_symbol;
    let fut_qty = operation_to_unhedge.target_futures_qty; // Нетто фьючерса

    // TODO: Получить текущий баланс спота, чтобы показать, сколько будет продано
    let spot_sell_qty_approx = operation_to_unhedge.spot_filled_qty; // Приблизительно

    let text = format!(
        "Подтвердите расхеджирование операции ID:{}\n\
        Символ: {}\n\
        Будет продано ~{:.8} {} спота (по рынку или лимиткой).\n\
        Будет куплено {:.8} {} фьючерса (по рынку).\n\n\
        Вы уверены?",
        operation_id, symbol, spot_sell_qty_approx, symbol, fut_qty, symbol
    );
    let kb = make_unhedge_confirmation_keyboard(operation_id);

    let bot_msg_id = if let Some(msg_id) = message_id_to_edit {
        bot.edit_message_text(chat_id, msg_id, text).reply_markup(kb).await?;
        msg_id
    } else {
        bot.send_message(chat_id, text).reply_markup(kb).await?.id
    };

    // Устанавливаем состояние ожидания подтверждения
    {
        let mut state_guard = state_storage.write().expect("Lock failed");
        state_guard.insert(chat_id, UserState::AwaitingUnhedgeConfirmation {
            operation_id,
            last_bot_message_id: Some(bot_msg_id.0),
        });
        info!("User state for {} set to AwaitingUnhedgeConfirmation for op_id {}", chat_id, operation_id);
    }
    Ok(())
}


/// Обработчик колбэка подтверждения расхеджа (префикс u_conf_)
pub async fn handle_unhedge_confirm_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: Arc<E>,
    state_storage: StateStorage,
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let (Some(data), Some(msg)) = (q.data, q.message) {
        let chat_id = msg.chat.id;
        if let Some(payload) = data.strip_prefix(callback_data::PREFIX_UNHEDGE_CONFIRM) {
            if payload == "yes" {
                info!("User {} confirmed unhedge operation", chat_id);

                // Получаем ID операции из состояния
                let operation_id_to_unhedge = {
                    let state_guard = state_storage.read().expect("Lock failed");
                     match state_guard.get(&chat_id) {
                         Some(UserState::AwaitingUnhedgeConfirmation { operation_id, .. }) => *operation_id,
                         _ => {
                             warn!("User {} confirmed unhedge but was in wrong state", chat_id);
                             bot.answer_callback_query(q.id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                             let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id)).await;
                              { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
                             return Ok(());
                         }
                     }
                };
                 // Сбрасываем состояние ПЕРЕД запуском
                 { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }

                 // Получаем детали исходной операции из БД
                 match crate::storage::get_hedge_operation_by_id(db.as_ref(), operation_id_to_unhedge).await {
                     Ok(Some(original_op)) => {
                         // Проверяем еще раз статус на всякий случай
                         if original_op.status != "Completed" || original_op.unhedged_op_id.is_some() {
                              error!("Attempted to unhedge already unhedged or invalid op_id: {}", operation_id_to_unhedge);
                              let _ = bot.edit_message_text(chat_id, msg.id, "❌ Операция уже расхеджирована или недействительна.")
                                         .reply_markup(navigation::make_main_menu_keyboard())
                                         .await;
                         } else {
                             // Показываем индикатор запуска
                             let waiting_text = format!("⏳ Запуск расхеджирования операции ID:{}...", operation_id_to_unhedge);
                             bot.edit_message_text(chat_id, msg.id, waiting_text)
                                .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                                .await?;

                             // Запускаем фоновую задачу
                             spawn_unhedge_task(
                                 bot.clone(),
                                 exchange.clone(),
                                 cfg.clone(),
                                 db.clone(),
                                 running_operations.clone(),
                                 chat_id,
                                 original_op, // Передаем всю операцию
                                 msg.id,      // ID сообщения для редактирования результата
                             ).await;
                         }
                     }
                     Ok(None) => {
                         error!("Hedge operation ID {} not found in DB for unhedge confirmation", operation_id_to_unhedge);
                          let _ = bot.edit_message_text(chat_id, msg.id, "❌ Ошибка: Операция не найдена в БД.")
                                     .reply_markup(navigation::make_main_menu_keyboard())
                                     .await;
                     }
                     Err(e) => {
                          error!("DB error getting hedge operation {} for unhedge: {}", operation_id_to_unhedge, e);
                          let _ = bot.edit_message_text(chat_id, msg.id, "❌ Ошибка БД при получении деталей операции.")
                                     .reply_markup(navigation::make_main_menu_keyboard())
                                     .await;
                     }
                 }

            } else if payload == "no" {
                 info!("User {} cancelled unhedge at confirmation", chat_id);
                 navigation::handle_cancel_dialog(bot, q, state_storage).await?;
                 return Ok(()); // Выходим, колбэк обработан
            } else {
                 warn!("Invalid payload for unhedge confirmation callback: {}", payload);
            }
        } else {
            warn!("Invalid callback data format for unhedge confirmation: {}", data);
        }
    } else {
        warn!("CallbackQuery missing data or message in handle_unhedge_confirm_callback");
    }
    bot.answer_callback_query(q.id).await?;
    Ok(())
}