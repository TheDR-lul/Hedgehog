// src/notifier/navigation.rs

use super::{Command, StateStorage, UserState, callback_data, RunningOperations}; // Добавили RunningOperations
use crate::config::Config;
use crate::exchange::Exchange;
use crate::storage::Db;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId, CallbackQuery, ChatId,
};
use teloxide::requests::Requester; // Добавили Requester
use teloxide::utils::command::BotCommands; // Добавили BotCommands
use tracing::{info, warn};

// --- Константы ---
const WELCOME_MESSAGE: &str = "Добро пожаловать в Hedgehog Bot! Выберите действие:";

// --- Вспомогательные функции ---

/// Создает клавиатуру главного меню
pub fn make_main_menu_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        // Ряд 1: Кошелек
        vec![
            InlineKeyboardButton::callback("💼 Кошелек", callback_data::MENU_WALLET),
        ],
        // Ряд 2: Управление
        vec![
            InlineKeyboardButton::callback("⚙️ Захеджировать", callback_data::START_HEDGE),
            InlineKeyboardButton::callback("🛠 Расхеджировать", callback_data::START_UNHEDGE),
        ],
        // Ряд 3: Инфо и Активные
        vec![
             InlineKeyboardButton::callback("📊 Информация", callback_data::MENU_INFO),
             InlineKeyboardButton::callback("⚡ Активные операции", callback_data::MENU_ACTIVE_OPS),
        ],
    ])
}

/// Показывает или редактирует сообщение с главным меню
/// Возвращает Ok(Message) если отправил новое, Ok(true) если отредактировал, Err при ошибке
pub async fn show_main_menu(bot: &Bot, chat_id: ChatId, message_to_edit: Option<MessageId>)
    -> Result<Result<Message, bool>, teloxide::RequestError>
{
    let text = WELCOME_MESSAGE;
    let kb = make_main_menu_keyboard();

    if let Some(message_id) = message_to_edit {
        match bot.edit_message_text(chat_id, message_id, text).reply_markup(kb).await {
            Ok(_) => Ok(Ok(true)), // Успешно отредактировано
            Err(e) => {
                warn!("Failed to edit message {} to main menu: {}. Sending new one.", message_id, e);
                // Если редактирование не удалось, пробуем отправить новое
                bot.send_message(chat_id, text).reply_markup(kb).await.map(Err) // Отправлено новое
            }
        }
    } else {
        // Отправляем новое сообщение
        bot.send_message(chat_id, text).reply_markup(kb).await.map(Err) // Отправлено новое
    }
}


// --- Обработчики ---

/// Обработчик команды /start
pub async fn handle_start<E>(
    bot: Bot,
    msg: Message,
    // Добавляем зависимости, которые могут понадобиться
    _exchange: Arc<E>, // Пока не используется
    state_storage: StateStorage,
    _cfg: Arc<Config>,
    _db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    info!("Processing /start command for chat_id: {}", chat_id);

    // Сбрасываем состояние пользователя при старте
    {
        let mut state_guard = state_storage.write().expect("Failed to lock state storage for write");
        // Перед сбросом проверяем, есть ли активные операции, о которых стоит помнить
        // (хотя обычно /start означает начало новой сессии)
        state_guard.insert(chat_id, UserState::None);
        info!("User state for {} reset to None.", chat_id);
    }

    // Пытаемся удалить старые сообщения бота, если они были сохранены (опционально)
    // cleanup_previous_bot_messages(&bot, chat_id, &state_storage).await; // Требует доп. логики

    // Удаляем сообщение с командой /start
    if let Err(e) = bot.delete_message(chat_id, msg.id).await {
        // Не критичная ошибка, просто логируем
        warn!("Failed to delete /start command message: {}", e);
    }

    // Показываем главное меню (отправляем новое сообщение)
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

        // Сбрасываем состояние пользователя
        {
            let mut state_guard = state_storage.write().expect("Failed to lock state storage for write");
            state_guard.insert(chat_id, UserState::None);
            info!("User state for {} reset to None.", chat_id);
        }

        // Редактируем сообщение, показывая главное меню
        let _ = show_main_menu(&bot, chat_id, Some(msg.id)).await;
    } else {
        warn!("CallbackQuery missing message in handle_back_to_main");
    }
    // Отвечаем на колбэк, чтобы убрать часики
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
        let chat_id = msg.chat.id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::CANCEL_DIALOG, chat_id);

        // Сбрасываем состояние пользователя
        {
            let mut state_guard = state_storage.write().expect("Failed to lock state storage for write");
            state_guard.insert(chat_id, UserState::None);
             info!("User state for {} reset to None.", chat_id);
        }

        // Редактируем сообщение на "Действие отменено."
        let text = "Действие отменено.";
        // Добавляем кнопку "Назад в главное меню"
        let kb = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("⬅️ Назад в главное меню", callback_data::BACK_TO_MAIN)
        ]]);
        // Игнорируем ошибку редактирования (сообщение могло быть удалено)
        let _ = bot.edit_message_text(chat_id, msg.id, text).reply_markup(kb).await;

     } else {
         warn!("CallbackQuery missing message in handle_cancel_dialog");
     }
     // Отвечаем на колбэк
     bot.answer_callback_query(q.id).await?;
     Ok(())
}

// Заглушки для обработчиков кнопок главного меню (будут вызывать функции из других модулей)

pub async fn handle_menu_wallet_callback<E>(
    bot: Bot, q: CallbackQuery, exchange: Arc<E>, cfg: Arc<Config>, db: Arc<Db>
) -> anyhow::Result<()> where E: Exchange + Clone + Send + Sync + 'static {
    info!("Callback '{}' triggered. Calling wallet_info handler...", callback_data::MENU_WALLET);
    // TODO: Вызвать функцию из wallet_info.rs для показа кошелька
    // wallet_info::show_wallet_summary(bot, q, exchange, cfg, db).await?;
    bot.answer_callback_query(q.id).text("Раздел Кошелек (не реализовано)").await?;
    Ok(())
}

pub async fn handle_menu_info_callback<E>(
    bot: Bot, q: CallbackQuery, exchange: Arc<E>, cfg: Arc<Config>, db: Arc<Db>
) -> anyhow::Result<()> where E: Exchange + Clone + Send + Sync + 'static {
     info!("Callback '{}' triggered. Calling market_info handler...", callback_data::MENU_INFO);
     // TODO: Вызвать функцию из market_info.rs для показа меню информации (статус, funding и т.д.)
     // market_info::show_info_menu(bot, q).await?;
     bot.answer_callback_query(q.id).text("Раздел Информация (не реализовано)").await?;
    Ok(())
}

pub async fn handle_menu_active_ops_callback(
    bot: Bot, q: CallbackQuery, running_operations: RunningOperations, state_storage: StateStorage
) -> anyhow::Result<()> {
    info!("Callback '{}' triggered. Calling active_ops handler...", callback_data::MENU_ACTIVE_OPS);
    // TODO: Вызвать функцию из active_ops.rs для показа активных операций
    // active_ops::show_active_operations(bot, q, running_operations, state_storage).await?;
    bot.answer_callback_query(q.id).text("Раздел Активные операции (не реализовано)").await?;
    Ok(())
}