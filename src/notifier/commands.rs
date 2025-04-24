// src/notifier/commands.rs

use crate::config::Config;
use crate::exchange::Exchange;
// --- ИЗМЕНЕНО: Добавляем нужные импорты ---
use crate::hedger::Hedger;
use crate::storage::{Db, get_completed_unhedged_ops_for_symbol, HedgeOperation}; // Импортируем HedgeOperation
// --- КОНЕЦ ИЗМЕНЕНИЙ ---
use super::{Command, StateStorage, UserState};
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use teloxide::utils::command::BotCommands;
use tracing::{warn, error, info};
use chrono::{DateTime, Utc, TimeZone, LocalResult}; // Импортируем LocalResult

// Вспомогательная функция для "чистки" чата
async fn cleanup_chat(bot: &Bot, chat_id: ChatId, user_msg_id: MessageId, bot_msg_id: Option<i32>) {
    if let Some(id_int) = bot_msg_id {
        if let Err(e) = bot.delete_message(chat_id, MessageId(id_int)).await {
            warn!("Failed to delete previous bot message {}: {}", id_int, e);
        }
    }
    if let Err(e) = bot.delete_message(chat_id, user_msg_id).await {
        warn!("Failed to delete user command message {}: {}", user_msg_id, e);
    }
}

// Основной обработчик команд
pub async fn handle_command<E>(
    bot: Bot,
    msg: Message,
    cmd: Command,
    mut exchange: E, // Сделаем mut для check_connection
    state_storage: StateStorage,
    cfg: Config,
    db: &Db,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let message_id = msg.id;

    // Сброс состояния и чистка чата при получении новой команды
    let mut previous_bot_message_id: Option<i32> = None;
    {
        let mut state_guard = state_storage
            .write()
            .expect("Failed to acquire write lock on state storage");
        if let Some(old_state) = state_guard.get(&chat_id) {
             // Определяем ID предыдущего сообщения бота для удаления
            previous_bot_message_id = match old_state {
                UserState::AwaitingAssetSelection { last_bot_message_id, .. } => *last_bot_message_id,
                UserState::AwaitingSum { last_bot_message_id, .. } => *last_bot_message_id,
                UserState::AwaitingVolatility { last_bot_message_id, .. } => *last_bot_message_id,
                UserState::AwaitingUnhedgeSelection { last_bot_message_id, .. } => *last_bot_message_id, // Добавлено новое состояние
                UserState::None => None,
            };
        }
        // Сбрасываем состояние, если оно не было None
        if !matches!(state_guard.get(&chat_id), Some(UserState::None) | None) {
             info!("Resetting user state for {} due to new command: {:?}", chat_id, cmd);
             state_guard.insert(chat_id, UserState::None);
        }
    }
    cleanup_chat(&bot, chat_id, message_id, previous_bot_message_id).await;


    match cmd {
        Command::Help => {
            let kb = InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("✅ Статус", "status"),
                    InlineKeyboardButton::callback("💼 Баланс", "wallet"),
                ],
                vec![
                    InlineKeyboardButton::callback("🪙 Баланс монеты", "balance"),
                    InlineKeyboardButton::callback("⚙️ Хедж", "hedge"),
                    InlineKeyboardButton::callback("🛠 Расхедж", "unhedge"), // Кнопка покажет выбор актива
                    InlineKeyboardButton::callback("📈 Funding", "funding"),
                ],
            ]);
            bot.send_message(chat_id, Command::descriptions().to_string())
                .reply_markup(kb)
                .await?;
        }
        Command::Status => {
            match exchange.check_connection().await {
                Ok(_) => { bot.send_message(chat_id, "✅ Бот запущен и успешно подключен к бирже.").await?; }
                Err(e) => { bot.send_message(chat_id, format!("⚠️ Бот запущен, но есть проблема с подключением к бирже: {}", e)).await?; }
            }
        }
        Command::Wallet => {
             info!("Fetching wallet balance for chat_id: {}", chat_id);
            match exchange.get_all_balances().await {
                Ok(balances) => {
                    let mut text = "💼 Баланс кошелька:\n".to_string();
                    let mut found_assets = false;
                    let mut sorted_balances: Vec<_> = balances.into_iter().collect();
                    sorted_balances.sort_by_key(|(coin, _)| coin.clone());
                    for (coin, bal) in sorted_balances {
                        use crate::hedger::ORDER_FILL_TOLERANCE;
                        if bal.free > ORDER_FILL_TOLERANCE || bal.locked > ORDER_FILL_TOLERANCE {
                            text.push_str(&format!( "• {}: ️free {:.8}, locked {:.8}\n", coin, bal.free, bal.locked )); // Исправлено на .8
                            found_assets = true;
                        }
                    }
                    if !found_assets { text = "ℹ️ Ваш кошелек пуст.".to_string(); }
                    bot.send_message(chat_id, text).await?;
                }
                Err(e) => {
                    error!("Failed to fetch wallet balance for chat_id: {}: {}", chat_id, e);
                    bot.send_message(chat_id, format!("❌ Не удалось получить баланс кошелька: {}", e)).await?;
                }
            }
        }
        Command::Balance(arg) => {
            let sym = arg.trim().to_uppercase();
            if sym.is_empty() { bot.send_message(chat_id, "Использование: /balance <SYMBOL>").await?; }
            else {
                info!("Fetching balance for {} for chat_id: {}", sym, chat_id);
                match exchange.get_balance(&sym).await {
                    Ok(b) => { bot.send_message( chat_id, format!("💰 {}: free {:.8}, locked {:.8}", sym, b.free, b.locked), ).await?; } // Исправлено на .8
                    Err(e) => { error!("Failed to fetch balance for {} for chat_id: {}: {}", sym, chat_id, e); bot.send_message(chat_id, format!("❌ Не удалось получить баланс {}: {}", sym, e)).await?; }
                }
            }
        }
        Command::Hedge(arg) => {
            let symbol = arg.trim().to_uppercase();
            if symbol.is_empty() { bot.send_message(chat_id, "Использование: /hedge <SYMBOL>\nИли используйте кнопку 'Хедж' из /help.").await?; }
            else {
                info!("Starting hedge dialog via command for chat_id: {}, symbol: {}", chat_id, symbol);
                let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),]]);
                let bot_msg = bot.send_message( chat_id, format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol), ).reply_markup(kb).await?;
                { let mut state = state_storage.write().expect("Lock failed"); state.insert(chat_id, UserState::AwaitingSum { symbol: symbol.clone(), last_bot_message_id: Some(bot_msg.id.0), }); info!("User state for {} set to AwaitingSum for symbol {}", chat_id, symbol); }
            }
        }

        // --- ОБРАБОТКА /unhedge <SYMBOL> ---
        Command::Unhedge(arg) => {
            let symbol = arg.trim().to_uppercase();
            if symbol.is_empty() {
                bot.send_message(chat_id, "Использование: /unhedge <SYMBOL>\n(Например: /unhedge BTC)").await?;
                return Ok(());
            }

            info!("Looking for completed hedges for symbol {} for chat_id {}", symbol, chat_id);

            match get_completed_unhedged_ops_for_symbol(db, chat_id.0, &symbol).await {
                Ok(operations) => {
                    if operations.is_empty() {
                        bot.send_message(chat_id, format!("ℹ️ Не найдено завершенных операций хеджирования для {}, которые можно было бы расхеджировать.", symbol)).await?;
                    } else if operations.len() == 1 {
                        // Если найдена ровно одна операция, запускаем сразу
                        let op_to_unhedge = operations.into_iter().next().unwrap();
                        let op_id = op_to_unhedge.id;
                        info!("Found single operation to unhedge (op_id: {}). Starting unhedge directly.", op_id);

                        let waiting_msg = bot.send_message(chat_id, format!("⏳ Запускаю расхеджирование для операции ID:{} ({})...", op_id, symbol)).await?;

                        // Запускаем run_unhedge в отдельной задаче
                        let hedger = Hedger::new(exchange.clone(), cfg.clone());
                        let bot_clone = bot.clone();
                        let waiting_msg_id = waiting_msg.id;
                        let symbol_clone = symbol.clone();
                        let db_clone = db.clone();

                        tokio::spawn(async move {
                            match hedger.run_unhedge(op_to_unhedge, &db_clone).await {
                                Ok((sold, bought)) => {
                                    info!("Direct unhedge successful for op_id: {}. Sold spot: {}, Bought fut: {}", op_id, sold, bought);
                                    let _ = bot_clone.edit_message_text(
                                        chat_id,
                                        waiting_msg_id,
                                        // Используем реальные sold/bought в сообщении
                                        format!(
                                            "✅ Расхеджирование {} (из операции ID:{}) завершено:\n\n🟢 Продано спота: {:.8}\n🔴 Куплено фьюча: {:.8}",
                                            symbol_clone, op_id, sold, bought,
                                        ),
                                    )
                                    .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                                    .await;
                                }
                                Err(e) => {
                                    error!("Direct unhedge failed for op_id: {}: {}", op_id, e);
                                    let _ = bot_clone.edit_message_text(
                                        chat_id,
                                        waiting_msg_id,
                                        format!("❌ Ошибка расхеджирования операции ID:{}: {}", op_id, e)
                                     )
                                     .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                                     .await;
                                }
                            }
                        });
                    } else {
                        // Если найдено несколько операций, предлагаем выбрать
                        info!("Found {} operations to unhedge for {}. Prompting user selection.", operations.len(), symbol);
                        let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
                        // Сортируем по ID в обратном порядке (последние сверху)
                        let mut sorted_ops = operations.clone(); // Клонируем для сохранения в состоянии
                        sorted_ops.sort_by_key(|op| std::cmp::Reverse(op.id));

                        for op in &sorted_ops { // Используем sorted_ops для отображения
                            // --- ИСПРАВЛЕНО: Обработка LocalResult от timestamp_opt ---
                            let timestamp_dt = match Utc.timestamp_opt(op.start_timestamp, 0) {
                                LocalResult::Single(dt) => dt, // Успешно получили дату
                                _ => { // Если None или Ambiguous, используем текущее время как запасной вариант
                                     warn!("Could not get unique timestamp for op_id {}. Using current time.", op.id);
                                     Utc::now()
                                }
                            };
                            // --- КОНЕЦ ИСПРАВЛЕНИЯ ---
                            let date_str = timestamp_dt.format("%Y-%m-%d %H:%M").to_string();
                            // Используем target_futures_qty (нетто), так как он точнее для пользователя
                            let label = format!("ID:{} {:.6} {} ({})", op.id, op.target_futures_qty, op.base_symbol, date_str);
                            let callback_data = format!("unhedge_select_{}", op.id);
                            buttons.push(vec![InlineKeyboardButton::callback(label, callback_data)]);
                        }
                        buttons.push(vec![InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge")]); // Кнопка отмены

                        let kb = InlineKeyboardMarkup::new(buttons);
                        let bot_msg = bot.send_message(chat_id, format!("Найдено несколько завершенных операций хеджирования для {}. Выберите, какую расхеджировать:", symbol))
                            .reply_markup(kb)
                            .await?;

                        // Сохраняем состояние ожидания выбора
                        {
                            let mut state = state_storage.write().expect("Failed to lock state storage");
                            state.insert(chat_id, UserState::AwaitingUnhedgeSelection {
                                symbol: symbol.clone(),
                                operations, // Сохраняем исходный (не сортированный) вектор
                                last_bot_message_id: Some(bot_msg.id.0),
                            });
                            info!("User state for {} set to AwaitingUnhedgeSelection for symbol {}", chat_id, symbol);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to query hedge operations for {}: {}", symbol, e);
                    bot.send_message(chat_id, format!("❌ Ошибка при поиске операций хеджирования в БД: {}", e)).await?;
                }
            }
        }
        // --- КОНЕЦ ОБРАБОТКИ /unhedge ---

         Command::Funding(arg) => {
            let parts: Vec<_> = arg.split_whitespace().collect();
            if parts.is_empty() { bot.send_message(chat_id, "Использование: /funding <SYMBOL> [days]").await?; }
            else {
                let sym = parts[0].to_uppercase();
                let days_u32 = parts.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(30);
                if days_u32 == 0 { bot.send_message(chat_id, "⚠️ Количество дней должно быть больше нуля.").await?; return Ok(()); }
                let days_u16 = days_u32 as u16;
                info!("Fetching funding rate for {} ({} days) for chat_id: {}", sym, days_u16, chat_id);
                match exchange.get_funding_rate(&sym, days_u16).await {
                    Ok(rate) => { bot.send_message(chat_id, format!("📈 Средняя ставка финансирования {} за {} дн: {:.4}%", sym, days_u16, rate * 100.0)).await?; }
                    Err(e) => { error!("Failed to fetch funding rate for {} for chat_id: {}: {}", sym, chat_id, e); bot.send_message(chat_id, format!("❌ Не удалось получить ставку финансирования {}: {}", sym, e)).await?; }
                }
            }
        }
    }
    Ok(())
}