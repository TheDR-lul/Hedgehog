// src/notifier/market_info.rs

use super::{Command, StateStorage, UserState, callback_data, navigation};
use crate::config::Config;
use crate::exchange::Exchange;
use crate::storage::Db;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId,
};
use teloxide::utils::command::BotCommands;
use tracing::{info, warn, error};


// --- Обработчики Команд ---

/// Обработчик команды /status
pub async fn handle_status_command<E>(
    bot: Bot,
    msg: Message,
    exchange: Arc<E>, // Убрали mut
    _state_storage: StateStorage,
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id; // ИСПРАВЛЕНО: Назад на поле
    info!("Processing /status command for chat_id: {}", chat_id);
    let indicator_msg = bot.send_message(chat_id, "⏳ Проверка соединения с биржей...").await?;

    // --- Логика проверки соединения ---
    let mut exchange_clone = (*exchange).clone();
    let status_text = match exchange_clone.check_connection().await {
         Ok(_) => "✅ Бот запущен и успешно подключен к бирже.".to_string(),
         Err(e) => format!("⚠️ Бот запущен, но есть проблема с подключением к бирже: {}", e),
    };
    // --- Конец логики ---

    bot.edit_message_text(chat_id, indicator_msg.id, status_text).await?;

    // Удаляем исходное сообщение /status
    if let Err(e) = bot.delete_message(chat_id, msg.id).await { // ИСПРАВЛЕНО: Назад на поле
        warn!("Failed to delete /status command message: {}", e);
    }

    Ok(())
}

/// Обработчик команды /funding SYMBOL [days]
pub async fn handle_funding_command<E>(
    bot: Bot,
    msg: Message,
    args: String,
    exchange: Arc<E>,
    _state_storage: StateStorage,
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
     E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id; // ИСПРАВЛЕНО: Назад на поле
    let parts: Vec<&str> = args.split_whitespace().collect();

    if parts.is_empty() {
         // ИСПРАВЛЕНО: Убрали ошибочный get_command_description
         bot.send_message(chat_id, format!("Использование: /funding <SYMBOL> [days]\nПример: /funding BTC или /funding BTC 7")).await?;
         if let Err(e) = bot.delete_message(chat_id, msg.id).await { warn!("Failed to delete invalid /funding command message: {}", e); } // ИСПРАВЛЕНО: Назад на поле
         return Ok(());
    }

    let symbol = parts[0].to_uppercase();
    let days_u32 = parts.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(30);

    if days_u32 == 0 {
        bot.send_message(chat_id, "⚠️ Количество дней должно быть больше нуля.").await?;
         if let Err(e) = bot.delete_message(chat_id, msg.id).await { warn!("Failed to delete invalid /funding command message: {}", e); } // ИСПРАВЛЕНО: Назад на поле
        return Ok(());
    }
    let days_u16 = days_u32.min(66) as u16;
    // ИСПРАВЛЕНО: Приведение типов для сравнения
    if (days_u16 as u32) != days_u32 && parts.get(1).is_some() {
         bot.send_message(chat_id, format!("ℹ️ Количество дней для фандинга ограничено до {}.", days_u16)).await?;
    }

    info!("Processing /funding {} ({} days) command for chat_id: {}", symbol, days_u16, chat_id);
    let indicator_msg = bot.send_message(chat_id, format!("⏳ Загрузка ставки финансирования для {} ({} дн.)...", symbol, days_u16)).await?;

    match exchange.get_funding_rate(&symbol, days_u16).await {
        Ok(rate) => {
            let text = format!("📈 Средняя ставка финансирования {} за ~{} дн.: {:.4}%", symbol, days_u16, rate * 100.0);
            bot.edit_message_text(chat_id, indicator_msg.id, text).await?;
        }
        Err(e) => {
            error!("Failed to fetch funding rate for {} for chat_id: {}: {}", symbol, chat_id, e);
            let error_text = format!("❌ Не удалось получить ставку финансирования {}: {}", symbol, e);
            bot.edit_message_text(chat_id, indicator_msg.id, error_text).await?;
        }
    }

     // Удаляем исходное сообщение /funding
     if let Err(e) = bot.delete_message(chat_id, msg.id).await { // ИСПРАВЛЕНО: Назад на поле
         warn!("Failed to delete /funding command message: {}", e);
     }

    Ok(())
}


// --- Обработчики Колбэков ---

/// Создает клавиатуру для подменю "Информация"
fn make_info_menu_keyboard() -> InlineKeyboardMarkup {
     InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("ℹ️ Статус API", callback_data::SHOW_STATUS),
        ],
        vec![
             InlineKeyboardButton::callback("📈 Ставка Funding", callback_data::SHOW_FUNDING),
        ],
        vec![
            InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN),
        ],
    ])
}

/// Обработчик колбэка кнопки "Информация" из главного меню
pub async fn handle_menu_info_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    _exchange: Arc<E>,
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    // Для CallbackQuery используем message.chat.id и message.id
    if let Some(msg) = q.message {
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::MENU_INFO, chat_id);

        let text = "📊 Информация:";
        let kb = make_info_menu_keyboard();
        bot.edit_message_text(chat_id, msg.id(), text).reply_markup(kb).await?;

    } else {
        warn!("CallbackQuery missing message in handle_menu_info_callback");
    }
    bot.answer_callback_query(q.id).await?;
    Ok(())
}

/// Обработчик колбэка кнопки "Статус API"
pub async fn handle_show_status_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: Arc<E>, // Убрали mut
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
 where
     E: Exchange + Clone + Send + Sync + 'static,
 {
    if let Some(msg) = q.message {
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::SHOW_STATUS, chat_id);

        // Показываем индикатор
        let kb = make_info_menu_keyboard();
        bot.edit_message_text(chat_id, msg.id(), "⏳ Проверка соединения...")
           .reply_markup(kb.clone())
           .await?;

        // Логика проверки статуса
        let mut exchange_clone = (*exchange).clone();
        let status_text = match exchange_clone.check_connection().await {
            Ok(_) => "✅ Бот запущен и успешно подключен к бирже.".to_string(),
            Err(e) => format!("⚠️ Бот запущен, но есть проблема с подключением к бирже: {}", e),
        };

        // Редактируем с результатом
        bot.edit_message_text(chat_id, msg.id(), status_text)
           .reply_markup(kb)
           .await?;
    } else {
         warn!("CallbackQuery missing message in handle_show_status_callback");
    }
    bot.answer_callback_query(q.id).await?;
    Ok(())
}

/// Обработчик колбэка кнопки "Ставка Funding"
pub async fn handle_show_funding_callback(
    bot: Bot,
    q: CallbackQuery,
    state_storage: StateStorage,
) -> anyhow::Result<()> {
     if let Some(msg) = q.message {
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::SHOW_FUNDING, chat_id);

        let text = "Введите символ для получения ставки финансирования (например, BTC):";
         let kb = InlineKeyboardMarkup::new(vec![
             vec![InlineKeyboardButton::callback("⬅️ Назад", callback_data::MENU_INFO)],
             vec![InlineKeyboardButton::callback("❌ Отмена (в главное меню)", callback_data::CANCEL_DIALOG)],
         ]);

        bot.edit_message_text(chat_id, msg.id(), text).reply_markup(kb).await?;

        // Устанавливаем состояние ожидания ввода символа
        {
            let mut state_guard = state_storage.write().expect("Lock failed");
            state_guard.insert(chat_id, UserState::AwaitingFundingSymbolInput { last_bot_message_id: Some(msg.id().0) });
            info!("User state for {} set to AwaitingFundingSymbolInput", chat_id);
        }

     } else {
         warn!("CallbackQuery missing message in handle_show_funding_callback");
     }
     bot.answer_callback_query(q.id).await?;
     Ok(())
}


/// Обработчик текстового ввода символа для Funding
pub async fn handle_funding_symbol_input<E>(
    bot: Bot,
    msg: Message,
    exchange: Arc<E>,
    state_storage: StateStorage,
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id; // ИСПРАВЛЕНО: Назад на поле
    let user_message_id = msg.id; // ИСПРАВЛЕНО: Назад на поле
    let symbol = msg.text().unwrap_or("").trim().to_uppercase();

    // Получаем ID предыдущего сообщения бота из состояния
    let previous_bot_message_id = {
        let state_guard = state_storage.read().expect("Lock failed");
        match state_guard.get(&chat_id) {
            Some(UserState::AwaitingFundingSymbolInput { last_bot_message_id }) => *last_bot_message_id,
            _ => {
                 if let Err(e) = bot.delete_message(chat_id, user_message_id).await { warn!("Failed delete unexpected funding symbol input: {}", e); }
                 return Ok(()); // Не то состояние
            }
        }
    };

    // Удаляем сообщение пользователя
    if let Err(e) = bot.delete_message(chat_id, user_message_id).await { warn!("Failed delete user funding symbol message: {}", e); }

    if symbol.is_empty() {
         if let Some(bot_msg_id) = previous_bot_message_id {
             let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id), "⚠️ Введите непустой символ (например, BTC):").await;
         }
         return Ok(());
    }

     info!("User {} entered symbol '{}' for funding", chat_id, symbol);

    // Сбрасываем состояние ПЕРЕД запросом к API
    {
        state_storage.write().expect("Lock failed").insert(chat_id, UserState::None);
        info!("User state for {} reset to None", chat_id);
    }

    // Показываем индикатор ожидания
    let days_u16 = 30;
    let loading_text = format!("⏳ Загрузка ставки финансирования для {} ({} дн.)...", symbol, days_u16);
    let bot_msg_id_opt = previous_bot_message_id.map(MessageId);

    if let Some(bot_msg_id) = bot_msg_id_opt {
        let _ = bot.edit_message_text(chat_id, bot_msg_id, loading_text).await;
    } else {
         warn!("No previous bot message ID to edit for funding result {}", chat_id);
    }


    // Выполняем запрос и показываем результат
    match exchange.get_funding_rate(&symbol, days_u16).await {
        Ok(rate) => {
            let text = format!("📈 Средняя ставка финансирования {} за ~{} дн.: {:.4}%", symbol, days_u16, rate * 100.0);
            if let Some(bot_msg_id) = bot_msg_id_opt {
                 let kb = make_info_menu_keyboard();
                 let _ = bot.edit_message_text(chat_id, bot_msg_id, text).reply_markup(kb).await;
            } else {
                 let _ = bot.send_message(chat_id, text).await;
            }
        }
        Err(e) => {
            error!("Failed fetch funding rate for {} (from input): {}", symbol, e);
            let error_text = format!("❌ Не удалось получить ставку финансирования {}: {}", symbol, e);
             if let Some(bot_msg_id) = bot_msg_id_opt {
                 let kb = make_info_menu_keyboard();
                 let _ = bot.edit_message_text(chat_id, bot_msg_id, error_text).reply_markup(kb).await;
             } else {
                  let _ = bot.send_message(chat_id, error_text).await;
             }
        }
    }

    Ok(())
}