// src/notifier/hedge_flow.rs

use super::{
    Command, StateStorage, UserState, RunningOperations, RunningOperationInfo, OperationType, callback_data, // Импорт общих типов
    navigation, // Для вызова handle_back_to_main, handle_cancel_dialog
    //progress, // Для колбэка прогресса (пока заглушка)
    //utils, // Если будут общие утилиты notifier
};
use crate::config::Config;
use crate::exchange::{Exchange, Balance};
use crate::storage::{Db, insert_hedge_operation, update_hedge_final_status, update_hedge_spot_order}; // Нужные функции DB
use crate::hedger::{Hedger, HedgeParams, HedgeProgressUpdate, HedgeProgressCallback, ORDER_FILL_TOLERANCE}; // Типы из Hedger
use crate::models::HedgeRequest; // Если HedgeRequest там
use std::sync::Arc;
use std::time::Duration; // Для sleep
use tokio::sync::Mutex as TokioMutex; // Для Arc<Mutex<>> в RunningOperationInfo
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId, ParseMode, // Добавлен ParseMode
};
use teloxide::utils::command::BotCommands;
use tracing::{info, warn, error};
use futures::future::FutureExt; // Для .boxed()

// --- Вспомогательные функции для этого модуля ---

/// Создает клавиатуру для шага подтверждения хеджа
fn make_hedge_confirmation_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("✅ Да, запустить", format!("{}{}", callback_data::PREFIX_HEDGE_CONFIRM, "yes")),
            InlineKeyboardButton::callback("❌ Нет, отмена", callback_data::CANCEL_DIALOG), // Отмена всего диалога
        ],
        // Можно добавить кнопку "Назад" для изменения параметров, но это усложнит логику состояний
        // vec![InlineKeyboardButton::callback("⬅️ Назад (изменить)", callback_data::BACK_TO_???)] // Куда назад? к вводу волатильности?
    ])
}

/// Создает клавиатуру для диалога (запрос суммы, волатильности)
fn make_dialog_keyboard() -> InlineKeyboardMarkup {
     InlineKeyboardMarkup::new(vec![vec![
        // Пока отмена будет возвращать в главное меню через общий обработчик
        InlineKeyboardButton::callback("❌ Отмена", callback_data::CANCEL_DIALOG),
        // Кнопка "Назад" здесь пока не реализована, т.к. требует управления стеком состояний
        // InlineKeyboardButton::callback("⬅️ Назад", "...")
    ]])
}

/// Запускает фоновую задачу хеджирования
async fn spawn_hedge_task<E>(
    bot: Bot,
    exchange: Arc<E>,
    cfg: Arc<Config>,
    db: Arc<Db>,
    running_operations: RunningOperations,
    chat_id: ChatId,
    params: HedgeParams, // Рассчитанные параметры
    initial_sum: f64, // Для отображения в сообщениях
    volatility_percent: f64, // Для отображения
    waiting_message: Message, // Сообщение "Запускаю..." для редактирования
)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let hedger = Hedger::new((*exchange).clone(), (*cfg).clone()); // Клонируем зависимости для задачи
    let operation_id_result = insert_hedge_operation(
        db.as_ref(),
        chat_id.0,
        &params.symbol,
        &cfg.quote_currency,
        initial_sum,
        volatility_percent / 100.0, // Сохраняем как долю
        params.spot_order_qty,
        params.fut_order_qty,
    )
    .await;

    let operation_id = match operation_id_result {
        Ok(id) => {
            info!("op_id:{}: Created DB record for hedge operation.", id);
            id
        }
        Err(e) => {
            error!("Failed to insert hedge operation into DB: {}", e);
            let error_text = format!("❌ Критическая ошибка БД при создании записи операции: {}", e);
            // Редактируем исходное сообщение об ошибке
            let _ = bot.edit_message_text(chat_id, waiting_message.id, error_text)
                       .reply_markup(navigation::make_main_menu_keyboard()) // Предлагаем вернуться в меню
                       .await;
            return; // Прерываем выполнение задачи
        }
    };

    // Создаем хранилища для состояния ордера и прогресса
    let current_spot_order_id_storage = Arc::new(TokioMutex::new(None::<String>));
    let total_filled_qty_storage = Arc::new(TokioMutex::new(0.0f64));

    let bot_clone = bot.clone();
    let cfg_clone = cfg.clone();
    let db_clone = db.clone();
    let symbol_clone = params.symbol.clone();
    let current_spot_order_id_storage_clone = current_spot_order_id_storage.clone();
    let total_filled_qty_storage_clone = total_filled_qty_storage.clone();
    let running_operations_clone = running_operations.clone(); // Клон для удаления из мапы

    // --- Создание колбэка прогресса ---
    let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
        let bot_for_callback = bot_clone.clone();
        let msg_id = waiting_message.id;
        let chat_id_for_callback = chat_id;
        let sum = initial_sum;
        let symbol = symbol_clone.clone(); // Клонируем еще раз для замыкания
        let qc = cfg_clone.quote_currency.clone();
        let operation_id_for_callback = operation_id; // Захватываем ID

        async move {
            // Формируем текст прогресса
            let filled_percent = if update.target_qty > ORDER_FILL_TOLERANCE {
                (update.filled_qty / update.target_qty) * 100.0
            } else {
                0.0
            };
            // Простой текстовый прогресс-бар
            let progress_bar_len = 10;
            let filled_blocks = (filled_percent / (100.0 / progress_bar_len as f64)).round() as usize;
            let empty_blocks = progress_bar_len - filled_blocks;
            let progress_bar = format!("[{}{}]", "█".repeat(filled_blocks), "░".repeat(empty_blocks)); // Пример бара

            let status_text = if update.is_replacement { "(Ордер переставлен)" } else { "" };

            let text = format!(
                "⏳ Хеджирование ID:{} {} {:.2} {} в процессе...\n{}\nРын.цена: {:.2}\nОрдер на покупку: {:.2} {}{}\nИсполнено: {:.6}/{:.6} ({:.1}%)",
                operation_id_for_callback, // Показываем ID операции
                progress_bar,
                sum, qc, symbol,
                update.current_spot_price,
                update.new_limit_price, status_text,
                update.filled_qty, update.target_qty, filled_percent
            );

            // Формируем клавиатуру с кнопкой отмены для ЭТОЙ операции
            let cancel_callback_data = format!("{}{}", callback_data::PREFIX_CANCEL_ACTIVE_OP, operation_id_for_callback);
            let cancel_button = InlineKeyboardButton::callback("❌ Отменить эту операцию", cancel_callback_data);
            let kb = InlineKeyboardMarkup::new(vec![vec![cancel_button]]);

            // Редактируем сообщение
            if let Err(e) = bot_for_callback.edit_message_text(chat_id_for_callback, msg_id, text)
                .reply_markup(kb)
                .parse_mode(ParseMode::MarkdownV2) // Если используем спецсимволы в баре
                .await {
                // Игнорируем ошибку "message is not modified", логируем остальные
                if !e.to_string().contains("not modified") {
                    warn!("op_id:{}: Progress callback failed: {}", operation_id_for_callback, e);
                }
            }
            Ok(())
        }.boxed() // Оборачиваем Future в BoxFuture
    });
    // --- Конец колбэка прогресса ---

    // Создаем задачу для хеджирования
    let symbol_for_task = params.symbol.clone();
    let task = tokio::spawn(async move {
        // Вызываем hedger.run_hedge
        let result = hedger.run_hedge(
            params,
            progress_callback,
            current_spot_order_id_storage_clone, // Передаем клоны хранилищ
            total_filled_qty_storage_clone,
            operation_id,
            db_clone.as_ref(),
        ).await;

        // Удаляем информацию об операции из `running_operations` после завершения (успешного или нет)
        // кроме случая, когда отмена произошла через кнопку (там удаление происходит раньше)
        let is_cancelled_by_button = result.is_err() && result.as_ref().err().map_or(false, |e| e.to_string().contains("cancelled by user")); // Нужен специфичный текст ошибки при отмене из active_ops
        if !is_cancelled_by_button {
            running_operations_clone.lock().await.remove(&(chat_id, operation_id));
            info!("op_id:{}: Removed running operation info for chat_id: {}", operation_id, chat_id);
        } else {
            info!("op_id:{}: Operation was cancelled via button, info already removed.", operation_id);
        }

        // Обрабатываем результат хеджирования
        match result {
            Ok((spot_qty_gross, fut_qty_net, final_spot_value_gross)) => {
                 info!( "op_id:{}: Hedge OK. Spot Gross: {}, Fut Net: {}, Value: {:.2}", operation_id, spot_qty_gross, fut_qty_net, final_spot_value_gross );
                 // Получаем финальный баланс спота (лучше после небольшой паузы)
                 tokio::time::sleep(Duration::from_millis(500)).await;
                 let final_net_spot_balance = match exchange.get_balance(&symbol_for_task).await {
                     Ok(b) => { info!("op_id:{}: Fetched final spot balance: {}", operation_id, b.free); b.free },
                     Err(e) => {
                         warn!("op_id:{}: Failed get final spot balance after hedge: {}. Using calculated gross.", operation_id, e);
                         spot_qty_gross // В крайнем случае показываем брутто
                     }
                 };

                 let success_text = format!(
                     "✅ Хеджирование ID:{} ~{:.2} {} ({}) при V={:.1}% завершено:\n\n🟢 Спот куплено (брутто): {:.8}\nspot_balance_check {:.8}\n🔴 Фьюч продано (нетто): {:.8}",
                     operation_id,
                     final_spot_value_gross, cfg.quote_currency, symbol_for_task,
                     volatility_percent,
                     spot_qty_gross, // Показываем БРУТТО спота
                     final_net_spot_balance, // Показываем реальный баланс спота после
                     fut_qty_net, // Показываем НЕТТО фьюча
                 );
                 // Редактируем сообщение об успехе
                 let _ = bot.edit_message_text(chat_id, waiting_message.id, success_text)
                            .reply_markup(navigation::make_main_menu_keyboard()) // Показываем главное меню
                            .await;
             }
             Err(e) => {
                 // Ошибка может быть как из hedger, так и из-за отмены
                 if is_cancelled_by_button {
                     // Сообщение об отмене уже должно было быть отправлено из active_ops
                     info!("op_id:{}: Hedge task finished after cancellation via button.", operation_id);
                 } else {
                     error!("op_id:{}: Hedge execution failed: {}", operation_id, e);
                     // Статус в БД должен был обновиться внутри run_hedge или при отмене
                     let error_text = format!("❌ Ошибка хеджирования ID:{}: {}", operation_id, e);
                      let _ = bot.edit_message_text(chat_id, waiting_message.id, error_text)
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
        symbol: symbol_for_task.clone(), // Используем ту же переменную
        bot_message_id: waiting_message.id.0,
        current_spot_order_id: current_spot_order_id_storage,
        total_filled_spot_qty: total_filled_qty_storage,
    };
    // Добавляем в хранилище активных операций
    running_operations.lock().await.insert((chat_id, operation_id), info);
    info!("op_id:{}: Stored running hedge info.", operation_id);

}


// --- Обработчики команд и колбэков ---

/// Обработчик команды /hedge [SYMBOL]
pub async fn handle_hedge_command<E>(
    bot: Bot,
    msg: Message,
    symbol_arg: String, // Может быть пустой
    exchange: Arc<E>,
    state_storage: StateStorage,
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let symbol = symbol_arg.trim().to_uppercase();

    // Сбрасываем состояние и чистим чат (логика из старого handle_command)
    // TODO: Вынести логику чистки и сброса в отдельную утилиту или делать это в dispatch_command
    // пока оставляем здесь для простоты переноса
    let mut previous_bot_message_id: Option<i32> = None;
    {
        let mut state_guard = state_storage.write().expect("Lock failed");
        if let Some(old_state) = state_guard.get(&chat_id) {
             previous_bot_message_id = match old_state {
                 UserState::AwaitingHedgeAssetSelection { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingHedgeSum { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingHedgeVolatility { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingHedgeConfirmation { last_bot_message_id, .. } => *last_bot_message_id,
                 // ... другие состояния ...
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
    // Удаляем команду пользователя
    let user_msg_id = msg.id;
    if let Err(e) = bot.delete_message(chat_id, user_msg_id).await { warn!("Failed delete user command msg: {}", e); }


    if symbol.is_empty() {
        // Если символ не указан, запускаем выбор актива
        info!("Processing /hedge command without symbol for chat_id: {}", chat_id);
        prompt_asset_selection(bot, chat_id, state_storage, exchange, cfg, db, None).await?;
    } else {
        // Если символ указан, проверяем его и переходим к вводу суммы
        info!("Processing /hedge command for chat_id: {}, symbol: {}", chat_id, symbol);
        // TODO: Валидация символа на бирже (наличие spot/linear)
        // Пока просто переходим к сумме
        let text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
        let kb = make_dialog_keyboard();
        let bot_msg = bot.send_message(chat_id, text).reply_markup(kb).await?;

        // Устанавливаем состояние AwaitingHedgeSum
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
        // Запускаем выбор актива, редактируя текущее сообщение
        prompt_asset_selection(bot, chat_id, state_storage, exchange, cfg, db, Some(msg.id())).await?;
    } else {
        warn!("CallbackQuery missing message in handle_start_hedge_callback");
    }
    bot.answer_callback_query(q.id).await?; // Отвечаем на колбэк
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

    // Показываем индикатор ожидания
    if let Some(msg_id) = bot_message_id {
        let kb = InlineKeyboardMarkup::new(vec![vec![
             InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN) // Кнопка Назад
        ]]);
         // Игнорируем ошибку, если не удалось отредактировать
        let _ = bot.edit_message_text(chat_id, msg_id, loading_text).reply_markup(kb).await;
    } else {
        let sent_msg = bot.send_message(chat_id, loading_text).await?;
        bot_message_id = Some(sent_msg.id);
    }

    // Получаем балансы для показа кнопок из кошелька
    match wallet_info::get_formatted_balances(exchange.as_ref(), &cfg.quote_currency, false).await { // Не показываем стоимость
        Ok((_, asset_data)) => {
            let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
            let mut assets_found = false;

            for (coin, free, locked) in asset_data {
                // Показываем кнопку, если это не quote_currency
                if coin != cfg.quote_currency {
                     assets_found = true;
                     // TODO: Проверять, доступна ли пара coin/quote_currency для хеджа (spot+linear) на бирже
                     let callback_data_asset = format!("{}{}", callback_data::PREFIX_HEDGE_ASSET, coin);
                     buttons.push(vec![InlineKeyboardButton::callback(
                         format!("💼 {} (free: {:.6}, locked: {:.6})", coin, free, locked), // Меньше знаков для краткости
                         callback_data_asset,
                     )]);
                }
            }

            let mut text = "Выберите актив из кошелька для хеджирования:".to_string();
            if !assets_found {
                text = "ℹ️ В вашем кошельке нет активов (кроме {}), подходящих для хеджирования.\n".to_string();
            }

             // Добавляем опцию ручного ввода
             text.push_str("\nИли отправьте тикер актива (например, BTC) сообщением.");

            // TODO: Добавить кнопку "Показать все доступные пары" согласно ТЗ
            // buttons.push(vec![InlineKeyboardButton::callback("🌐 Показать все пары", callback_data::VIEW_ALL_PAIRS)]);

            // Добавляем кнопку "Назад"
            buttons.push(vec![InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)]);

            let keyboard = InlineKeyboardMarkup::new(buttons);

            // Редактируем сообщение с кнопками
            if let Some(msg_id) = bot_message_id {
                 if let Err(e) = bot.edit_message_text(chat_id, msg_id, text).reply_markup(keyboard).await {
                    error!("Failed to edit message for asset selection: {}. Sending new.", e);
                    bot_message_id = Some(bot.send_message(chat_id, text).reply_markup(keyboard).await?.id); // Отправляем новое
                 }
            } else {
                 bot_message_id = Some(bot.send_message(chat_id, text).reply_markup(keyboard).await?.id); // Отправляем новое, если не было ID
            }

            // Устанавливаем состояние ожидания выбора актива
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
             // Сбрасываем состояние
              { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
             return Err(e.into()); // Возвращаем ошибку
        }
    }
    Ok(())
}

/// Обработчик колбэка выбора актива для хеджа (кнопка с префиксом h_asset_)
pub async fn handle_hedge_asset_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    _exchange: Arc<E>, // Пока не используется для валидации
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

            // Проверяем состояние пользователя
            let is_correct_state = matches!(
                state_storage.read().expect("Lock failed").get(&chat_id),
                Some(UserState::AwaitingHedgeAssetSelection { .. })
            );

            if is_correct_state {
                // Переходим к запросу суммы
                let text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                let kb = make_dialog_keyboard();
                bot.edit_message_text(chat_id, msg.id(), text).reply_markup(kb).await?;

                // Обновляем состояние
                {
                    let mut state_guard = state_storage.write().expect("Lock failed");
                    // Проверяем еще раз, что состояние не изменилось пока ждали edit_message_text
                    if let Some(current_state @ UserState::AwaitingHedgeAssetSelection { .. }) = state_guard.get_mut(&chat_id) {
                         *current_state = UserState::AwaitingHedgeSum {
                            symbol: symbol.to_string(),
                            last_bot_message_id: Some(msg.id().0),
                        };
                        info!("User state for {} set to AwaitingHedgeSum for {}", chat_id, symbol);
                    } else {
                         warn!("State changed unexpectedly for {} before setting AwaitingHedgeSum", chat_id);
                         // Если состояние изменилось, лучше ничего не делать или вернуть в меню
                         let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await;
                    }
                }
            } else {
                 warn!("User {} clicked hedge asset button but was in wrong state", chat_id);
                 // Сбрасываем состояние и возвращаем в меню
                 { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
                 let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await;
                 bot.answer_callback_query(q.id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                 return Ok(()); // Возвращаемся, чтобы не отвечать на колбэк второй раз
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
    exchange: Arc<E>,
    state_storage: StateStorage,
    cfg: Arc<Config>,
    _db: Arc<Db>, // Пока не используется
) -> anyhow::Result<()>
 where
     E: Exchange + Clone + Send + Sync + 'static,
 {
    let chat_id = msg.chat.id;
    let message_id = msg.id; // ID сообщения пользователя
    let text = msg.text().unwrap_or("").trim().to_uppercase();

    if text.is_empty() || text.starts_with('/') {
        // Игнорируем пустые сообщения и команды
         if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete ignored message: {}", e); }
        return Ok(());
    }

    // Проверяем состояние
    let previous_bot_message_id = {
         let state_guard = state_storage.read().expect("Lock failed");
         match state_guard.get(&chat_id) {
             Some(UserState::AwaitingHedgeAssetSelection { last_bot_message_id }) => *last_bot_message_id,
             // TODO: Добавить обработку для UserState::ViewingAllPairs для фильтрации
             _ => {
                 // Не то состояние, удаляем сообщение пользователя
                 if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete unexpected text message: {}", e); }
                 return Ok(());
             }
         }
    };

    info!("User {} entered ticker '{}' for hedge", chat_id, text);

    // Удаляем сообщение пользователя
    if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete user ticker message: {}", e); }

    // TODO: Валидация тикера на бирже (наличие spot/linear)
    // Пока считаем, что любой введенный тикер валиден для примера
    let is_valid_ticker = true; // Заглушка

    if is_valid_ticker {
        // Переходим к запросу суммы, редактируя предыдущее сообщение бота
        let prompt_text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, text);
        let kb = make_dialog_keyboard();

        if let Some(bot_msg_id_int) = previous_bot_message_id {
            let bot_msg_id = MessageId(bot_msg_id_int);
            match bot.edit_message_text(chat_id, bot_msg_id, prompt_text).reply_markup(kb).await {
                Ok(_) => {
                    // Обновляем состояние
                     {
                        let mut state_guard = state_storage.write().expect("Lock failed");
                         if let Some(current_state @ UserState::AwaitingHedgeAssetSelection { .. }) = state_guard.get_mut(&chat_id) {
                             *current_state = UserState::AwaitingHedgeSum {
                                symbol: text.clone(), // Сохраняем введенный символ
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
                     // Можно попробовать отправить новое сообщение или вернуть в меню
                     let _ = navigation::show_main_menu(&bot, chat_id, None).await; // Отправляем новое меню
                     { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); } // Сброс состояния
                }
            }
        } else {
             warn!("No previous bot message id found for chat_id {} to edit for sum prompt", chat_id);
             // Отправляем новое сообщение
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
        // Тикер невалиден
        let error_text = format!("❌ Символ '{}' не найден или не подходит для хеджирования. Попробуйте другой.", text);
        if let Some(bot_msg_id_int) = previous_bot_message_id {
             // Редактируем старое сообщение об ошибке
             let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
             // Состояние остается AwaitingHedgeAssetSelection
        } else {
             // Отправляем новое сообщение об ошибке
             let _ = bot.send_message(chat_id, error_text).await;
             // Состояние остается AwaitingHedgeAssetSelection (если не было ID) или сбрасываем
             // { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }
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
    let message_id = msg.id; // ID сообщения пользователя
    let text = msg.text().unwrap_or("").trim();

    // Получаем символ и ID сообщения бота из состояния
    let (symbol, previous_bot_message_id) = {
        let state_guard = state_storage.read().expect("Lock failed");
        match state_guard.get(&chat_id) {
            Some(UserState::AwaitingHedgeSum { symbol, last_bot_message_id }) => (symbol.clone(), *last_bot_message_id),
            _ => {
                 // Не то состояние, удаляем сообщение пользователя
                 if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete unexpected sum message: {}", e); }
                 return Ok(());
            }
        }
    };

     // Удаляем сообщение пользователя
     if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete user sum message: {}", e); }

    // Парсим и валидируем сумму
    match text.parse::<f64>() {
         Ok(sum) if sum > 0.0 => {
             // Сумма введена корректно, переходим к запросу волатильности
             info!("User {} entered sum {} for hedge {}", chat_id, sum, symbol);
             let prompt_text = format!("Введите ожидаемую волатильность для {} {} (%):", sum, cfg.quote_currency);
             let kb = make_dialog_keyboard();

             if let Some(bot_msg_id_int) = previous_bot_message_id {
                 let bot_msg_id = MessageId(bot_msg_id_int);
                 match bot.edit_message_text(chat_id, bot_msg_id, prompt_text).reply_markup(kb).await {
                    Ok(_) => {
                        // Обновляем состояние
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
             // Сумма не положительная
             warn!("User {} entered non-positive sum: {}", chat_id, text);
              if let Some(bot_msg_id_int) = previous_bot_message_id {
                  let error_text = format!("⚠️ Сумма должна быть положительной. Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                  let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await; // Клавиатура остается прежней
             } else { /* Не должно произойти, но можно отправить новое сообщение */ }
         }
         Err(_) => {
             // Не удалось распарсить
             warn!("User {} entered invalid sum format: {}", chat_id, text);
             if let Some(bot_msg_id_int) = previous_bot_message_id {
                  let error_text = format!("⚠️ Неверный формат суммы. Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                  let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
             } else { /* ... */ }
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
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
 where
     E: Exchange + Clone + Send + Sync + 'static,
 {
    let chat_id = msg.chat.id;
    let message_id = msg.id; // ID сообщения пользователя
    let text = msg.text().unwrap_or("").trim();

    // Получаем символ, сумму и ID сообщения бота из состояния
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

     // Удаляем сообщение пользователя
     if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete user volatility message: {}", e); }

    // Парсим и валидируем волатильность
    match text.trim_end_matches('%').trim().parse::<f64>() {
        Ok(volatility_percent) if volatility_percent >= 0.0 => {
             info!("User {} entered volatility {}% for hedge {} {}", chat_id, volatility_percent, sum, symbol);
             let volatility_fraction = volatility_percent / 100.0;

            // --- Расчет параметров хеджа ---
            let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction };
            let hedger = Hedger::new((*exchange).clone(), (*cfg).clone()); // Клонируем зависимости

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
                     // Параметры рассчитаны, переходим к подтверждению
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
                         (params.fut_order_qty * params.current_spot_price) / params.available_collateral.max(f64::EPSILON), // Пересчет плеча для показа
                         cfg.max_allowed_leverage
                     );
                     let kb = make_hedge_confirmation_keyboard();

                     // Редактируем сообщение с подтверждением
                     bot.edit_message_text(chat_id, bot_msg_id, confirmation_text).reply_markup(kb).await?;

                     // Обновляем состояние
                     {
                         let mut state_guard = state_storage.write().expect("Lock failed");
                         if let Some(current_state @ UserState::AwaitingHedgeVolatility { .. }) = state_guard.get_mut(&chat_id) {
                             *current_state = UserState::AwaitingHedgeConfirmation {
                                symbol: symbol.clone(),
                                sum,
                                volatility: volatility_fraction, // Сохраняем долю
                                // Можно сохранить и params, если нужно
                                last_bot_message_id: Some(bot_msg_id.0),
                            };
                             info!("User state for {} set to AwaitingHedgeConfirmation", chat_id);
                         } else {
                              warn!("State changed for {} before setting AwaitingHedgeConfirmation", chat_id);
                         }
                     }
                 }
                 Err(e) => {
                     // Ошибка расчета параметров
                     error!("Hedge parameter calculation failed for {}: {}", chat_id, e);
                     let error_text = format!("❌ Ошибка расчета параметров: {}\nПопробуйте изменить сумму или волатильность.", e);
                     let kb = InlineKeyboardMarkup::new(vec![vec![
                         // Позволяем вернуться к вводу волатильности или отменить
                         // TODO: Реализовать возврат к AwaitingHedgeVolatility
                         // InlineKeyboardButton::callback("⬅️ Назад (к волатильности)", "back_to_volatility"),
                         InlineKeyboardButton::callback("❌ Отмена", callback_data::CANCEL_DIALOG)
                     ]]);
                     bot.edit_message_text(chat_id, bot_msg_id, error_text).reply_markup(kb).await?;
                     // Состояние НЕ меняем, остаемся в AwaitingHedgeVolatility или сбрасываем в None при отмене
                     // { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); } // Или не сбрасывать? Зависит от UX
                 }
            }
        }
        Ok(_) => {
             // Волатильность не положительная
             warn!("User {} entered non-positive volatility: {}", chat_id, text);
              if let Some(bot_msg_id_int) = previous_bot_message_id {
                 let error_text = format!("⚠️ Волатильность должна быть не отрицательной (в %). Введите снова:");
                 let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
             }
        }
        Err(_) => {
            // Не удалось распарсить
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
        let chat_id = msg.chat().id;
        if let Some(payload) = data.strip_prefix(callback_data::PREFIX_HEDGE_CONFIRM) {
             if payload == "yes" {
                 info!("User {} confirmed hedge operation", chat_id);

                // Получаем параметры из состояния
                let (symbol, sum, volatility_fraction) = {
                    let state_guard = state_storage.read().expect("Lock failed");
                     match state_guard.get(&chat_id) {
                         Some(UserState::AwaitingHedgeConfirmation { symbol, sum, volatility, .. }) => (symbol.clone(), *sum, *volatility),
                         _ => {
                             warn!("User {} confirmed hedge but was in wrong state", chat_id);
                             bot.answer_callback_query(q.id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                             let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await; // Возврат в меню
                              { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); } // Сброс
                             return Ok(());
                         }
                     }
                };
                 // Сбрасываем состояние ПЕРЕД запуском долгой операции
                 { state_storage.write().expect("Lock failed").insert(chat_id, UserState::None); }

                 // Показываем индикатор запуска
                 let waiting_text = format!("⏳ Запуск хеджирования для {}...", symbol);
                 // Убираем кнопки подтверждения
                 bot.edit_message_text(chat_id, msg.id(), waiting_text)
                    .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                    .await?;

                 // Пересчитываем параметры (на случай изменения цены) и запускаем задачу
                 let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction };
                 let hedger = Hedger::new((*exchange).clone(), (*cfg).clone());

                  match hedger.calculate_hedge_params(&hedge_request).await {
                     Ok(params) => {
                         info!("Hedge parameters re-calculated just before execution for {}: {:?}", chat_id, params);
                         // Запускаем фоновую задачу
                         spawn_hedge_task(
                             bot.clone(),
                             exchange.clone(),
                             cfg.clone(),
                             db.clone(),
                             running_operations.clone(),
                             chat_id,
                             params,
                             sum,
                             volatility_fraction * 100.0, // Передаем % для показа
                             msg.clone(), // Передаем сообщение для редактирования прогресса
                         ).await;
                     }
                     Err(e) => {
                         error!("Hedge parameter calculation failed just before execution for {}: {}", chat_id, e);
                         let error_text = format!("❌ Ошибка расчета параметров перед запуском: {}\nПопробуйте снова.", e);
                         let _ = bot.edit_message_text(chat_id, msg.id(), error_text)
                                     .reply_markup(navigation::make_main_menu_keyboard())
                                     .await;
                         // Состояние уже сброшено в None
                     }
                 }

             } else if payload == "no" {
                  info!("User {} cancelled hedge at confirmation", chat_id);
                  // Вызываем обработчик отмены диалога
                  navigation::handle_cancel_dialog(bot, q, state_storage).await?;
                  return Ok(()); // Возвращаемся, т.к. колбэк уже обработан внутри cancel_dialog
             } else {
                  warn!("Invalid payload for hedge confirmation callback: {}", payload);
             }
        } else {
             warn!("Invalid callback data format for hedge confirmation: {}", data);
        }
    } else {
         warn!("CallbackQuery missing data or message in handle_hedge_confirm_callback");
    }
     bot.answer_callback_query(q.id).await?;
     Ok(())
}

// TODO: Добавить обработчики для VIEW_ALL_PAIRS и пагинации (PREFIX_PAGE_*)
// TODO: Добавить обработчик для PREFIX_HEDGE_PAIR