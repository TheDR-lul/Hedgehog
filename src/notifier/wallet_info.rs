// src/notifier/wallet_info.rs

// <<< ИСПРАВЛЕНО: Убраны UserState, RunningOperations, Balance, MessageId, ChatId >>>
// Command и callback_data используются (косвенно через Command::descriptions и в handle_menu_wallet_callback)
use crate::notifier::{callback_data, StateStorage}; // Оставляем StateStorage, т.к. он в сигнатурах
use crate::config::Config;
use crate::exchange::Exchange; // Оставляем Exchange
use crate::storage::Db;
use crate::hedger::ORDER_FILL_TOLERANCE; // Используется в get_formatted_balances
use std::sync::Arc;
use teloxide::prelude::*; // Оставляем для Bot, Requester и т.д.
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, CallbackQuery, // Убраны MessageId, ChatId
};
 // Используется для Command::descriptions
use tracing::{info, warn, error}; // Используются

// --- Вспомогательная функция ---

/// Получает балансы и форматирует их для вывода + возвращает данные
pub async fn get_formatted_balances<E: Exchange>(
    exchange: &E,
    quote_currency: &str,
    include_approx_value: bool,
) -> Result<(String, Vec<(String, f64, f64)>), anyhow::Error> {
    info!("Fetching all balances from exchange...");
    let balances = exchange.get_all_balances().await?;
    info!("Received {} balance entries.", balances.len());

    let mut text = "💼 Баланс кошелька:\n".to_string();
    let mut found_assets = false;
    let mut sorted_balances: Vec<_> = balances.into_iter().collect();
    sorted_balances.sort_by_key(|(coin, _)| coin.clone());

    let mut asset_data = Vec::new();

    let mut prices: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    if include_approx_value {
        info!("Fetching prices for value approximation...");
        for (coin, _) in &sorted_balances {
             if coin != quote_currency {
                 match exchange.get_spot_price(coin).await {
                     Ok(price) => { prices.insert(coin.clone(), price); },
                     Err(e) => warn!("Could not fetch price for {}: {}", coin, e),
                 }
             }
        }
        info!("Fetched {} prices.", prices.len());
    }

    for (coin, balance) in sorted_balances {
        if balance.free > ORDER_FILL_TOLERANCE || balance.locked > ORDER_FILL_TOLERANCE || coin == quote_currency {
            let mut line = format!(
                "• {}: ️free {:.8}, locked {:.8}",
                coin, balance.free, balance.locked
            );

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
            asset_data.push((coin.clone(), balance.free, balance.locked));
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
    _state_storage: StateStorage, // Аргумент добавлен, но пока не используется
    cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    info!("Processing /wallet command for chat_id: {}", chat_id);
    let indicator_msg = bot.send_message(chat_id, "⏳ Загрузка баланса...").await?;

    match get_formatted_balances(exchange.as_ref(), &cfg.quote_currency, true).await {
        Ok((text, _)) => {
            bot.edit_message_text(chat_id, indicator_msg.id, text).await?;
        }
        Err(e) => {
            error!("Failed to fetch wallet balance for chat_id: {}: {}", chat_id, e);
            let error_text = format!("❌ Не удалось получить баланс кошелька: {}", e);
             bot.edit_message_text(chat_id, indicator_msg.id, error_text).await?;
        }
    }

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
    _state_storage: StateStorage, // Аргумент добавлен, но пока не используется
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
     E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let symbol = symbol_arg.trim().to_uppercase();

    if symbol.is_empty() {
        // <<< ИСПРАВЛЕНО: Упрощенная строка использования >>>
        bot.send_message(chat_id, "Использование: /balance <SYMBOL>\nПример: /balance BTC").await?;
        // ---
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
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    // <<< ИСПРАВЛЕНО: Используем as_ref() и методы .chat()/.id() >>>
    if let Some(msg) = q.message.as_ref() { // Используем as_ref() для заимствования
        let chat_id = msg.chat().id; // Вызов метода
        info!("Processing '{}' callback for chat_id: {}", callback_data::MENU_WALLET, chat_id);

        let indicator_text = "⏳ Загрузка баланса...";
        let kb = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
        ]]);
        // Используем msg.id() - вызов метода
        bot.edit_message_text(chat_id, msg.id(), indicator_text)
           .reply_markup(kb.clone())
           .await?;

        match get_formatted_balances(exchange.as_ref(), &cfg.quote_currency, true).await {
            Ok((text, _)) => {
                 // Используем msg.id() - вызов метода
                bot.edit_message_text(chat_id, msg.id(), text)
                   .reply_markup(kb)
                   .await?;
            }
            Err(e) => {
                error!("Failed to fetch wallet balance via callback for chat_id: {}: {}", chat_id, e);
                let error_text = format!("❌ Не удалось получить баланс кошелька: {}", e);
                 // Используем msg.id() - вызов метода
                bot.edit_message_text(chat_id, msg.id(), error_text)
                   .reply_markup(kb)
                   .await?;
            }
        }
    } else {
        warn!("CallbackQuery missing message in handle_menu_wallet_callback");
    }
     bot.answer_callback_query(q.id).await?;
    Ok(())
}

// Конец оригинального кода. Дубликат удален.