// src/notifier/hedge_flow_logic/handlers.rs

use super::ui::{make_dialog_keyboard, make_hedge_confirmation_keyboard, prompt_asset_selection};
use super::spawners::spawn_sequential_hedge_task; // Пока используем старый спавнер
use crate::notifier::{StateStorage, UserState, RunningOperations, callback_data, navigation};
use crate::config::Config;
use crate::exchange::Exchange;
use crate::storage::Db;
use crate::hedger::Hedger; // Для старой логики расчета
use crate::models::HedgeRequest;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{Message, CallbackQuery, ChatId, MessageId, MaybeInaccessibleMessage};
use tracing::{info, warn, error};
use anyhow::Result; // Добавляем anyhow

/// Обработчик команды /hedge [SYMBOL]
pub async fn handle_hedge_command<E>(
    bot: Bot,
    msg: Message,
    symbol_arg: String,
    exchange: Arc<E>,
    state_storage: StateStorage,
    _running_operations: RunningOperations, // Оставляем для совместимости, но не используем в WS
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let symbol = symbol_arg.trim().to_uppercase();
    // ... (остальная логика handle_hedge_command без изменений) ...
     let mut previous_bot_message_id: Option<i32> = None;
     {
         let mut state_guard = state_storage.write().await;
         if let Some(old_state) = state_guard.get(&chat_id) { /* ... */ }
         if !matches!(state_guard.get(&chat_id), Some(UserState::None) | None) { /* ... */ state_guard.insert(chat_id, UserState::None); }
     }
     if let Some(bot_msg_id) = previous_bot_message_id { if let Err(e) = bot.delete_message(chat_id, MessageId(bot_msg_id)).await { warn!(/* ... */); } }
     if let Err(e) = bot.delete_message(chat_id, msg.id).await { warn!(/* ... */); }

     if symbol.is_empty() {
         info!("Processing /hedge command without symbol for chat_id: {}", chat_id);
         prompt_asset_selection(&bot, chat_id, &state_storage, exchange, cfg, db, None).await?;
     } else {
         info!("Processing /hedge command for chat_id: {}, symbol: {}", chat_id, symbol);
         let text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
         let kb = make_dialog_keyboard();
         let bot_msg = bot.send_message(chat_id, text).reply_markup(kb).await?;
         {
             let mut state_guard = state_storage.write().await;
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
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    // ... (логика без изменений, вызывает prompt_asset_selection) ...
    if let Some(msg) = q.message.as_ref() {
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::START_HEDGE, chat_id);
        bot.answer_callback_query(q.id).await?;
        prompt_asset_selection(&bot, chat_id, &state_storage, exchange, cfg, db, Some(msg.id())).await?;
    } else { /* ... */ }
    Ok(())
}

/// Обработчик колбэка выбора актива для хеджа
pub async fn handle_hedge_asset_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    _exchange: Arc<E>,
    state_storage: StateStorage,
    cfg: Arc<Config>,
    _db: Arc<Db>,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
     // ... (логика без изменений) ...
     if let (Some(data), Some(msg)) = (q.data.as_deref(), q.message.as_ref()) {
         let chat_id = msg.chat().id;
         if let Some(symbol) = data.strip_prefix(callback_data::PREFIX_HEDGE_ASSET) {
              info!(/* ... */);
              let is_correct_state = { /* ... */ };
              if is_correct_state {
                  let text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                  let kb = make_dialog_keyboard();
                  bot.edit_message_text(chat_id, msg.id(), text).reply_markup(kb).await?;
                  { /* ... обновление state на AwaitingHedgeSum ... */ }
              } else { /* ... обработка неверного состояния ... */ return Ok(()); }
         } else { warn!(/* ... */); }
     } else { warn!(/* ... */); }
      bot.answer_callback_query(q.id).await?;
      Ok(())
}

/// Обработчик ручного ввода тикера
pub async fn handle_asset_ticker_input<E>(
    bot: Bot,
    msg: Message,
    _exchange: Arc<E>,
    state_storage: StateStorage,
    cfg: Arc<Config>,
    _db: Arc<Db>,
) -> Result<()>
where
     E: Exchange + Clone + Send + Sync + 'static,
{
     // ... (логика без изменений) ...
     let chat_id = msg.chat.id;
     let message_id = msg.id;
     let text = msg.text().unwrap_or("").trim().to_uppercase();
     if text.is_empty() || text.starts_with('/') { /* ... */ return Ok(()); }
     let previous_bot_message_id = { /* ... */ };
     if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!(/* ... */); }
     let is_valid_ticker = true; // TODO: Валидация
     if is_valid_ticker {
         let prompt_text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, text);
         let kb = make_dialog_keyboard();
         if let Some(bot_msg_id_int) = previous_bot_message_id { /* ... редактируем сообщение ... */ } else { /* ... отправляем новое ... */ }
         { /* ... обновляем state на AwaitingHedgeSum ... */ }
     } else { /* ... сообщаем об ошибке ... */ }
     Ok(())
}


/// Обработчик ввода суммы
pub async fn handle_sum_input(
    bot: Bot,
    msg: Message,
    state_storage: StateStorage,
    cfg: Arc<Config>,
) -> Result<()> {
     // ... (логика без изменений) ...
      let chat_id = msg.chat.id;
     let message_id = msg.id;
     let text = msg.text().unwrap_or("").trim();
     let (symbol, previous_bot_message_id) = { /* ... */ };
      if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!(/* ... */); }
      match text.parse::<f64>() {
          Ok(sum) if sum > 0.0 => {
               info!(/* ... */);
               let prompt_text = format!("Введите ожидаемую волатильность для {} {} (%):", sum, cfg.quote_currency);
               let kb = make_dialog_keyboard();
               if let Some(bot_msg_id_int) = previous_bot_message_id { /* ... редактируем ... */ } else { /* ... отправляем новое ... */ }
               { /* ... обновляем state на AwaitingHedgeVolatility ... */ }
           }
           Ok(_) => { warn!(/* ... не положительная сумма ... */); }
           Err(_) => { warn!(/* ... неверный формат ... */); }
      }
     Ok(())
}

/// Обработчик ввода волатильности
pub async fn handle_volatility_input<E>(
    bot: Bot,
    msg: Message,
    exchange: Arc<E>,
    state_storage: StateStorage,
    _running_operations: RunningOperations,
    cfg: Arc<Config>,
    _db: Arc<Db>,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
     // ... (логика без изменений, вызывает calculate_hedge_params и prompt_confirmation) ...
     let chat_id = msg.chat.id;
     let message_id = msg.id;
     let text = msg.text().unwrap_or("").trim();
     let (symbol, sum, previous_bot_message_id) = { /* ... */ };
      if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!(/* ... */); }
      match text.trim_end_matches('%').trim().parse::<f64>() {
          Ok(volatility_percent) if volatility_percent >= 0.0 => {
               info!(/* ... */);
               let volatility_fraction = volatility_percent / 100.0;
               let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction };
               let hedger = Hedger::new((*exchange).clone(), (*cfg).clone()); // Используем старый Hedger для расчета параметров
               let calc_indicator_text = "⏳ Расчет параметров хеджирования...";
               let mut bot_msg_id_opt = previous_bot_message_id.map(MessageId);
               if let Some(bot_msg_id) = bot_msg_id_opt { /* ... */ } else { /* ... */ }
               let bot_msg_id = bot_msg_id_opt.ok_or_else(|| anyhow::anyhow!("Failed to get bot message ID"))?;

               match hedger.calculate_hedge_params(&hedge_request).await {
                   Ok(params) => {
                        info!(/* ... */);
                       let confirmation_text = format!(/* ... */);
                       let kb = make_hedge_confirmation_keyboard(); // Клавиатура подтверждения
                       bot.edit_message_text(chat_id, bot_msg_id, confirmation_text).reply_markup(kb).await?;
                       { /* ... обновляем state на AwaitingHedgeConfirmation ... */ }
                   }
                   Err(e) => { error!(/* ... */); /* ... сообщаем об ошибке ... */ }
               }
           }
          Ok(_) => { warn!(/* ... */); }
          Err(_) => { warn!(/* ... */); }
     }
      Ok(())
}

/// Обработчик колбэка подтверждения хеджа
pub async fn handle_hedge_confirm_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: Arc<E>,
    state_storage: StateStorage,
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    // !!! ЗДЕСЬ НУЖНО БУДЕТ ДОБАВИТЬ ЛОГИКУ ВЫБОРА СТРАТЕГИИ !!!
    // Пока что она просто вызывает старый spawn_hedge_task
    let query_id = q.id.clone();

    if let (Some(data), Some(msg)) = (q.data.as_deref(), q.message.as_ref()) {
        let chat_id = msg.chat().id;
        let message_id = msg.id();

        if let Some(payload) = data.strip_prefix(callback_data::PREFIX_HEDGE_CONFIRM) {
            if payload == "yes" {
                if let Some(msg_owned) = q.message { // Перемещаем msg сюда
                    info!("User {} confirmed hedge operation (Sequential)", chat_id);

                    let (symbol, sum, volatility_fraction) = { /* ... получаем данные из state ... */ };
                    { state_storage.write().await.insert(chat_id, UserState::None); } // Сбрасываем state

                    let waiting_text = format!("⏳ Запуск хеджирования (Sequential) для {}...", symbol);
                    bot.edit_message_text(chat_id, message_id, waiting_text)
                       .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())).await?;

                    let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction };
                    let hedger = Hedger::new((*exchange).clone(), (*cfg).clone()); // Старый hedger

                    match hedger.calculate_hedge_params(&hedge_request).await {
                        Ok(params) => {
                            info!("Sequential hedge params OK for {}: {:?}", chat_id, params);
                            // Вызываем старый спавнер
                            spawn_sequential_hedge_task(
                                bot.clone(), exchange.clone(), cfg.clone(), db.clone(),
                                running_operations.clone(), chat_id, params, sum,
                                volatility_fraction * 100.0, msg_owned, // Передаем перемещенное сообщение
                            ).await;
                        }
                        Err(e) => { error!(/* ... */); /* ... обработка ошибки ... */ return Ok(()); }
                    }
                } else { warn!("CallbackQuery missing message on 'yes' confirmation"); }
            } else if payload == "no" {
                info!("User {} cancelled hedge at confirmation", chat_id);
                navigation::handle_cancel_dialog(bot, chat_id, message_id, state_storage).await?;
            } else { warn!("Invalid payload for hedge confirmation: {}", payload); }
        } else if data == callback_data::CANCEL_DIALOG {
             info!("User cancelled hedge dialog via cancel button");
             navigation::handle_cancel_dialog(bot, chat_id, message_id, state_storage).await?;
        } else { warn!("Invalid callback data format for hedge confirmation prefix: {}", data); }
    } else { warn!("CallbackQuery missing data in handle_hedge_confirm_callback"); }

    bot.answer_callback_query(query_id).await?;
    Ok(())
}