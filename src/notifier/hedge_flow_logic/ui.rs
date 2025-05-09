// src/notifier/hedge_flow_logic/ui.rs

use super::super::{StateStorage, UserState, callback_data}; // Импорт из родительского notifier
use crate::config::Config;
use crate::exchange::Exchange;
use crate::storage::Db;
use crate::notifier::wallet_info; // Для get_formatted_balances
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ChatId,
};
use tracing::{info, error};
use anyhow::Result;

// Создает клавиатуру подтверждения хеджа с выбором стратегии
pub(super) fn make_hedge_confirmation_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback(
                "🚀 Запустить Sequential",
                // Используем существующий PREFIX_HEDGE_CONFIRM, но с новым payload "seq"
                format!("{}{}", callback_data::PREFIX_HEDGE_CONFIRM, "seq")
            ),
        ],
        vec![
            InlineKeyboardButton::callback(
                "🛰️ Запустить WebSocket",
                // Используем существующий PREFIX_HEDGE_CONFIRM, но с новым payload "ws"
                format!("{}{}", callback_data::PREFIX_HEDGE_CONFIRM, "ws")
            ),
        ],
        vec![
            InlineKeyboardButton::callback("❌ Нет, отмена", callback_data::CANCEL_DIALOG),
        ],
    ])
}

// Создает простую клавиатуру с отменой
pub(super) fn make_dialog_keyboard() -> InlineKeyboardMarkup {
     InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("❌ Отмена", callback_data::CANCEL_DIALOG),
    ]])
}

// Запрашивает у пользователя выбор актива для хеджирования
pub(super) async fn prompt_asset_selection<E>(
    bot: &Bot, // Принимаем бот по ссылке
    chat_id: ChatId,
    state_storage: &StateStorage, // Принимаем по ссылке
    exchange: Arc<E>,
    cfg: Arc<Config>,
    _db: Arc<Db>,
    message_id_to_edit: Option<MessageId>,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    info!("Prompting asset selection for hedge, chat_id: {}", chat_id);
    let loading_text = "⏳ Загрузка доступных активов...";
    let mut bot_message_id = message_id_to_edit;

    // Показываем индикатор загрузки
    if let Some(msg_id) = bot_message_id {
        let kb = InlineKeyboardMarkup::new(vec![vec![
             InlineKeyboardButton::callback("⬅️ Назад", callback_data::BACK_TO_MAIN)
        ]]);
        let _ = bot.edit_message_text(chat_id, msg_id, loading_text).reply_markup(kb).await;
    } else {
        let sent_msg = bot.send_message(chat_id, loading_text).await?;
        bot_message_id = Some(sent_msg.id);
    }

    // Получаем балансы и формируем кнопки
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

            // Редактируем или отправляем новое сообщение с выбором
            if let Some(msg_id) = bot_message_id {
                if let Err(e) = bot.edit_message_text(chat_id, msg_id, &text).reply_markup(keyboard.clone()).await {
                   error!("Failed to edit message for asset selection: {}. Sending new.", e);
                   bot_message_id = Some(bot.send_message(chat_id, text).reply_markup(keyboard).await?.id);
                }
            } else {
                 bot_message_id = Some(bot.send_message(chat_id, text).reply_markup(keyboard).await?.id);
            }

            // Устанавливаем состояние пользователя
            {
                let mut state_guard = state_storage.write().await;
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
              { state_storage.write().await.insert(chat_id, UserState::None); }
             return Err(e.into());
        }
    }
    Ok(())
}