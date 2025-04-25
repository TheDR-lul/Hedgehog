// src/notifier/navigation.rs

// <<< ИСПРАВЛЕНО: Убран RunningOperations >>>
use super::{StateStorage, UserState, callback_data};
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
pub async fn show_main_menu(bot: &Bot, chat_id: ChatId, message_to_edit: Option<MessageId>)
    -> Result<(), teloxide::RequestError>
{
    let text = WELCOME_MESSAGE;
    let kb = make_main_menu_keyboard();

    if let Some(message_id) = message_to_edit {
        match bot.edit_message_text(chat_id, message_id, text).reply_markup(kb.clone()).await {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!("Failed to edit message {} to main menu: {}. Sending new one.", message_id, e);
                bot.send_message(chat_id, text).reply_markup(kb).await?;
                Ok(())
            }
        }
    } else {
        bot.send_message(chat_id, text).reply_markup(kb).await?;
        Ok(())
    }
}


// --- Обработчики ---

/// Обработчик команды /start
pub async fn handle_start<E>(
    bot: Bot,
    msg: Message,
    _exchange: Arc<E>,
    state_storage: StateStorage, // Тип StateStorage уже Arc<TokioRwLock<...>> из mod.rs
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    info!("Processing /start command for chat_id: {}", chat_id);

    {
        // <<< ИСПРАВЛЕНО: Используем .await >>>
        let mut state_guard = state_storage.write().await;
        state_guard.insert(chat_id, UserState::None);
        info!("User state for {} reset to None.", chat_id);
    } // Блокировка state_guard освобождается здесь

    if let Err(e) = bot.delete_message(chat_id, msg.id).await {
        warn!("Failed to delete /start command message: {}", e);
    }

    let _ = show_main_menu(&bot, chat_id, None).await;

    Ok(())
}

/// Обработчик колбэка кнопки "Назад" (возврат в главное меню)
pub async fn handle_back_to_main(
    bot: Bot,
    q: CallbackQuery,
    state_storage: StateStorage, // Тип StateStorage уже Arc<TokioRwLock<...>>
) -> anyhow::Result<()> {
    if let Some(msg) = q.message.as_ref() {
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::BACK_TO_MAIN, chat_id);

        {
            // <<< ИСПРАВЛЕНО: Используем .await >>>
            let mut state_guard = state_storage.write().await;
            state_guard.insert(chat_id, UserState::None);
            info!("User state for {} reset to None.", chat_id);
        } // Блокировка state_guard освобождается здесь

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
    chat_id: ChatId,
    message_id: MessageId,
    state_storage: StateStorage, // Тип StateStorage уже Arc<TokioRwLock<...>>
) -> anyhow::Result<()> {
    info!("Processing cancel dialog for chat_id: {}", chat_id);

    {
        // <<< ИСПРАВЛЕНО: Используем .await >>>
        let mut state_guard = state_storage.write().await;
        state_guard.insert(chat_id, UserState::None);
         info!("User state for {} reset to None.", chat_id);
    } // Блокировка state_guard освобождается здесь

    let _ = show_main_menu(&bot, chat_id, Some(message_id)).await;

    // Ответ на колбэк выполняется в вызывающей функции
    Ok(())
}


// --- Заглушки для обработчиков кнопок главного меню ---
// (Заглушки не используют state_storage, поэтому не требуют изменений)

pub async fn handle_menu_wallet_callback<E>(
    bot: Bot, q: CallbackQuery, _exchange: Arc<E>, _cfg: Arc<Config>, _db: Arc<Db>
) -> anyhow::Result<()> where E: Exchange + Clone + Send + Sync + 'static {
    info!("Callback '{}' triggered. Calling wallet_info handler...", callback_data::MENU_WALLET);
    bot.answer_callback_query(q.id).text("Раздел Кошелек (не реализовано)").await?;
    Ok(())
}

pub async fn handle_menu_info_callback<E>(
    bot: Bot, q: CallbackQuery, _exchange: Arc<E>, _cfg: Arc<Config>, _db: Arc<Db>
) -> anyhow::Result<()> where E: Exchange + Clone + Send + Sync + 'static {
      info!("Callback '{}' triggered. Calling market_info handler...", callback_data::MENU_INFO);
      bot.answer_callback_query(q.id).text("Раздел Информация (не реализовано)").await?;
    Ok(())
}

// <<< ИСПРАВЛЕНО: Убран неиспользуемый RunningOperations из импорта и сигнатуры >>>
use super::RunningOperations; // Убран из импорта super
pub async fn handle_menu_active_ops_callback(
    bot: Bot, q: CallbackQuery, _running_operations: RunningOperations, _state_storage: StateStorage
) -> anyhow::Result<()> {
    info!("Callback '{}' triggered. Calling active_ops handler...", callback_data::MENU_ACTIVE_OPS);
    bot.answer_callback_query(q.id).text("Раздел Активные операции (не реализовано)").await?;
    Ok(())
}