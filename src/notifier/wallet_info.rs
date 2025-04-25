// src/notifier/wallet_info.rs

use super::{Command, StateStorage, UserState, callback_data, RunningOperations}; // Импортируем из родительского mod.rs
use crate::config::Config;
use crate::exchange::{Exchange, Balance}; // Добавлен Balance
use crate::storage::Db;
use crate::hedger::ORDER_FILL_TOLERANCE; // Для форматирования
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId, // Добавлены типы
};
use teloxide::utils::command::BotCommands; // Для Command::descriptions
use tracing::{info, warn, error};

// --- Вспомогательная функция (перенесена из callbacks.rs) ---

/// Получает балансы и форматирует их для вывода + возвращает данные
pub async fn get_formatted_balances<E: Exchange>(
    exchange: &E,
    quote_currency: &str,
    include_approx_value: bool, // Флаг для добавления стоимости
) -> Result<(String, Vec<(String, f64, f64)>), anyhow::Error> {
    info!("Fetching all balances from exchange...");
    let balances = exchange.get_all_balances().await?;
    info!("Received {} balance entries.", balances.len());

    let mut text = "💼 Баланс кошелька:\n".to_string();
    let mut found_assets = false;
    let mut sorted_balances: Vec<_> = balances.into_iter().collect();
    sorted_balances.sort_by_key(|(coin, _)| coin.clone());

    let mut asset_data = Vec::new(); // Для возврата данных об активах

    // Если нужно показывать стоимость, получаем цены
    let mut prices: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    if include_approx_value {
        info!("Fetching prices for value approximation...");
        for (coin, _) in &sorted_balances {
             if coin != quote_currency {
                match exchange.get_spot_price(coin).await {
                    Ok(price) => { prices.insert(coin.clone(), price); },
                    Err(e) => warn!("Could not fetch price for {}: {}", coin, e), // Не блокируем, если цена недоступна
                }
             }
        }
        info!("Fetched {} prices.", prices.len());
    }

    for (coin, balance) in sorted_balances {
        // Показываем актив, если есть свободные или залоченные средства, или это quote_currency
        if balance.free > ORDER_FILL_TOLERANCE || balance.locked > ORDER_FILL_TOLERANCE || coin == quote_currency {
            let mut line = format!(
                "• {}: ️free {:.8}, locked {:.8}",
                coin, balance.free, balance.locked
            );

            // Добавляем примерную стоимость, если нужно и цена есть
            if include_approx_value && coin != quote_currency {
                 if let Some(price) = prices.get(&coin) {
                     let total_qty = balance.free + balance.locked;
                     if total_qty > ORDER_FILL_TOLERANCE && *price > 0.0 {
                         let value = total_qty * price;
                         line.push_str(&format!(" (≈ {:.2} {})", value, quote_currency));
                     }
                 }
            }
            line.push('\n');
            text.push_str(&line);
            asset_data.push((coin.clone(), balance.free, balance.locked)); // Сохраняем данные
            found_assets = true;
        }
    }

    if !found_assets {
        text = "ℹ️ Ваш кошелек пуст.".to_string();
    }
    Ok((text, asset_data))
}


// --- Обработчики ---

/// Обработчик команды /wallet
pub async fn handle_wallet_command<E>(
    bot: Bot,
    msg: Message,
    exchange: Arc<E>,
    _state_storage: StateStorage, // Пока не используется
    cfg: Arc<Config>,
    _db: Arc<Db>, // Пока не используется
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    info!("Processing /wallet command for chat_id: {}", chat_id);
    let indicator_msg = bot.send_message(chat_id, "⏳ Загрузка баланса...").await?; // Индикатор ожидания

    match get_formatted_balances(exchange.as_ref(), &cfg.quote_currency, true).await { // Включаем расчет стоимости
        Ok((text, _)) => {
            // Редактируем сообщение с индикатором, показывая результат
            bot.edit_message_text(chat_id, indicator_msg.id, text).await?;
        }
        Err(e) => {
            error!("Failed to fetch wallet balance for chat_id: {}: {}", chat_id, e);
            let error_text = format!("❌ Не удалось получить баланс кошелька: {}", e);
             bot.edit_message_text(chat_id, indicator_msg.id, error_text).await?;
        }
    }

     // Удаляем исходное сообщение /wallet
     if let Err(e) = bot.delete_message(chat_id, msg.id).await {
         warn!("Failed to delete /wallet command message: {}", e);
     }

    Ok(())
}

/// Обработчик команды /balance SYMBOL
pub async fn handle_balance_command<E>(
    bot: Bot,
    msg: Message,
    symbol_arg: String,
    exchange: Arc<E>,
    _state_storage: StateStorage, // Пока не используется
    _cfg: Arc<Config>, // Пока не используется
    _db: Arc<Db>, // Пока не используется
) -> anyhow::Result<()>
where
     E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let symbol = symbol_arg.trim().to_uppercase();

    if symbol.is_empty() {
        bot.send_message(chat_id, format!("Использование: {}\nПример: /balance BTC", Command::descriptions().get_command_description("balance").unwrap_or("/balance <SYMBOL>"))).await?;
        // Удаляем исходное сообщение с неверной командой
         if let Err(e) = bot.delete_message(chat_id, msg.id).await {
             warn!("Failed to delete invalid /balance command message: {}", e);
         }
        return Ok(());
    }

     info!("Processing /balance {} command for chat_id: {}", symbol, chat_id);
     let indicator_msg = bot.send_message(chat_id, format!("⏳ Загрузка баланса для {}...", symbol)).await?;

    match exchange.get_balance(&symbol).await {
        Ok(balance) => {
            let text = format!("💰 {}: free {:.8}, locked {:.8}", symbol, balance.free, balance.locked);
            bot.edit_message_text(chat_id, indicator_msg.id, text).await?;
        }
        Err(e) => {
            error!("Failed to fetch balance for {} for chat_id: {}: {}", symbol, chat_id, e);
            let error_text = format!("❌ Не удалось получить баланс {}: {}", symbol, e);
             bot.edit_message_text(chat_id, indicator_msg.id, error_text).await?;
        }
    }

     // Удаляем исходное сообщение /balance
     if let Err(e) = bot.delete_message(chat_id, msg.id).await {
         warn!("Failed to delete /balance command message: {}", e);
     }

    Ok(())
}


/// Обработчик колбэка кнопки "Кошелек" из главного меню
pub async fn handle_menu_wallet_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: Arc<E>,
    cfg: Arc<Config>,
    _db: Arc<Db>, // Пока не используется
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let Some(msg) = q.message {
        let chat_id = msg.chat.id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::MENU_WALLET, chat_id);

        // Показываем индикатор ожидания, редактируя существующее сообщение
        let indicator_text = "⏳ Загрузка баланса...";
        // Добавляем кнопку "Назад" сразу
        let kb = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
        ]]);
        bot.edit_message_text(chat_id, msg.id, indicator_text)
           .reply_markup(kb.clone()) // Используем клон для второго вызова
           .await?; // Здесь ошибка редактирования критична, т.к. след. шаг тоже редактирует

        match get_formatted_balances(exchange.as_ref(), &cfg.quote_currency, true).await { // Включаем расчет стоимости
            Ok((text, _)) => {
                // Редактируем сообщение еще раз, показывая результат и кнопку "Назад"
                bot.edit_message_text(chat_id, msg.id, text)
                   .reply_markup(kb) // Клавиатура та же
                   .await?;
            }
            Err(e) => {
                error!("Failed to fetch wallet balance via callback for chat_id: {}: {}", chat_id, e);
                let error_text = format!("❌ Не удалось получить баланс кошелька: {}", e);
                // Показываем ошибку и кнопку "Назад"
                bot.edit_message_text(chat_id, msg.id, error_text)
                   .reply_markup(kb) // Клавиатура та же
                   .await?;
            }
        }
    } else {
        warn!("CallbackQuery missing message in handle_menu_wallet_callback");
    }
     // Отвечаем на колбэк, чтобы убрать часики
     bot.answer_callback_query(q.id).await?;
    Ok(())
}

// TODO: Добавить обработчик для колбэка "баланс монеты" (если решите делать его через кнопки,
// а не только через команду /balance). Он может запрашивать ввод тикера.// src/notifier/wallet_info.rs

use super::{Command, StateStorage, UserState, callback_data, RunningOperations}; // Импортируем из родительского mod.rs
use crate::config::Config;
use crate::exchange::{Exchange, Balance}; // Добавлен Balance
use crate::storage::Db;
use crate::hedger::ORDER_FILL_TOLERANCE; // Для форматирования
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId, // Добавлены типы
};
use teloxide::utils::command::BotCommands; // Для Command::descriptions
use tracing::{info, warn, error};

// --- Вспомогательная функция (перенесена из callbacks.rs) ---

/// Получает балансы и форматирует их для вывода + возвращает данные
pub async fn get_formatted_balances<E: Exchange>(
    exchange: &E,
    quote_currency: &str,
    include_approx_value: bool, // Флаг для добавления стоимости
) -> Result<(String, Vec<(String, f64, f64)>), anyhow::Error> {
    info!("Fetching all balances from exchange...");
    let balances = exchange.get_all_balances().await?;
    info!("Received {} balance entries.", balances.len());

    let mut text = "💼 Баланс кошелька:\n".to_string();
    let mut found_assets = false;
    let mut sorted_balances: Vec<_> = balances.into_iter().collect();
    sorted_balances.sort_by_key(|(coin, _)| coin.clone());

    let mut asset_data = Vec::new(); // Для возврата данных об активах

    // Если нужно показывать стоимость, получаем цены
    let mut prices: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    if include_approx_value {
        info!("Fetching prices for value approximation...");
        for (coin, _) in &sorted_balances {
             if coin != quote_currency {
                match exchange.get_spot_price(coin).await {
                    Ok(price) => { prices.insert(coin.clone(), price); },
                    Err(e) => warn!("Could not fetch price for {}: {}", coin, e), // Не блокируем, если цена недоступна
                }
             }
        }
        info!("Fetched {} prices.", prices.len());
    }

    for (coin, balance) in sorted_balances {
        // Показываем актив, если есть свободные или залоченные средства, или это quote_currency
        if balance.free > ORDER_FILL_TOLERANCE || balance.locked > ORDER_FILL_TOLERANCE || coin == quote_currency {
            let mut line = format!(
                "• {}: ️free {:.8}, locked {:.8}",
                coin, balance.free, balance.locked
            );

            // Добавляем примерную стоимость, если нужно и цена есть
            if include_approx_value && coin != quote_currency {
                 if let Some(price) = prices.get(&coin) {
                     let total_qty = balance.free + balance.locked;
                     if total_qty > ORDER_FILL_TOLERANCE && *price > 0.0 {
                         let value = total_qty * price;
                         line.push_str(&format!(" (≈ {:.2} {})", value, quote_currency));
                     }
                 }
            }
            line.push('\n');
            text.push_str(&line);
            asset_data.push((coin.clone(), balance.free, balance.locked)); // Сохраняем данные
            found_assets = true;
        }
    }

    if !found_assets {
        text = "ℹ️ Ваш кошелек пуст.".to_string();
    }
    Ok((text, asset_data))
}


// --- Обработчики ---

/// Обработчик команды /wallet
pub async fn handle_wallet_command<E>(
    bot: Bot,
    msg: Message,
    exchange: Arc<E>,
    _state_storage: StateStorage, // Пока не используется
    cfg: Arc<Config>,
    _db: Arc<Db>, // Пока не используется
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    info!("Processing /wallet command for chat_id: {}", chat_id);
    let indicator_msg = bot.send_message(chat_id, "⏳ Загрузка баланса...").await?; // Индикатор ожидания

    match get_formatted_balances(exchange.as_ref(), &cfg.quote_currency, true).await { // Включаем расчет стоимости
        Ok((text, _)) => {
            // Редактируем сообщение с индикатором, показывая результат
            bot.edit_message_text(chat_id, indicator_msg.id, text).await?;
        }
        Err(e) => {
            error!("Failed to fetch wallet balance for chat_id: {}: {}", chat_id, e);
            let error_text = format!("❌ Не удалось получить баланс кошелька: {}", e);
             bot.edit_message_text(chat_id, indicator_msg.id, error_text).await?;
        }
    }

     // Удаляем исходное сообщение /wallet
     if let Err(e) = bot.delete_message(chat_id, msg.id).await {
         warn!("Failed to delete /wallet command message: {}", e);
     }

    Ok(())
}

/// Обработчик команды /balance SYMBOL
pub async fn handle_balance_command<E>(
    bot: Bot,
    msg: Message,
    symbol_arg: String,
    exchange: Arc<E>,
    _state_storage: StateStorage, // Пока не используется
    _cfg: Arc<Config>, // Пока не используется
    _db: Arc<Db>, // Пока не используется
) -> anyhow::Result<()>
where
     E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let symbol = symbol_arg.trim().to_uppercase();

    if symbol.is_empty() {
        bot.send_message(chat_id, format!("Использование: {}\nПример: /balance BTC", Command::descriptions().get_command_description("balance").unwrap_or("/balance <SYMBOL>"))).await?;
        // Удаляем исходное сообщение с неверной командой
         if let Err(e) = bot.delete_message(chat_id, msg.id).await {
             warn!("Failed to delete invalid /balance command message: {}", e);
         }
        return Ok(());
    }

     info!("Processing /balance {} command for chat_id: {}", symbol, chat_id);
     let indicator_msg = bot.send_message(chat_id, format!("⏳ Загрузка баланса для {}...", symbol)).await?;

    match exchange.get_balance(&symbol).await {
        Ok(balance) => {
            let text = format!("💰 {}: free {:.8}, locked {:.8}", symbol, balance.free, balance.locked);
            bot.edit_message_text(chat_id, indicator_msg.id, text).await?;
        }
        Err(e) => {
            error!("Failed to fetch balance for {} for chat_id: {}: {}", symbol, chat_id, e);
            let error_text = format!("❌ Не удалось получить баланс {}: {}", symbol, e);
             bot.edit_message_text(chat_id, indicator_msg.id, error_text).await?;
        }
    }

     // Удаляем исходное сообщение /balance
     if let Err(e) = bot.delete_message(chat_id, msg.id).await {
         warn!("Failed to delete /balance command message: {}", e);
     }

    Ok(())
}


/// Обработчик колбэка кнопки "Кошелек" из главного меню
pub async fn handle_menu_wallet_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: Arc<E>,
    cfg: Arc<Config>,
    _db: Arc<Db>, // Пока не используется
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let Some(msg) = q.message {
        let chat_id = msg.chat.id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::MENU_WALLET, chat_id);

        // Показываем индикатор ожидания, редактируя существующее сообщение
        let indicator_text = "⏳ Загрузка баланса...";
        // Добавляем кнопку "Назад" сразу
        let kb = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
        ]]);
        bot.edit_message_text(chat_id, msg.id, indicator_text)
           .reply_markup(kb.clone()) // Используем клон для второго вызова
           .await?; // Здесь ошибка редактирования критична, т.к. след. шаг тоже редактирует

        match get_formatted_balances(exchange.as_ref(), &cfg.quote_currency, true).await { // Включаем расчет стоимости
            Ok((text, _)) => {
                // Редактируем сообщение еще раз, показывая результат и кнопку "Назад"
                bot.edit_message_text(chat_id, msg.id, text)
                   .reply_markup(kb) // Клавиатура та же
                   .await?;
            }
            Err(e) => {
                error!("Failed to fetch wallet balance via callback for chat_id: {}: {}", chat_id, e);
                let error_text = format!("❌ Не удалось получить баланс кошелька: {}", e);
                // Показываем ошибку и кнопку "Назад"
                bot.edit_message_text(chat_id, msg.id, error_text)
                   .reply_markup(kb) // Клавиатура та же
                   .await?;
            }
        }
    } else {
        warn!("CallbackQuery missing message in handle_menu_wallet_callback");
    }
     // Отвечаем на колбэк, чтобы убрать часики
     bot.answer_callback_query(q.id).await?;
    Ok(())
}

// TODO: Добавить обработчик для колбэка "баланс монеты" (если решите делать его через кнопки,
// а не только через команду /balance). Он может запрашивать ввод тикера.