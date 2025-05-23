// src/notifier/unhedge_flow.rs
use crate::notifier::{
    StateStorage, UserState, RunningOperations, callback_data, navigation,
};
use crate::config::Config;
use crate::exchange::Exchange;
use crate::storage::{
    Db, HedgeOperation, get_completed_unhedged_ops_for_symbol,
    get_all_completed_unhedged_ops, get_hedge_operation_by_id,
};
// --- ДОБАВЛЕНЫ НУЖНЫЕ ИМПОРТЫ ---
use crate::hedger::{
    Hedger, HedgeProgressCallback, HedgeProgressUpdate, ORDER_FILL_TOLERANCE
};
use std::{collections::HashMap, sync::Arc};
use chrono::{Utc, TimeZone, LocalResult};
use futures::future::FutureExt; // Для .boxed()
// --- КОНЕЦ ДОБАВЛЕННЫХ ИМПОРТОВ ---
// use tokio::sync::Mutex as TokioMutex;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId,
};
use tracing::{info, warn, error};


// --- Вспомогательные функции --- (без изменений)

fn make_unhedge_asset_selection_keyboard(available_symbols: &[String]) -> InlineKeyboardMarkup {
    let mut sorted_symbols = available_symbols.to_vec();
    sorted_symbols.sort();
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = sorted_symbols
        .iter()
        .map(|symbol| {
            let callback_data_asset = format!("{}{}", callback_data::PREFIX_UNHEDGE_ASSET, symbol);
            vec![InlineKeyboardButton::callback(format!("🪙 {}", symbol), callback_data_asset)]
        })
        .collect();
    buttons.push(vec![InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)]);
    buttons.push(vec![InlineKeyboardButton::callback("❌ Отмена", callback_data::CANCEL_DIALOG)]);
    InlineKeyboardMarkup::new(buttons)
}

fn make_unhedge_selection_keyboard(operations: &[HedgeOperation]) -> InlineKeyboardMarkup {
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut sorted_ops = operations.to_vec();
    sorted_ops.sort_by_key(|op| std::cmp::Reverse(op.id));
    for op in &sorted_ops {
        let timestamp_dt = match Utc.timestamp_opt(op.start_timestamp, 0) {
            LocalResult::Single(dt) => dt,
            _ => Utc::now(),
        };
        let date_str = timestamp_dt.format("%y-%m-%d %H:%M").to_string();
        let label = format!("ID:{} {:.4} {} ({})", op.id, op.target_futures_qty, op.base_symbol, date_str);
        let callback_data_op = format!("{}{}", callback_data::PREFIX_UNHEDGE_OP_SELECT, op.id);
        buttons.push(vec![InlineKeyboardButton::callback(label, callback_data_op)]);
    }
    buttons.push(vec![InlineKeyboardButton::callback("⬅️ Назад (в гл. меню)", callback_data::BACK_TO_MAIN)]);
    buttons.push(vec![InlineKeyboardButton::callback("❌ Отмена", callback_data::CANCEL_DIALOG)]);
    InlineKeyboardMarkup::new(buttons)
}

fn make_unhedge_confirmation_keyboard(_operation_id: i64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("✅ Да, расхеджировать", format!("{}{}", callback_data::PREFIX_UNHEDGE_CONFIRM, "yes")),
            InlineKeyboardButton::callback("❌ Нет, отмена", callback_data::CANCEL_DIALOG),
        ],
    ])
}

async fn prompt_operation_selection(
    bot: &Bot,
    chat_id: ChatId,
    symbol: &str,
    operations: Vec<HedgeOperation>,
    state_storage: StateStorage,
    message_id_to_edit: Option<MessageId>,
) -> anyhow::Result<()> {

      let text = format!("Найдено {} завершенных операций для {}. Выберите одну для расхеджирования:", operations.len(), symbol);
      let keyboard = make_unhedge_selection_keyboard(&operations);

      let bot_msg_id = if let Some(msg_id) = message_id_to_edit {
          bot.edit_message_text(chat_id, msg_id, text).reply_markup(keyboard).await?;
          msg_id
      } else {
          bot.send_message(chat_id, text).reply_markup(keyboard).await?.id
      };

      {
          let mut state_guard = state_storage.write().await;
          state_guard.insert(chat_id, UserState::AwaitingUnhedgeOperationSelection {
              symbol: symbol.to_string(),
              operations,
              last_bot_message_id: Some(bot_msg_id.0),
          });
          info!("User state for {} set to AwaitingUnhedgeOperationSelection for symbol {}", chat_id, symbol);
      }
      Ok(())
}

/// Запускает фоновую задачу расхеджирования (без изменений)
async fn spawn_unhedge_task<E>(
    bot: Bot,
    exchange: Arc<E>,
    cfg: Arc<Config>,
    db: Arc<Db>,
    _running_operations: RunningOperations, // Пока не используется для отслеживания unhedge
    chat_id: ChatId,
    op_to_unhedge: HedgeOperation, // Принимаем всю операцию
    message_id_to_edit: MessageId,
)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let hedger = Hedger::new((*exchange).clone(), (*cfg).clone());
    let original_op_id = op_to_unhedge.id;
    let symbol = op_to_unhedge.base_symbol.clone(); // Клон символа для задачи

    // --- Клоны для колбэка прогресса ---
    let bot_for_callback = bot.clone();
    let symbol_for_callback = symbol.clone();
    let cfg_for_callback = cfg.clone();
    let original_op_for_callback = op_to_unhedge.clone();
    // --- Конец клонов для колбэка ---

    // --- Клоны для основной задачи spawn ---
    let bot_for_spawn = bot.clone();
    let db_for_spawn = db.clone();
    // `op_to_unhedge` и `symbol` будут перемещены в spawn ниже
    // --- Конец клонов для основной задачи spawn ---


    // --- Создание колбэка прогресса для расхеджирования ---
    let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
        // Используем клоны, созданные специально для колбэка
        let bot_cb = bot_for_callback.clone(); // Клонируем еще раз внутри, т.к. async move
        let qc = cfg_for_callback.quote_currency.clone(); // Используем клон cfg
        let symbol_cb = symbol_for_callback.clone(); // Используем клон symbol
        let msg_id_cb = message_id_to_edit; // Копируем ID сообщения
        let chat_id_cb = chat_id; // Копируем ID чата
        let operation_id_cb = original_op_id; // Копируем ID операции
        // Используем целевое количество спота из оригинальной операции для расчета общего %
        let _overall_target_qty = original_op_for_callback.spot_filled_qty;

        // --- УДАЛЕНО: Неиспользуемая переменная qc ---
        // let qc = cfg_clone.quote_currency.clone();

        async move {
            // Прогресс текущего ордера
            let current_order_filled_percent = if update.target_qty > ORDER_FILL_TOLERANCE {
                (update.filled_qty / update.target_qty) * 100.0 } else { 0.0 };

            let progress_bar_len = 10;
            let filled_blocks = (current_order_filled_percent / (100.0 / progress_bar_len as f64)).round() as usize;
            let empty_blocks = progress_bar_len - filled_blocks;
            let progress_bar = format!("[{}{}]", "█".repeat(filled_blocks), "░".repeat(empty_blocks));
            let status_text = if update.is_replacement { "(Ордер переставлен)" } else { "" };

            // --- Адаптированный текст для Расхеджирования ---
            let text = format!(
                 "⏳ Расхеджирование ID:{} {} ({}) в процессе...\nРын.цена: {:.2}\nОрдер на ПРОДАЖУ: {:.2} {}\nИсполнено (тек.ордер): {:.6}/{:.6} ({:.1}%)",
                 operation_id_cb, progress_bar, symbol_cb, // Используем symbol_cb
                 update.current_spot_price, update.new_limit_price, status_text,
                 update.filled_qty, update.target_qty, current_order_filled_percent
                 // Можно добавить общий прогресс, если передавать cumulative_filled_qty в update
                 // / {:.6} (Общий: {:.1}%)", ..., _overall_target_qty, overall_filled_percent
            );
            // --- Конец адаптации текста ---

            // Кнопка отмены (пока не работает для unhedge, так как нет отслеживания в RunningOperations)
            // let cancel_callback_data = format!("{}{}", callback_data::PREFIX_CANCEL_ACTIVE_OP, operation_id_cb);
            // let cancel_button = InlineKeyboardButton::callback("❌ Отменить эту операцию", cancel_callback_data);
            // let kb = InlineKeyboardMarkup::new(vec![vec![cancel_button]]);
            let kb = InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()); // Пока без кнопки отмены

            if let Err(e) = bot_cb.edit_message_text(chat_id_cb, msg_id_cb, text) // Используем bot_cb
                .reply_markup(kb)
                .await {
                // Игнорируем ошибку "message is not modified"
                if !e.to_string().contains("message is not modified") {
                    warn!("op_id:{}: Unhedge Progress callback failed: {}", operation_id_cb, e);
                }
            }
            Ok(())
        }.boxed() // Используем .boxed() для преобразования в BoxFuture
    });
    // --- Конец колбэка прогресса ---

    tokio::spawn(async move {
        // --- Передаем колбэк в run_unhedge ---
        // `op_to_unhedge` перемещается сюда
        // `db_for_spawn` перемещается сюда
        // `progress_callback` перемещается сюда
        match hedger.run_unhedge(op_to_unhedge, db_for_spawn.as_ref(), progress_callback).await {
            Ok((sold_spot_qty, bought_fut_qty)) => {
                info!("Unhedge OK for original op_id: {}", original_op_id);
                let text = format!(
                    "✅ Расхеджирование {} (из операции ID:{}) завершено:\n\n🟢 Спот продано: {:.8}\n🔴 Фьюч куплено: {:.8}",
                    symbol, original_op_id, sold_spot_qty, bought_fut_qty // `symbol` перемещен сюда
                );
                // Редактируем исходное сообщение с результатом
                // `bot_for_spawn` перемещается сюда
                let _ = bot_for_spawn.edit_message_text(chat_id, message_id_to_edit, text)
                             .reply_markup(navigation::make_main_menu_keyboard())
                             .await
                             .map_err(|e| warn!("op_id:{}: Failed edit success unhedge message: {}", original_op_id, e));
            }
            Err(e) => {
                error!("Unhedge FAILED for original op_id: {}: {}", original_op_id, e);
                let error_text = format!("❌ Ошибка расхеджирования операции ID:{}: {}", original_op_id, e);
                // Редактируем исходное сообщение с ошибкой
                // `bot_for_spawn` используется здесь (если не был использован в Ok)
                let _ = bot_for_spawn.edit_message_text(chat_id, message_id_to_edit, error_text)
                             .reply_markup(navigation::make_main_menu_keyboard())
                             .await
                             .map_err(|e| warn!("op_id:{}: Failed edit error unhedge message: {}", original_op_id, e));
            }
        }
        // TODO: Удалить информацию об операции из running_operations, если она туда добавлялась для unhedge
        // (Пока не добавлялась, т.к. нет отмены для unhedge)
    });
} // Конец spawn_unhedge_task
/// Определяет, нужно ли выбирать актив или можно сразу показать операции
async fn start_unhedge_asset_or_op_selection(
    bot: Bot,
    chat_id: ChatId,
    state_storage: StateStorage, // Тип StateStorage уже Arc<TokioRwLock<...>>
    db: Arc<Db>,
    message_id_to_edit: Option<MessageId>,
) -> anyhow::Result<()>
{
    info!("Starting unhedge flow for chat_id: {}", chat_id);
    let loading_text = "⏳ Поиск доступных операций для расхеджирования...";
    let mut current_message_id = message_id_to_edit;

    if let Some(msg_id) = current_message_id {
        let _ = bot.edit_message_text(chat_id, msg_id, loading_text)
             .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
             .await;
    } else {
        current_message_id = Some(bot.send_message(chat_id, loading_text).await?.id);
    }
    let bot_msg_id = current_message_id.ok_or_else(|| anyhow::anyhow!("Failed to obtain message ID for unhedge flow"))?;

    match get_all_completed_unhedged_ops(db.as_ref(), chat_id.0).await {
        Ok(all_operations) => {
            if all_operations.is_empty() {
                info!("No completed hedge operations found for chat_id: {}", chat_id);
                let text = "ℹ️ Не найдено завершенных операций хеджирования, которые можно было бы расхеджировать.";
                let keyboard = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
                ]]);
                bot.edit_message_text(chat_id, bot_msg_id, text).reply_markup(keyboard).await?;
                // <<< ИСПРАВЛЕНО: .await >>>
                { state_storage.write().await.insert(chat_id, UserState::None); }
                return Ok(());
            }

            let mut ops_by_symbol: HashMap<String, Vec<HedgeOperation>> = HashMap::new();
            for op in all_operations {
                ops_by_symbol.entry(op.base_symbol.clone()).or_default().push(op);
            }

            let mut available_symbols: Vec<String> = ops_by_symbol.keys().cloned().collect();
            available_symbols.sort();

            if available_symbols.len() == 1 {
                let symbol = available_symbols.first().unwrap().clone();
                let symbol_operations = ops_by_symbol.remove(&symbol).unwrap();
                info!("Found {} operations for single symbol {} for chat_id: {}", symbol_operations.len(), symbol, chat_id);

                if symbol_operations.len() == 1 {
                    let op_to_confirm = symbol_operations.into_iter().next().unwrap();
                    prompt_unhedge_confirmation(&bot, chat_id, op_to_confirm, state_storage, Some(bot_msg_id)).await?;
                } else {
                    prompt_operation_selection(&bot, chat_id, &symbol, symbol_operations, state_storage, Some(bot_msg_id)).await?;
                }

            } else {
                info!("Found operations for multiple symbols ({:?}) for chat_id: {}. Prompting asset selection.", available_symbols, chat_id);
                let text = "Найдены завершенные операции хеджирования по нескольким активам. Выберите актив для расхеджирования:";
                let keyboard = make_unhedge_asset_selection_keyboard(&available_symbols);
                bot.edit_message_text(chat_id, bot_msg_id, text).reply_markup(keyboard).await?;

                {
                    // <<< ИСПРАВЛЕНО: .await >>>
                    let mut state_guard = state_storage.write().await;
                    state_guard.insert(chat_id, UserState::AwaitingUnhedgeAssetSelection {
                        last_bot_message_id: Some(bot_msg_id.0),
                    });
                    info!("User state for {} set to AwaitingUnhedgeAssetSelection", chat_id);
                } // Блокировка записи освобождается здесь
            }
        }
        Err(e) => {
             error!("Failed query all hedge operations for chat_id {}: {}", chat_id, e);
             let error_text = format!("❌ Ошибка при поиске операций в БД: {}", e);
             let keyboard = InlineKeyboardMarkup::new(vec![vec![
                 InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
             ]]);
             bot.edit_message_text(chat_id, bot_msg_id, error_text).reply_markup(keyboard).await?;
              // <<< ИСПРАВЛЕНО: .await >>>
             { state_storage.write().await.insert(chat_id, UserState::None); }
             return Err(e.into());
        }
    }
    Ok(())
}


/// Ищет операции для КОНКРЕТНОГО символа и либо запускает одну, либо предлагает выбор
async fn find_and_process_symbol_operations(
    bot: Bot,
    chat_id: ChatId,
    symbol: String,
    state_storage: StateStorage, // Тип StateStorage уже Arc<TokioRwLock<...>>
    db: Arc<Db>,
    message_id_to_edit: Option<MessageId>,
) -> anyhow::Result<()>
{
    info!("Looking for completed hedges for specific symbol {} for chat_id {}", symbol, chat_id);
    let loading_text = format!("⏳ Поиск завершенных операций для {}...", symbol);
    let mut bot_msg_id_opt = message_id_to_edit;

    if let Some(msg_id) = bot_msg_id_opt {
         let _ = bot.edit_message_text(chat_id, msg_id, loading_text)
               .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
               .await;
    } else {
         bot_msg_id_opt = Some(bot.send_message(chat_id, loading_text).await?.id);
    }
    let bot_msg_id = bot_msg_id_opt.ok_or_else(|| anyhow::anyhow!("Failed to get bot message ID for unhedge status"))?;

    match get_completed_unhedged_ops_for_symbol(db.as_ref(), chat_id.0, &symbol).await {
        Ok(operations) => {
            if operations.is_empty() {
                let text = format!("ℹ️ Не найдено завершенных операций хеджирования для {}, которые можно было бы расхеджировать.", symbol);
                 let keyboard = InlineKeyboardMarkup::new(vec![vec![
                     InlineKeyboardButton::callback("⬅️ Назад (в гл. меню)", callback_data::BACK_TO_MAIN)
                 ]]);
                bot.edit_message_text(chat_id, bot_msg_id, text).reply_markup(keyboard).await?;
                 // <<< ИСПРАВЛЕНО: .await >>>
                { state_storage.write().await.insert(chat_id, UserState::None); }
            } else if operations.len() == 1 {
                 let op_to_unhedge = operations.into_iter().next().unwrap();
                 prompt_unhedge_confirmation(&bot, chat_id, op_to_unhedge, state_storage, Some(bot_msg_id)).await?;
            } else {
                 prompt_operation_selection(&bot, chat_id, &symbol, operations, state_storage, Some(bot_msg_id)).await?;
            }
        }
        Err(e) => {
            error!("Failed to query hedge operations for {}: {}", symbol, e);
            let error_text = format!("❌ Ошибка при поиске операций хеджирования в БД: {}", e);
            let keyboard = InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
            ]]);
            bot.edit_message_text(chat_id, bot_msg_id, error_text).reply_markup(keyboard).await?;
             // <<< ИСПРАВЛЕНО: .await >>>
            { state_storage.write().await.insert(chat_id, UserState::None); }
            return Err(e.into());
        }
    }
    Ok(())
}

// --- Обработчики команд и колбэков ---

/// Обработчик команды /unhedge [SYMBOL]
pub async fn handle_unhedge_command<E>(
    bot: Bot,
    msg: Message,
    symbol_arg: String,
    _exchange: Arc<E>,
    state_storage: StateStorage, // Тип StateStorage уже Arc<TokioRwLock<...>>
    _running_operations: RunningOperations,
    _cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let symbol = symbol_arg.trim().to_uppercase();

    let mut previous_bot_message_id: Option<i32> = None;
      {
         // <<< ИСПРАВЛЕНО: .await >>>
         let mut state_guard = state_storage.write().await;
         if let Some(old_state) = state_guard.get(&chat_id) {
              previous_bot_message_id = match old_state {
                   UserState::AwaitingUnhedgeAssetSelection { last_bot_message_id, .. } => *last_bot_message_id,
                   UserState::AwaitingUnhedgeOperationSelection { last_bot_message_id, .. } => *last_bot_message_id,
                   UserState::AwaitingUnhedgeConfirmation { last_bot_message_id, .. } => *last_bot_message_id,
                   _ => None,
              };
         }
         if !matches!(state_guard.get(&chat_id), Some(UserState::None) | None) {
             info!("Resetting state for {} due to /unhedge command", chat_id);
             state_guard.insert(chat_id, UserState::None);
         }
    } // Блокировка записи освобождается здесь
    if let Some(bot_msg_id) = previous_bot_message_id {
        if let Err(e) = bot.delete_message(chat_id, MessageId(bot_msg_id)).await { warn!("Failed delete prev bot msg: {}", e); }
    }
    let user_msg_id = msg.id;
    if let Err(e) = bot.delete_message(chat_id, user_msg_id).await { warn!("Failed delete user command msg: {}", e); }


    if symbol.is_empty() {
        info!("Processing /unhedge command without symbol for chat_id: {}", chat_id);
        start_unhedge_asset_or_op_selection(bot, chat_id, state_storage, db, None).await?;
    } else {
        info!("Processing /unhedge command for chat_id: {}, symbol: {}", chat_id, symbol);
        find_and_process_symbol_operations(bot, chat_id, symbol, state_storage, db, None).await?;
    }

    Ok(())
}

/// Обработчик колбэка кнопки "Расхеджировать" из главного меню (без изменений)
pub async fn handle_start_unhedge_callback<E>(
    bot: Bot,
    query: CallbackQuery,
    _exchange: Arc<E>,
    state_storage: StateStorage,
    _cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
      if let Some(msg) = query.message.as_ref() {
          let chat_id = msg.chat().id;
          info!("Processing '{}' callback for chat_id: {}", callback_data::START_UNHEDGE, chat_id);
          bot.answer_callback_query(query.id).await?;
          start_unhedge_asset_or_op_selection(bot, chat_id, state_storage, db, Some(msg.id())).await?;
      } else {
          warn!("CallbackQuery missing message in handle_start_unhedge_callback");
          bot.answer_callback_query(query.id).await?;
      }
    Ok(())
}

/// Обработчик колбэка выбора актива для расхеджа (префикс u_asset_)
pub async fn handle_unhedge_asset_callback<E>(
    bot: Bot,
    query: CallbackQuery,
    _exchange: Arc<E>,
    state_storage: StateStorage, // Тип StateStorage уже Arc<TokioRwLock<...>>
    _cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let query_id = query.id;
    if let (Some(data), Some(msg)) = (query.data.as_deref(), query.message.as_ref()) {
        let chat_id = msg.chat().id;
        if let Some(symbol) = data.strip_prefix(callback_data::PREFIX_UNHEDGE_ASSET) {
             info!("User {} selected asset {} for unhedge via callback", chat_id, symbol);

            let is_correct_state = {
                 // <<< ИСПРАВЛЕНО: .await >>>
                 let state_read_guard = state_storage.read().await;
                 matches!(state_read_guard.get(&chat_id), Some(UserState::AwaitingUnhedgeAssetSelection { .. }))
            }; // Блокировка чтения освобождается здесь

            if is_correct_state {
                 bot.answer_callback_query(query_id).await?;
                 find_and_process_symbol_operations(bot, chat_id, symbol.to_string(), state_storage, db, Some(msg.id())).await?;
                 return Ok(());
            } else {
                 warn!("User {} clicked unhedge asset button but was in wrong state", chat_id);
                  // <<< ИСПРАВЛЕНО: .await >>>
                  { state_storage.write().await.insert(chat_id, UserState::None); }
                  let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await;
                  bot.answer_callback_query(query_id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                  return Ok(());
            }

        } else {
             warn!("Invalid callback data format for unhedge asset selection: {}", data);
        }
    } else {
         warn!("CallbackQuery missing data or message in handle_unhedge_asset_callback");
    }
      bot.answer_callback_query(query_id).await?;
      Ok(())
}


/// Обработчик колбэка выбора операции для расхеджа (префикс u_opsel_)
pub async fn handle_unhedge_select_op_callback<E>(
    bot: Bot,
    query: CallbackQuery,
    _exchange: Arc<E>,
    state_storage: StateStorage, // Тип StateStorage уже Arc<TokioRwLock<...>>
    _running_operations: RunningOperations,
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let query_id = query.id;
    if let (Some(data), Some(msg)) = (query.data.as_deref(), query.message.as_ref()) {
        let chat_id = msg.chat().id;
        if let Some(op_id_str) = data.strip_prefix(callback_data::PREFIX_UNHEDGE_OP_SELECT) {
            if let Ok(operation_id) = op_id_str.parse::<i64>() {
                 info!("User {} selected operation ID {} to unhedge", chat_id, operation_id);

                let op_to_confirm_opt: Option<HedgeOperation> = {
                     // <<< ИСПРАВЛЕНО: .await >>>
                    let state_guard = state_storage.read().await;
                    if let Some(UserState::AwaitingUnhedgeOperationSelection { operations, .. }) = state_guard.get(&chat_id) {
                        operations.iter().find(|op| op.id == operation_id).cloned()
                    } else {
                        warn!("User {} clicked unhedge operation selection but was in wrong state", chat_id);
                        drop(state_guard);
                         // <<< ИСПРАВЛЕНО: .await >>>
                         { state_storage.write().await.insert(chat_id, UserState::None); }
                        let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await;
                        bot.answer_callback_query(query_id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                        return Ok(());
                    }
                }; // Блокировка чтения освобождается здесь

                if let Some(op) = op_to_confirm_opt {
                    prompt_unhedge_confirmation(&bot, chat_id, op, state_storage, Some(msg.id())).await?;
                    bot.answer_callback_query(query_id).await?;
                    return Ok(());
                } else {
                    error!("Operation ID {} not found in state for chat_id {}", operation_id, chat_id);
                     // <<< ИСПРАВЛЕНО: .await >>>
                    { state_storage.write().await.insert(chat_id, UserState::None); }
                    let _ = bot.edit_message_text(chat_id, msg.id(), "❌ Ошибка: Выбранная операция не найдена. Попробуйте снова.")
                             .reply_markup(navigation::make_main_menu_keyboard())
                             .await;
                    bot.answer_callback_query(query_id).await?;
                    return Ok(());
                }

            } else {
                 error!("Failed to parse operation_id from callback data: {}", data);
                 let _ = bot.edit_message_text(chat_id, msg.id(), "❌ Ошибка: Неверный ID операции в кнопке.")
                          .reply_markup(navigation::make_main_menu_keyboard())
                          .await;
                    // <<< ИСПРАВЛЕНО: .await >>>
                   { state_storage.write().await.insert(chat_id, UserState::None); }
                 bot.answer_callback_query(query_id).await?;
                 return Ok(());
            }
        } else {
             warn!("Invalid callback data format for unhedge operation selection: {}", data);
        }
    } else {
         warn!("CallbackQuery missing data or message in handle_unhedge_select_op_callback");
    }
    bot.answer_callback_query(query_id).await?;
    Ok(())
}

/// Запрашивает подтверждение перед расхеджированием
async fn prompt_unhedge_confirmation(
    bot: &Bot,
    chat_id: ChatId,
    operation_to_unhedge: HedgeOperation,
    state_storage: StateStorage, // Тип StateStorage уже Arc<TokioRwLock<...>>
    message_id_to_edit: Option<MessageId>,
) -> anyhow::Result<()> {
    let operation_id = operation_to_unhedge.id;
    let symbol = operation_to_unhedge.base_symbol.clone();
    let fut_qty = operation_to_unhedge.target_futures_qty;
    let spot_sell_qty_approx = operation_to_unhedge.spot_filled_qty;

    let text = format!(
        "Подтвердите расхеджирование операции ID:{}\n\
         Символ: {}\n\
         Будет продано ~{:.8} {} спота.\n\
         Будет куплено {:.8} {} фьючерса.\n\n\
         Вы уверены?",
        operation_id, symbol, spot_sell_qty_approx, symbol, fut_qty, symbol
    );
    let keyboard = make_unhedge_confirmation_keyboard(operation_id);

    let bot_msg_id = if let Some(msg_id) = message_id_to_edit {
        bot.edit_message_text(chat_id, msg_id, text).reply_markup(keyboard).await?;
        msg_id
    } else {
        bot.send_message(chat_id, text).reply_markup(keyboard).await?.id
    };

    {
         // <<< ИСПРАВЛЕНО: .await >>>
        let mut state_guard = state_storage.write().await;
        state_guard.insert(chat_id, UserState::AwaitingUnhedgeConfirmation {
            operation_id,
            last_bot_message_id: Some(bot_msg_id.0),
        });
        info!("User state for {} set to AwaitingUnhedgeConfirmation for op_id {}", chat_id, operation_id);
    } // Блокировка записи освобождается здесь
    Ok(())
}


/// Обработчик колбэка подтверждения расхеджа (префикс u_conf_)
pub async fn handle_unhedge_confirm_callback<E>(
    bot: Bot,
    query: CallbackQuery,
    exchange: Arc<E>,
    state_storage: StateStorage, // Тип StateStorage уже Arc<TokioRwLock<...>>
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let query_id = query.id.clone();

    if let (Some(data), Some(msg)) = (query.data.as_deref(), query.message.as_ref()) {
        let chat_id = msg.chat().id;
        if let Some(payload) = data.strip_prefix(callback_data::PREFIX_UNHEDGE_CONFIRM) {
            if payload == "yes" {
                info!("User {} confirmed unhedge operation", chat_id);

                let operation_id_to_unhedge = {
                     // <<< ИСПРАВЛЕНО: .await >>>
                     let state_guard = state_storage.read().await;
                      match state_guard.get(&chat_id) {
                         Some(UserState::AwaitingUnhedgeConfirmation { operation_id, .. }) => *operation_id,
                         _ => {
                             warn!("User {} confirmed unhedge but was in wrong state", chat_id);
                             drop(state_guard);
                              // <<< ИСПРАВЛЕНО: .await >>>
                              { state_storage.write().await.insert(chat_id, UserState::None); }
                             let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await;
                              bot.answer_callback_query(query_id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                             return Ok(());
                         }
                     }
                }; // Блокировка чтения освобождается здесь
                 // <<< ИСПРАВЛЕНО: .await >>>
                 { state_storage.write().await.insert(chat_id, UserState::None); }

                match get_hedge_operation_by_id(db.as_ref(), operation_id_to_unhedge).await {
                     Ok(Some(original_op)) => {
                         if original_op.status != "Completed" || original_op.unhedged_op_id.is_some() {
                             error!("Attempted to unhedge already unhedged or invalid op_id: {}", operation_id_to_unhedge);
                             let _ = bot.edit_message_text(chat_id, msg.id(), "❌ Операция уже расхеджирована или недействительна.")
                                      .reply_markup(navigation::make_main_menu_keyboard())
                                      .await;
                             bot.answer_callback_query(query_id).await?;
                             return Ok(());
                         } else {
                             let _ = bot.edit_message_text(chat_id, msg.id(), format!("⏳ Запуск расхеджирования операции ID:{}...", operation_id_to_unhedge))
                                .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                                .await?;
                             spawn_unhedge_task(
                                 bot.clone(), exchange.clone(), cfg.clone(), db.clone(),
                                 running_operations.clone(), chat_id, original_op, msg.id(),
                             ).await;
                         }
                     }
                     Ok(None) => {
                         error!("Hedge operation ID {} not found in DB for unhedge confirmation", operation_id_to_unhedge);
                         let _ = bot.edit_message_text(chat_id, msg.id(), "❌ Ошибка: Операция не найдена в БД.")
                                  .reply_markup(navigation::make_main_menu_keyboard())
                                  .await;
                         bot.answer_callback_query(query_id).await?;
                         return Ok(());
                     }
                     Err(e) => {
                          error!("DB error getting hedge operation {} for unhedge: {}", operation_id_to_unhedge, e);
                          let _ = bot.edit_message_text(chat_id, msg.id(), "❌ Ошибка БД при получении деталей операции.")
                                   .reply_markup(navigation::make_main_menu_keyboard())
                                   .await;
                         bot.answer_callback_query(query_id).await?;
                         return Ok(());
                     }
                }

            } else if payload == "no" {
                info!("User {} cancelled unhedge at confirmation", chat_id);
                bot.answer_callback_query(query_id).await?;
                navigation::handle_cancel_dialog(bot, chat_id, msg.id(), state_storage).await?;
                return Ok(());

            } else {
                 warn!("Invalid payload for unhedge confirmation callback: {}", payload);
                 bot.answer_callback_query(query_id).await?;
                 return Ok(());
            }
        } else if data == callback_data::CANCEL_DIALOG {
            info!("User cancelled unhedge dialog via cancel button");
            bot.answer_callback_query(query_id).await?;
            navigation::handle_cancel_dialog(bot, chat_id, msg.id(), state_storage).await?;
            return Ok(());
        }
         else {
             warn!("Invalid callback data format for unhedge confirmation: {}", data);
             bot.answer_callback_query(query_id).await?;
             return Ok(());
        }
    } else {
         warn!("CallbackQuery missing data or message in handle_unhedge_confirm_callback");
         bot.answer_callback_query(query_id).await?;
         return Ok(());
    }
    Ok(())
}