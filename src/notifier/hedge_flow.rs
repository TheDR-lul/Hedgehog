// src/notifier/hedge_flow.rs

use super::{
    StateStorage, UserState, RunningOperations, RunningOperationInfo, OperationType, callback_data,
    navigation,
    wallet_info,
    // progress,
    // utils,
};
use crate::config::Config;
use crate::exchange::Exchange;
use crate::storage::{Db, insert_hedge_operation};
use crate::hedger::{Hedger, HedgeParams, HedgeProgressUpdate, HedgeProgressCallback, ORDER_FILL_TOLERANCE};
use crate::models::HedgeRequest;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId,
    MaybeInaccessibleMessage
};
use tracing::{info, warn, error};
use futures::future::FutureExt;

// --- Вспомогательные функции для этого модуля ---

fn make_hedge_confirmation_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("✅ Да, запустить", format!("{}{}", callback_data::PREFIX_HEDGE_CONFIRM, "yes")),
            InlineKeyboardButton::callback("❌ Нет, отмена", callback_data::CANCEL_DIALOG), // Имя кнопки "Отмена" должно совпадать с callback_data
        ],
    ])
}

fn make_dialog_keyboard() -> InlineKeyboardMarkup {
     InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("❌ Отмена", callback_data::CANCEL_DIALOG), // Имя кнопки "Отмена" должно совпадать с callback_data
    ]])
}

/// Запускает фоновую задачу хеджирования
async fn spawn_hedge_task<E>(
    bot: Bot,
    exchange: Arc<E>, // Передаем Arc для клонирования внутри задачи
    cfg: Arc<Config>,   // Передаем Arc для клонирования внутри задачи
    db: Arc<Db>,     // Передаем Arc для клонирования внутри задачи
    running_operations: RunningOperations,
    chat_id: ChatId,
    params: HedgeParams, // params перемещается в задачу
    initial_sum: f64,
    volatility_percent: f64,
    waiting_message: MaybeInaccessibleMessage, // waiting_message перемещается в задачу
)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let bot_message_id = waiting_message.id();
    let _message_chat_id = waiting_message.chat().id;

    // <<<--- ИСПРАВЛЕНО: Клонируем символ трижды ---
    let symbol_for_callback = params.symbol.clone();  // Для progress_callback
    let symbol_for_task_body = params.symbol.clone(); // Для остального тела задачи
    let symbol_for_info = params.symbol.clone();      // Для RunningOperationInfo
    // ---

    let hedger = Hedger::new((*exchange).clone(), (*cfg).clone()); // Клонируем для hedger

    let operation_id_result = insert_hedge_operation(
        db.as_ref(), // Заимствуем из Arc
        chat_id.0,
        &params.symbol, // Заимствуем из params перед перемещением
        &cfg.quote_currency, // Заимствуем из Arc
        initial_sum,
        volatility_percent / 100.0,
        params.spot_order_qty, // Заимствуем из params перед перемещением
        params.fut_order_qty, // Заимствуем из params перед перемещением
    ).await;

    let operation_id = match operation_id_result {
        Ok(id) => {
            info!("op_id:{}: Created DB record for hedge operation.", id);
            id
        }
        Err(e) => {
            error!("Failed to insert hedge operation into DB: {}", e);
            let error_text = format!("❌ Критическая ошибка БД при создании записи операции: {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, error_text)
                         .reply_markup(navigation::make_main_menu_keyboard())
                         .await;
            return;
        }
    };

    let current_spot_order_id_storage = Arc::new(TokioMutex::new(None::<String>));
    let total_filled_qty_storage = Arc::new(TokioMutex::new(0.0f64));

    let bot_clone = bot.clone();
    let cfg_clone = cfg.clone(); // Клонируем Arc для колбэка
    let db_clone = db.clone();   // Клонируем Arc для задачи
    let current_spot_order_id_storage_clone = current_spot_order_id_storage.clone();
    let total_filled_qty_storage_clone = total_filled_qty_storage.clone();
    let running_operations_clone = running_operations.clone();

    // --- Создание колбэка прогресса ---
    let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
        // Захватываем клоны: bot_clone, cfg_clone, symbol_for_callback
        let bot_for_callback = bot_clone.clone();
        let qc = cfg_clone.quote_currency.clone(); // Клонируем строку из Arc<Config>
        let symbol_cb = symbol_for_callback.clone(); // Клонируем строку для async move
        let msg_id_cb = bot_message_id; // MessageId is Copy
        let chat_id_cb = chat_id;       // ChatId is Copy
        let initial_sum_cb = initial_sum; // f64 is Copy
        let operation_id_cb = operation_id; // i64 is Copy

        async move {
            // Используем захваченные переменные (_cb)
            let symbol = symbol_cb;
            let filled_percent = if update.target_qty > ORDER_FILL_TOLERANCE {
                (update.filled_qty / update.target_qty) * 100.0 } else { 0.0 };
            let progress_bar_len = 10;
            let filled_blocks = (filled_percent / (100.0 / progress_bar_len as f64)).round() as usize;
            let empty_blocks = progress_bar_len - filled_blocks;
            let progress_bar = format!("[{}{}]", "█".repeat(filled_blocks), "░".repeat(empty_blocks));
            let status_text = if update.is_replacement { "(Ордер переставлен)" } else { "" };

            let text = format!(
                "⏳ Хеджирование ID:{} {} {:.2} {} ({}) в процессе...\nРын.цена: {:.2}\nОрдер на покупку: {:.2} {}\nИсполнено: {:.6}/{:.6} ({:.1}%)",
                operation_id_cb, progress_bar, initial_sum_cb, qc, symbol,
                update.current_spot_price, update.new_limit_price, status_text,
                update.filled_qty, update.target_qty, filled_percent
            );

            let cancel_callback_data = format!("{}{}", callback_data::PREFIX_CANCEL_ACTIVE_OP, operation_id_cb);
            let cancel_button = InlineKeyboardButton::callback("❌ Отменить эту операцию", cancel_callback_data);
            let kb = InlineKeyboardMarkup::new(vec![vec![cancel_button]]);

            if let Err(e) = bot_for_callback.edit_message_text(chat_id_cb, msg_id_cb, text)
                .reply_markup(kb)
                .await {
                if !e.to_string().contains("not modified") {
                    warn!("op_id:{}: Progress callback failed: {}", operation_id_cb, e);
                }
            }
            Ok(())
        }.boxed()
    });
    // --- Конец колбэка прогресса ---

    // Клонируем Arc<> для задачи
    let exchange_task = exchange.clone();
    let cfg_task = cfg.clone(); // Этот клон будет использован внутри задачи

    let task = tokio::spawn(async move {
        // Перемещаются: params, progress_callback, current_spot...clone, total_filled...clone, operation_id, db_clone
        // Также используются (захватываются): exchange_task, cfg_task, symbol_for_task_body, chat_id, bot_message_id, running_operations_clone, volatility_percent
        // bot (оригинал) используется для edit_message_text в конце
        let result = hedger.run_hedge(
            params, // Перемещается сюда
            progress_callback, // Перемещается сюда
            current_spot_order_id_storage_clone, // Перемещается сюда
            total_filled_qty_storage_clone, // Перемещается сюда
            operation_id, // Копируется (i64)
            db_clone.as_ref(), // Заимствуется из Arc
        ).await;

        let is_cancelled_by_button = result.is_err() && result.as_ref().err().map_or(false, |e| e.to_string().contains("cancelled by user"));
        if !is_cancelled_by_button {
            running_operations_clone.lock().await.remove(&(chat_id, operation_id));
            info!("op_id:{}: Removed running operation info for chat_id: {}", operation_id, chat_id);
        } else {
            info!("op_id:{}: Operation was cancelled via button, info already removed.", operation_id);
        }

        match result {
            Ok((spot_qty_gross, fut_qty_net, final_spot_value_gross)) => {
                info!( "op_id:{}: Hedge OK. Spot Gross: {}, Fut Net: {}, Value: {:.2}", operation_id, spot_qty_gross, fut_qty_net, final_spot_value_gross );
                tokio::time::sleep(Duration::from_millis(500)).await;
                // <<<--- ИСПРАВЛЕНО: Используем exchange_task и symbol_for_task_body ---
                let final_net_spot_balance = match exchange_task.get_balance(&symbol_for_task_body).await {
                    Ok(b) => { info!("op_id:{}: Fetched final spot balance: {}", operation_id, b.free); b.free },
                    Err(e) => {
                        warn!("op_id:{}: Failed get final spot balance after hedge: {}. Using calculated gross.", operation_id, e);
                        spot_qty_gross
                    }
                };
                let success_text = format!(
                     "✅ Хеджирование ID:{} ~{:.2} {} ({}) при V={:.1}% завершено:\n\n🟢 Спот куплено (брутто): {:.8}\nspot_balance_check {:.8}\n🔴 Фьюч продано (нетто): {:.8}",
                    operation_id, final_spot_value_gross, cfg_task.quote_currency, symbol_for_task_body,
                    volatility_percent, spot_qty_gross, final_net_spot_balance, fut_qty_net,
                );
                // ---
                // Используем оригинальный `bot`, который не был перемещен в задачу
                let _ = bot.edit_message_text(chat_id, bot_message_id, success_text)
                         .reply_markup(navigation::make_main_menu_keyboard())
                         .await;
            }
            Err(e) => {
                if is_cancelled_by_button {
                    info!("op_id:{}: Hedge task finished after cancellation via button.", operation_id);
                } else {
                    error!("op_id:{}: Hedge execution failed: {}", operation_id, e);
                    let error_text = format!("❌ Ошибка хеджирования ID:{}: {}", operation_id, e);
                     // Используем оригинальный `bot`
                     let _ = bot.edit_message_text(chat_id, bot_message_id, error_text)
                                .reply_markup(navigation::make_main_menu_keyboard())
                                .await;
                }
            }
        }
    });

    // Сохраняем информацию о запущенной задаче
    let info = RunningOperationInfo {
        handle: task.abort_handle(),
        operation_id,
        operation_type: OperationType::Hedge,
        // <<<--- ИСПРАВЛЕНО: Используем symbol_for_info ---
        symbol: symbol_for_info,
        // ---
        bot_message_id: bot_message_id.0,
        current_spot_order_id: current_spot_order_id_storage, // Перемещаем Arc
        total_filled_spot_qty: total_filled_qty_storage, // Перемещаем Arc
    };
    running_operations.lock().await.insert((chat_id, operation_id), info);
    info!("op_id:{}: Stored running hedge info.", operation_id);
}


// --- Обработчики команд и колбэков ---

/// Обработчик команды /hedge [SYMBOL]
pub async fn handle_hedge_command<E>(
    bot: Bot,
    msg: Message,
    symbol_arg: String,
    exchange: Arc<E>,
    state_storage: StateStorage,
    _running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let symbol = symbol_arg.trim().to_uppercase();

    let mut previous_bot_message_id: Option<i32> = None;
    {
        let mut state_guard = state_storage.write().expect("Lock failed");
        if let Some(old_state) = state_guard.get(&chat_id) {
            previous_bot_message_id = match old_state {
                UserState::AwaitingHedgeAssetSelection { last_bot_message_id, .. } => *last_bot_message_id,
                UserState::AwaitingHedgeSum { last_bot_message_id, .. } => *last_bot_message_id,
                UserState::AwaitingHedgeVolatility { last_bot_message_id, .. } => *last_bot_message_id,
                UserState::AwaitingHedgeConfirmation { last_bot_message_id, .. } => *last_bot_message_id,
                _ => None,
            };
        }
        if !matches!(state_guard.get(&chat_id), Some(UserState::None) | None) {
            info!("Resetting state for {} due to /hedge command", chat_id);
            state_guard.insert(chat_id, UserState::None);
        }
    }
    if let Some(bot_msg_id) = previous_bot_message_id {
        if let Err(e) = bot.delete_message(chat_id, MessageId(bot_msg_id)).await { warn!("Failed delete prev bot msg: {}", e); }
    }
    let user_msg_id = msg.id;
    if let Err(e) = bot.delete_message(chat_id, user_msg_id).await { warn!("Failed delete user command msg: {}", e); }


    if symbol.is_empty() {
        info!("Processing /hedge command without symbol for chat_id: {}", chat_id);
        prompt_asset_selection(bot, chat_id, state_storage, exchange, cfg, db, None).await?;
    } else {
        info!("Processing /hedge command for chat_id: {}, symbol: {}", chat_id, symbol);
        // TODO: Валидация символа
        let text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
        let kb = make_dialog_keyboard();
        let bot_msg = bot.send_message(chat_id, text).reply_markup(kb).await?;
        {
            let mut state_guard = state_storage.write().expect("Lock failed");
            state_guard.insert(chat_id, UserState::AwaitingHedgeSum {
                symbol: symbol.clone(),
                last_bot_message_id: Some(bot_msg.id.0),
            });
            info!("User state for {} set to AwaitingHedgeSum for symbol {}", chat_id, symbol);
        }
    }
    Ok(())
}

/// Обработчик колбэка кнопки "Захеджировать" из главного меню
pub async fn handle_start_hedge_callback<E>(
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
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::START_HEDGE, chat_id);
        bot.answer_callback_query(q.id).await?;
        prompt_asset_selection(bot, chat_id, state_storage, exchange, cfg, db, Some(msg.id())).await?;
    } else {
        warn!("CallbackQuery missing message in handle_start_hedge_callback");
        bot.answer_callback_query(q.id).await?;
    }
    Ok(())
}


/// Запрашивает у пользователя выбор актива для хеджирования
async fn prompt_asset_selection<E>(
    bot: Bot,
    chat_id: ChatId,
    state_storage: StateStorage,
    exchange: Arc<E>,
    cfg: Arc<Config>,
    _db: Arc<Db>,
    message_id_to_edit: Option<MessageId>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    info!("Prompting asset selection for hedge, chat_id: {}", chat_id);
    let loading_text = "⏳ Загрузка доступных активов...";
    let mut bot_message_id = message_id_to_edit;

    if let Some(msg_id) = bot_message_id {
        let kb = InlineKeyboardMarkup::new(vec![vec![
             InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
        ]]);
        let _ = bot.edit_message_text(chat_id, msg_id, loading_text).reply_markup(kb).await;
    } else {
        let sent_msg = bot.send_message(chat_id, loading_text).await?;
        bot_message_id = Some(sent_msg.id);
    }

    match wallet_info::get_formatted_balances(exchange.as_ref(), &cfg.quote_currency, false).await {
        Ok((_, asset_data)) => {
            let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
            let mut assets_found = false;

            for (coin, free, locked) in asset_data {
                if coin != cfg.quote_currency {
                     assets_found = true;
                     let callback_data_asset = format!("{}{}", callback_data::PREFIX_HEDGE_ASSET, coin);
                     buttons.push(vec![InlineKeyboardButton::callback(
                         format!("💼 {} (free: {:.6}, locked: {:.6})", coin, free, locked),
                         callback_data_asset,
                     )]);
                }
            }

            let mut text = "Выберите актив из кошелька для хеджирования:".to_string();
            if !assets_found {
                text = format!("ℹ️ В вашем кошельке нет активов (кроме {}), подходящих для хеджирования.\n", cfg.quote_currency);
            }
            text.push_str("\nИли отправьте тикер актива (например, BTC) сообщением.");
            buttons.push(vec![InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)]);
            let keyboard = InlineKeyboardMarkup::new(buttons);

            if let Some(msg_id) = bot_message_id {
                if let Err(e) = bot.edit_message_text(chat_id, msg_id, &text).reply_markup(keyboard.clone()).await {
                   error!("Failed to edit message for asset selection: {}. Sending new.", e);
                   bot_message_id = Some(bot.send_message(chat_id, text).reply_markup(keyboard).await?.id);
                }
            } else {
                 bot_message_id = Some(bot.send_message(chat_id, text).reply_markup(keyboard).await?.id);
            }

            {
                let mut state_guard = state_storage.write().expect("Lock failed");
                state_guard.insert(chat_id, UserState::AwaitingHedgeAssetSelection {
                    last_bot_message_id: bot_message_id.map(|id| id.0),
                });
                info!("User state for {} set to AwaitingHedgeAssetSelection", chat_id);
            }
        }
        Err(e) => {
             error!("Failed to get balances for asset selection: {}", e);
             let error_text = format!("❌ Не удалось получить список активов из кошелька: {}", e);
             let kb = InlineKeyboardMarkup::new(vec![vec![
                 InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
             ]]);
             if let Some(msg_id) = bot_message_id {
                 let _ = bot.edit_message_text(chat_id, msg_id, error_text).reply_markup(kb).await;
             } else {
                 let _ = bot.send_message(chat_id, error_text).reply_markup(kb).await;
             }
              { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
             return Err(e.into());
        }
    }
    Ok(())
}

/// Обработчик колбэка выбора актива для хеджа (кнопка с префиксом h_asset_)
pub async fn handle_hedge_asset_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    _exchange: Arc<E>,
    state_storage: StateStorage,
    cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let (Some(data), Some(msg)) = (q.data, q.message) {
        let chat_id = msg.chat().id;
        if let Some(symbol) = data.strip_prefix(callback_data::PREFIX_HEDGE_ASSET) {
             info!("User {} selected asset {} for hedge via callback", chat_id, symbol);

            let is_correct_state = matches!(
                state_storage.read().expect("Lock failed").get(&chat_id),
                Some(UserState::AwaitingHedgeAssetSelection { .. })
            );

            if is_correct_state {
                let text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                let kb = make_dialog_keyboard();
                bot.edit_message_text(chat_id, msg.id(), text).reply_markup(kb).await?;

                {
                    let mut state_guard = state_storage.write().expect("Lock failed");
                    if let Some(current_state @ UserState::AwaitingHedgeAssetSelection { .. }) = state_guard.get_mut(&chat_id) {
                         *current_state = UserState::AwaitingHedgeSum {
                             symbol: symbol.to_string(),
                             last_bot_message_id: Some(msg.id().0),
                         };
                         info!("User state for {} set to AwaitingHedgeSum for {}", chat_id, symbol);
                    } else {
                         warn!("State changed unexpectedly for {} before setting AwaitingHedgeSum", chat_id);
                         let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await;
                    }
                }
            } else {
                 warn!("User {} clicked hedge asset button but was in wrong state", chat_id);
                 { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
                 let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await;
                 bot.answer_callback_query(q.id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                 return Ok(());
            }
        } else {
             warn!("Invalid callback data format for hedge asset selection: {}", data);
        }
    } else {
         warn!("CallbackQuery missing data or message in handle_hedge_asset_callback");
    }
     bot.answer_callback_query(q.id).await?;
     Ok(())
}

/// Обработчик ручного ввода тикера в состоянии AwaitingHedgeAssetSelection
pub async fn handle_asset_ticker_input<E>(
    bot: Bot,
    msg: Message,
    _exchange: Arc<E>,
    state_storage: StateStorage,
    cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
     E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let message_id = msg.id;
    let text = msg.text().unwrap_or("").trim().to_uppercase();

    if text.is_empty() || text.starts_with('/') {
        if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete ignored message: {}", e); }
        return Ok(());
    }

    let previous_bot_message_id = {
         let state_guard = state_storage.read().expect("Lock failed");
         match state_guard.get(&chat_id) {
            Some(UserState::AwaitingHedgeAssetSelection { last_bot_message_id }) => *last_bot_message_id,
            _ => {
                if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete unexpected text message: {}", e); }
                return Ok(());
            }
         }
    };

    info!("User {} entered ticker '{}' for hedge", chat_id, text);

    if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete user ticker message: {}", e); }

    // TODO: Валидация тикера
    let is_valid_ticker = true; // Заглушка

    if is_valid_ticker {
        let prompt_text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, text);
        let kb = make_dialog_keyboard();

        if let Some(bot_msg_id_int) = previous_bot_message_id {
            let bot_msg_id = MessageId(bot_msg_id_int);
            match bot.edit_message_text(chat_id, bot_msg_id, prompt_text).reply_markup(kb).await {
               Ok(_) => {
                    {
                        let mut state_guard = state_storage.write().expect("Lock failed");
                         if let Some(current_state @ UserState::AwaitingHedgeAssetSelection { .. }) = state_guard.get_mut(&chat_id) {
                             *current_state = UserState::AwaitingHedgeSum {
                                 symbol: text.clone(),
                                 last_bot_message_id: Some(bot_msg_id.0),
                            };
                            info!("User state for {} set to AwaitingHedgeSum for {}", chat_id, text);
                        } else {
                             warn!("State changed for {} before setting AwaitingHedgeSum after ticker input", chat_id);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to edit message {} to prompt sum: {}", bot_msg_id, e);
                    let _ = navigation::show_main_menu(&bot, chat_id, None).await;
                    { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
                }
            }
        } else {
             warn!("No previous bot message id found for chat_id {} to edit for sum prompt", chat_id);
             let bot_msg = bot.send_message(chat_id, prompt_text).reply_markup(kb).await?;
              {
                 let mut state_guard = state_storage.write().expect("Lock failed");
                state_guard.insert(chat_id, UserState::AwaitingHedgeSum {
                     symbol: text.clone(),
                     last_bot_message_id: Some(bot_msg.id.0),
                 });
                 info!("User state for {} set to AwaitingHedgeSum for {}", chat_id, text);
               }
        }
    } else {
        let error_text = format!("❌ Символ '{}' не найден или не подходит для хеджирования. Попробуйте другой.", text);
        if let Some(bot_msg_id_int) = previous_bot_message_id {
             let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
        } else {
             let _ = bot.send_message(chat_id, error_text).await;
        }
    }
    Ok(())
}


/// Обработчик ввода суммы хеджирования
pub async fn handle_sum_input(
    bot: Bot,
    msg: Message,
    state_storage: StateStorage,
    cfg: Arc<Config>,
) -> anyhow::Result<()> {
    let chat_id = msg.chat.id;
    let message_id = msg.id;
    let text = msg.text().unwrap_or("").trim();

    let (symbol, previous_bot_message_id) = {
        let state_guard = state_storage.read().expect("Lock failed");
        match state_guard.get(&chat_id) {
            Some(UserState::AwaitingHedgeSum { symbol, last_bot_message_id }) => (symbol.clone(), *last_bot_message_id),
            _ => {
                if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete unexpected sum message: {}", e); }
                return Ok(());
            }
        }
    };

     if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete user sum message: {}", e); }

    match text.parse::<f64>() {
         Ok(sum) if sum > 0.0 => {
             info!("User {} entered sum {} for hedge {}", chat_id, sum, symbol);
             let prompt_text = format!("Введите ожидаемую волатильность для {} {} (%):", sum, cfg.quote_currency);
             let kb = make_dialog_keyboard();

             if let Some(bot_msg_id_int) = previous_bot_message_id {
                 let bot_msg_id = MessageId(bot_msg_id_int);
                 match bot.edit_message_text(chat_id, bot_msg_id, prompt_text).reply_markup(kb).await {
                    Ok(_) => {
                         {
                             let mut state_guard = state_storage.write().expect("Lock failed");
                              if let Some(current_state @ UserState::AwaitingHedgeSum { .. }) = state_guard.get_mut(&chat_id) {
                                  *current_state = UserState::AwaitingHedgeVolatility {
                                      symbol: symbol.clone(),
                                      sum,
                                      last_bot_message_id: Some(bot_msg_id.0),
                                 };
                                 info!("User state for {} set to AwaitingHedgeVolatility", chat_id);
                             } else {
                                 warn!("State changed for {} before setting AwaitingHedgeVolatility", chat_id);
                             }
                         }
                    }
                    Err(e) => {
                         error!("Failed to edit message {} to prompt volatility: {}", bot_msg_id, e);
                         let _ = navigation::show_main_menu(&bot, chat_id, None).await;
                         { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
                    }
                 }
             } else {
                  warn!("No previous bot message id found for chat_id {} to edit for volatility prompt", chat_id);
                  let bot_msg = bot.send_message(chat_id, prompt_text).reply_markup(kb).await?;
                   {
                      let mut state_guard = state_storage.write().expect("Lock failed");
                      state_guard.insert(chat_id, UserState::AwaitingHedgeVolatility {
                          symbol: symbol.clone(),
                          sum,
                          last_bot_message_id: Some(bot_msg.id.0),
                     });
                       info!("User state for {} set to AwaitingHedgeVolatility", chat_id);
                   }
             }
         }
         Ok(_) => {
             warn!("User {} entered non-positive sum: {}", chat_id, text);
              if let Some(bot_msg_id_int) = previous_bot_message_id {
                   let error_text = format!("⚠️ Сумма должна быть положительной. Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                   let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
              }
         }
         Err(_) => {
             warn!("User {} entered invalid sum format: {}", chat_id, text);
             if let Some(bot_msg_id_int) = previous_bot_message_id {
                 let error_text = format!("⚠️ Неверный формат суммы. Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                 let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
             }
         }
    }
     Ok(())
}

/// Обработчик ввода волатильности хеджирования
pub async fn handle_volatility_input<E>(
    bot: Bot,
    msg: Message,
    exchange: Arc<E>,
    state_storage: StateStorage,
    _running_operations: RunningOperations,
    cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let message_id = msg.id;
    let text = msg.text().unwrap_or("").trim();

    let (symbol, sum, previous_bot_message_id) = {
        let state_guard = state_storage.read().expect("Lock failed");
        match state_guard.get(&chat_id) {
            Some(UserState::AwaitingHedgeVolatility { symbol, sum, last_bot_message_id }) => (symbol.clone(), *sum, *last_bot_message_id),
            _ => {
                if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete unexpected volatility message: {}", e); }
                return Ok(());
            }
        }
    };

     if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete user volatility message: {}", e); }

    match text.trim_end_matches('%').trim().parse::<f64>() {
        Ok(volatility_percent) if volatility_percent >= 0.0 => {
            info!("User {} entered volatility {}% for hedge {} {}", chat_id, volatility_percent, sum, symbol);
            let volatility_fraction = volatility_percent / 100.0;

            let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction };
            let hedger = Hedger::new((*exchange).clone(), (*cfg).clone());

            let calc_indicator_text = "⏳ Расчет параметров хеджирования...";
            let mut bot_msg_id_opt = previous_bot_message_id.map(MessageId);

            if let Some(bot_msg_id) = bot_msg_id_opt {
                 let _ = bot.edit_message_text(chat_id, bot_msg_id, calc_indicator_text).await;
            } else {
                 bot_msg_id_opt = Some(bot.send_message(chat_id, calc_indicator_text).await?.id);
            }
            let bot_msg_id = bot_msg_id_opt.ok_or_else(|| anyhow::anyhow!("Failed to get bot message ID for calculation status"))?;

            match hedger.calculate_hedge_params(&hedge_request).await {
                Ok(params) => {
                    info!("Hedge parameters calculated for {}: {:?}", chat_id, params);
                    let confirmation_text = format!(
                        "Подтвердите параметры хеджирования для {}:\n\n\
                         Сумма: {:.2} {}\n\
                         Волатильность: {:.1}%\n\
                         --- Расчет ---\n\
                         Спот (брутто): ~{:.8} {}\n\
                         Фьючерс (нетто): ~{:.8} {}\n\
                         Требуемое плечо: ~{:.2}x (Макс: {:.1}x)\n\n\
                         Запустить хеджирование?",
                        symbol, sum, cfg.quote_currency,
                        volatility_percent,
                        params.spot_order_qty, symbol,
                        params.fut_order_qty, symbol,
                        (params.fut_order_qty * params.current_spot_price) / params.available_collateral.max(f64::EPSILON),
                        cfg.max_allowed_leverage
                    );
                    let kb = make_hedge_confirmation_keyboard();
                    bot.edit_message_text(chat_id, bot_msg_id, confirmation_text).reply_markup(kb).await?;

                    {
                        let mut state_guard = state_storage.write().expect("Lock failed");
                        if let Some(current_state @ UserState::AwaitingHedgeVolatility { .. }) = state_guard.get_mut(&chat_id) {
                            *current_state = UserState::AwaitingHedgeConfirmation {
                                symbol: symbol.clone(),
                                sum,
                                volatility: volatility_fraction,
                                last_bot_message_id: Some(bot_msg_id.0),
                           };
                           info!("User state for {} set to AwaitingHedgeConfirmation", chat_id);
                       } else {
                            warn!("State changed for {} before setting AwaitingHedgeConfirmation", chat_id);
                       }
                    }
                }
                Err(e) => {
                    error!("Hedge parameter calculation failed for {}: {}", chat_id, e);
                    let error_text = format!("❌ Ошибка расчета параметров: {}\nПопробуйте изменить сумму или волатильность.", e);
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("❌ Отмена", callback_data::CANCEL_DIALOG)
                    ]]);
                    bot.edit_message_text(chat_id, bot_msg_id, error_text).reply_markup(kb).await?;
                }
            }
        }
        Ok(_) => {
            warn!("User {} entered non-positive volatility: {}", chat_id, text);
             if let Some(bot_msg_id_int) = previous_bot_message_id {
                let error_text = format!("⚠️ Волатильность должна быть не отрицательной (в %). Введите снова:");
                let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
             }
        }
        Err(_) => {
            warn!("User {} entered invalid volatility format: {}", chat_id, text);
            if let Some(bot_msg_id_int) = previous_bot_message_id {
                let error_text = format!("⚠️ Неверный формат волатильности (в %). Введите снова:");
                let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
            }
        }
    }
     Ok(())
}

/// Обработчик колбэка подтверждения хеджа (кнопки yes/no с префиксом h_conf_)
pub async fn handle_hedge_confirm_callback<E>(
    bot: Bot,
    q: CallbackQuery, // q - будет перемещена только если payload == "yes" и нужно сообщение
    exchange: Arc<E>,
    state_storage: StateStorage,
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    // <<<--- ИСПРАВЛЕНО: Клонируем ID, если он String ---
    let query_id = q.id.clone(); // Если q.id - String, клонируем. Если Copy, clone() не повредит.
    // ---

    // Сначала проверяем данные, не перемещая q
    if let Some(data) = q.data.as_deref() { // Заимствуем данные
        if let Some(payload) = data.strip_prefix(callback_data::PREFIX_HEDGE_CONFIRM) {
            if payload == "yes" {
                // --- Ветка "Да" ---
                if let Some(msg) = q.message { // << Перемещение q.message происходит здесь
                    let chat_id = msg.chat().id;
                    info!("User {} confirmed hedge operation", chat_id);

                    let (symbol, sum, volatility_fraction) = {
                        let state_guard = state_storage.read().expect("Lock failed");
                        match state_guard.get(&chat_id) {
                            Some(UserState::AwaitingHedgeConfirmation { symbol, sum, volatility, .. }) => (symbol.clone(), *sum, *volatility),
                            _ => {
                                warn!("User {} confirmed hedge but was in wrong state", chat_id);
                                bot.answer_callback_query(query_id.clone()).text("Состояние изменилось, начните заново.").show_alert(true).await?; // Используем клон query_id
                                let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await;
                                { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
                                return Ok(());
                            }
                        }
                    };
                    { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }

                    let waiting_text = format!("⏳ Запуск хеджирования для {}...", symbol);
                    bot.edit_message_text(chat_id, msg.id(), waiting_text)
                       .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                       .await?;

                    let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction };
                    let hedger = Hedger::new((*exchange).clone(), (*cfg).clone());

                    match hedger.calculate_hedge_params(&hedge_request).await {
                        Ok(params) => {
                            info!("Hedge parameters re-calculated just before execution for {}: {:?}", chat_id, params);
                            spawn_hedge_task(
                                bot.clone(), exchange.clone(), cfg.clone(), db.clone(),
                                running_operations.clone(), chat_id, params, sum,
                                volatility_fraction * 100.0, msg,
                            ).await;
                        }
                        Err(e) => {
                            error!("Hedge parameter calculation failed just before execution for {}: {}", chat_id, e);
                            let error_text = format!("❌ Ошибка расчета параметров перед запуском: {}\nПопробуйте снова.", e);
                            let _ = bot.edit_message_text(chat_id, msg.id(), error_text)
                                     .reply_markup(navigation::make_main_menu_keyboard())
                                     .await;
                        }
                    }

                } else {
                    warn!("CallbackQuery missing message on 'yes' confirmation for {}", query_id);
                }

            } else if payload == "no" {
                // --- Ветка "Нет" ---
                 let chat_id = q.message.as_ref().map(|m| m.chat().id);
                 info!("User {} cancelled hedge at confirmation", chat_id.map_or("UNKNOWN".to_string(), |c| c.to_string()));

                // Вызываем оригинальную функцию handle_cancel_dialog с q
                navigation::handle_cancel_dialog(bot, q, state_storage).await?;
                // handle_cancel_dialog должен сам вызвать answer_callback_query
                return Ok(()); // Выходим

            } else {
                warn!("Invalid payload for hedge confirmation callback: {}", payload);
            }
        } else {
             // Если data есть, но префикс не совпадает, возможно это кнопка Отмена из диалога?
             if data == callback_data::CANCEL_DIALOG {
                 info!("User cancelled hedge dialog via cancel button");
                 // Вызываем оригинальную функцию handle_cancel_dialog с q
                 navigation::handle_cancel_dialog(bot, q, state_storage).await?;
                 return Ok(()); // Выходим
             } else {
                warn!("Invalid callback data format for hedge confirmation prefix: {}", data);
             }
        }
    } else {
        warn!("CallbackQuery missing data in handle_hedge_confirm_callback");
    }

    // Отвечаем на колбэк только если не вышли ранее
    bot.answer_callback_query(query_id).await?; // Используем клон query_id
    Ok(())
}

// TODO: Добавить обработчики для VIEW_ALL_PAIRS и пагинации (PREFIX_PAGE_*)
// TODO: Добавить обработчик для PREFIX_HEDGE_PAIR