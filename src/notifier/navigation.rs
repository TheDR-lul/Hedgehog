// src/notifier/navigation.rs

// ИСПРАВЛЕНО: Убраны неиспользуемые Command, BotCommands
use super::{StateStorage, UserState, callback_data, RunningOperations};
use crate::config::Config;
use crate::exchange::Exchange;
use crate::storage::Db;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId,
};
use teloxide::requests::Requester;
use tracing::{info, warn};

// --- Константы ---
const WELCOME_MESSAGE: &str = "Добро пожаловать в Hedgehog Bot! Выберите действие:";

// --- Вспомогательные функции ---

/// Создает клавиатуру главного меню
pub fn make_main_menu_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("💼 Кошелек", callback_data::MENU_WALLET),
        ],
        vec![
            InlineKeyboardButton::callback("⚙️ Захеджировать", callback_data::START_HEDGE),
            InlineKeyboardButton::callback("🛠 Расхеджировать", callback_data::START_UNHEDGE),
        ],
        vec![
             InlineKeyboardButton::callback("📊 Информация", callback_data::MENU_INFO),
             InlineKeyboardButton::callback("⚡ Активные операции", callback_data::MENU_ACTIVE_OPS),
        ],
    ])
}

/// Показывает или редактирует сообщение с главным меню
// ИСПРАВЛЕНО: Тип возвращаемого значения и логика возврата
pub async fn show_main_menu(bot: &Bot, chat_id: ChatId, message_to_edit: Option<MessageId>)
    -> Result<(), teloxide::RequestError>
{
    let text = WELCOME_MESSAGE;
    let kb = make_main_menu_keyboard(); // Создаем клавиатуру один раз

    if let Some(message_id) = message_to_edit {
        match bot.edit_message_text(chat_id, message_id, text).reply_markup(kb.clone()).await { // Клонируем kb здесь
            Ok(_) => Ok(()),
            Err(e) => {
                warn!("Failed to edit message {} to main menu: {}. Sending new one.", message_id, e);
                 // Отправляем новое, если редактирование не удалось
                bot.send_message(chat_id, text).reply_markup(kb).await?; // Используем оригинал kb здесь
                Ok(())
            }
        }
    } else {
        // Отправляем новое сообщение
        bot.send_message(chat_id, text).reply_markup(kb).await?; // Используем оригинал kb здесь
        Ok(())
    }
}


// --- Обработчики ---

/// Обработчик команды /start
pub async fn handle_start<E>(
    bot: Bot,
    msg: Message,
    _exchange: Arc<E>,
    state_storage: StateStorage,
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id; // Используем поле
    info!("Processing /start command for chat_id: {}", chat_id);

    {
        let mut state_guard = state_storage.write().expect("Failed to lock state storage for write");
        state_guard.insert(chat_id, UserState::None);
        info!("User state for {} reset to None.", chat_id);
    }

    if let Err(e) = bot.delete_message(chat_id, msg.id).await { // Используем поле
        warn!("Failed to delete /start command message: {}", e);
    }

    let _ = show_main_menu(&bot, chat_id, None).await;

    Ok(())
}

/// Обработчик колбэка кнопки "Назад" (возврат в главное меню)
pub async fn handle_back_to_main(
    bot: Bot,
    q: CallbackQuery,
    state_storage: StateStorage,
) -> anyhow::Result<()> {
    if let Some(msg) = q.message {
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::BACK_TO_MAIN, chat_id);

        {
            let mut state_guard = state_storage.write().expect("Failed to lock state storage for write");
            state_guard.insert(chat_id, UserState::None);
            info!("User state for {} reset to None.", chat_id);
        }

        // ИСПРАВЛЕНО: Используем поле id
        let _ = show_main_menu(&bot, chat_id, Some(msg.id())).await;
    } else {
        warn!("CallbackQuery missing message in handle_back_to_main");
    }
    bot.answer_callback_query(q.id).await?;
    Ok(())
}

/// Обработчик колбэка кнопки "Отмена" в диалоге
pub async fn handle_cancel_dialog(
    bot: Bot,
    q: CallbackQuery,
    state_storage: StateStorage,
) -> anyhow::Result<()> {
     if let Some(msg) = q.message {
        // ИСПРАВЛЕНО: Используем поля chat.id и id для сообщения из CallbackQuery
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::CANCEL_DIALOG, chat_id);

        {
            let mut state_guard = state_storage.write().expect("Failed to lock state storage for write");
            state_guard.insert(chat_id, UserState::None);
             info!("User state for {} reset to None.", chat_id);
        }

        let text = "Действие отменено.";
        let kb = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("⬅️ Назад в главное меню", callback_data::BACK_TO_MAIN)
        ]]);
        // ИСПРАВЛЕНО: Используем поле id
        let _ = bot.edit_message_text(chat_id, msg.id(), text).reply_markup(kb).await;

     } else {
         warn!("CallbackQuery missing message in handle_cancel_dialog");
     }
     bot.answer_callback_query(q.id).await?;
     Ok(())
}

// --- Заглушки для обработчиков кнопок главного меню ---
// ИСПРАВЛЕНО: Добавлены префиксы '_' к неиспользуемым аргументам

pub async fn handle_menu_wallet_callback<E>(
    bot: Bot, q: CallbackQuery, _exchange: Arc<E>, _cfg: Arc<Config>, _db: Arc<Db>
) -> anyhow::Result<()> where E: Exchange + Clone + Send + Sync + 'static {
    info!("Callback '{}' triggered. Calling wallet_info handler...", callback_data::MENU_WALLET);
    // TODO: Вызвать функцию из wallet_info.rs для показа кошелька
    // wallet_info::show_wallet_summary(bot, q, _exchange, _cfg, _db).await?;
    bot.answer_callback_query(q.id).text("Раздел Кошелек (не реализовано)").await?;
    Ok(())
}

pub async fn handle_menu_info_callback<E>(
    bot: Bot, q: CallbackQuery, _exchange: Arc<E>, _cfg: Arc<Config>, _db: Arc<Db>
) -> anyhow::Result<()> where E: Exchange + Clone + Send + Sync + 'static {
     info!("Callback '{}' triggered. Calling market_info handler...", callback_data::MENU_INFO);
     // TODO: Вызвать функцию из market_info.rs для показа меню информации
     // market_info::show_info_menu(bot, q).await?;
     bot.answer_callback_query(q.id).text("Раздел Информация (не реализовано)").await?;
    Ok(())
}

pub async fn handle_menu_active_ops_callback(
    bot: Bot, q: CallbackQuery, _running_operations: RunningOperations, _state_storage: StateStorage
) -> anyhow::Result<()> {
    info!("Callback '{}' triggered. Calling active_ops handler...", callback_data::MENU_ACTIVE_OPS);
    // TODO: Вызвать функцию из active_ops.rs для показа активных операций
    // active_ops::show_active_operations(bot, q, _running_operations, _state_storage).await?;
    bot.answer_callback_query(q.id).text("Раздел Активные операции (не реализовано)").await?;
    Ok(())
}