// src/notifier/hedge_flow_logic/handlers.rs

use crate::notifier::hedge_flow_logic::ui::{make_dialog_keyboard, make_hedge_confirmation_keyboard, prompt_asset_selection};
use crate::notifier::hedge_flow_spawners::{spawn_sequential_hedge_task, spawn_ws_hedge_task};
use crate::notifier::{StateStorage, UserState, RunningOperations, callback_data, navigation};
use crate::config::{Config, HedgeStrategy};
use crate::exchange::Exchange;
use crate::storage::Db;
use crate::hedger::Hedger;
use crate::models::HedgeRequest;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{Message, CallbackQuery, MessageId, InlineKeyboardMarkup, InlineKeyboardButton, MaybeInaccessibleMessage}; // Добавили MaybeInaccessibleMessage
use tracing::{info, warn, error};
use anyhow::{Result, anyhow};

// handle_hedge_command, handle_start_hedge_callback, handle_hedge_asset_callback,
// handle_asset_ticker_input, handle_sum_input остаются БЕЗ ИЗМЕНЕНИЙ

// ... (существующий код handle_hedge_command, handle_start_hedge_callback, handle_hedge_asset_callback, handle_asset_ticker_input, handle_sum_input) ...
// (Код этих функций остается таким же, как в предыдущем вашем ответе)

/// Обработчик команды /hedge [SYMBOL]
pub async fn handle_hedge_command<E>(
    bot: Bot,
    msg: Message,
    symbol_arg: String,
    exchange: Arc<E>,
    state_storage: StateStorage,
    _running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let symbol = symbol_arg.trim().to_uppercase();

    let mut previous_bot_message_id: Option<i32> = None;
    {
        let mut state_guard = state_storage.write().await;
        if let Some(old_state) = state_guard.get(&chat_id) {
             previous_bot_message_id = match old_state {
                 UserState::AwaitingHedgeAssetSelection { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingHedgeSum { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingHedgeVolatility { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingHedgeConfirmation { last_bot_message_id, .. } => *last_bot_message_id,
                 _ => None,
             };
        }
        if !matches!(state_guard.get(&chat_id), Some(UserState::None) | None) {
            info!("Resetting state for {} due to /hedge command", chat_id);
            state_guard.insert(chat_id, UserState::None);
        }
    }

    if let Some(bot_msg_id) = previous_bot_message_id {
        if let Err(e) = bot.delete_message(chat_id, MessageId(bot_msg_id)).await {
            warn!("Failed delete prev bot msg {}: {}", bot_msg_id, e);
        }
    }
    if let Err(e) = bot.delete_message(chat_id, msg.id).await {
        warn!("Failed delete user command msg: {}", e);
    }

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
    if let Some(msg) = q.message.as_ref() { // Работаем со ссылкой на MaybeInaccessibleMessage
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::START_HEDGE, chat_id);
        bot.answer_callback_query(q.id).await?;
        // Передаем ID сообщения, если оно доступно
        let message_id_to_edit = match msg {
            MaybeInaccessibleMessage::Regular(m) => Some(m.id),
            MaybeInaccessibleMessage::Inaccessible(_) => None,
        };
        prompt_asset_selection(&bot, chat_id, &state_storage, exchange, cfg, db, message_id_to_edit).await?;
    } else {
        warn!("CallbackQuery missing message in handle_start_hedge_callback");
        bot.answer_callback_query(q.id).await?;
    }
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
     if let (Some(data), Some(msg)) = (q.data.as_deref(), q.message.as_ref()) { // msg здесь MaybeInaccessibleMessage
         let chat_id = msg.chat().id;
         if let Some(symbol) = data.strip_prefix(callback_data::PREFIX_HEDGE_ASSET) {
              info!("User {} selected asset {} for hedge via callback", chat_id, symbol);
             let is_correct_state = {
                  let state_guard = state_storage.read().await;
                  matches!(state_guard.get(&chat_id), Some(UserState::AwaitingHedgeAssetSelection { .. }))
             };

             if is_correct_state {
                 if let MaybeInaccessibleMessage::Regular(regular_msg) = msg { // Убедимся, что сообщение доступно для редактирования
                     let text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                     let kb = make_dialog_keyboard();
                     bot.edit_message_text(chat_id, regular_msg.id, text).reply_markup(kb).await?;
                     {
                         let mut state_guard = state_storage.write().await;
                         if let Some(current_state @ UserState::AwaitingHedgeAssetSelection { .. }) = state_guard.get_mut(&chat_id) {
                              *current_state = UserState::AwaitingHedgeSum {
                                  symbol: symbol.to_string(),
                                  last_bot_message_id: Some(regular_msg.id.0),
                              };
                              info!("User state for {} set to AwaitingHedgeSum for {}", chat_id, symbol);
                         } else {
                             warn!("State changed unexpectedly for {} before setting AwaitingHedgeSum", chat_id);
                             let _ = navigation::show_main_menu(&bot, chat_id, Some(regular_msg.id)).await;
                             state_guard.insert(chat_id, UserState::None);
                         }
                     }
                 } else {
                    warn!("Message to edit is inaccessible for asset callback, chat_id: {}", chat_id);
                    // Можно отправить новое сообщение или вернуть пользователя в меню
                    let _ = navigation::show_main_menu(&bot, chat_id, None).await;
                    { state_storage.write().await.insert(chat_id, UserState::None); }
                 }
             } else {
                 warn!("User {} clicked hedge asset button but was in wrong state", chat_id);
                 { state_storage.write().await.insert(chat_id, UserState::None); }
                 if let MaybeInaccessibleMessage::Regular(regular_msg) = msg {
                    let _ = navigation::show_main_menu(&bot, chat_id, Some(regular_msg.id)).await;
                 } else {
                    let _ = navigation::show_main_menu(&bot, chat_id, None).await;
                 }
                 bot.answer_callback_query(q.id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                 return Ok(());
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

/// Обработчик ручного ввода тикера
pub async fn handle_asset_ticker_input<E>(
    bot: Bot,
    msg: Message, // Это всегда Message, не MaybeInaccessibleMessage
    _exchange: Arc<E>,
    state_storage: StateStorage,
    cfg: Arc<Config>,
    _db: Arc<Db>,
) -> Result<()>
where
     E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let user_message_id = msg.id;
    let ticker_input = msg.text().unwrap_or("").trim().to_uppercase();

    if ticker_input.is_empty() || ticker_input.starts_with('/') {
        if let Err(e) = bot.delete_message(chat_id, user_message_id).await { warn!("Failed to delete ignored message: {}", e); }
        return Ok(());
    }

    let previous_bot_message_id_opt = {
         let state_guard = state_storage.read().await;
         match state_guard.get(&chat_id) {
            Some(UserState::AwaitingHedgeAssetSelection { last_bot_message_id }) => *last_bot_message_id,
            _ => {
                if let Err(e) = bot.delete_message(chat_id, user_message_id).await { warn!("Failed to delete unexpected text message: {}", e); }
                return Ok(());
            }
         }
    };

    info!("User {} entered ticker '{}' for hedge", chat_id, ticker_input);
    if let Err(e) = bot.delete_message(chat_id, user_message_id).await { warn!("Failed to delete user ticker message: {}", e); }

    let is_valid_ticker = true; // Placeholder

    if is_valid_ticker {
        let prompt_text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, ticker_input);
        let kb = make_dialog_keyboard();

        if let Some(bot_msg_id_int) = previous_bot_message_id_opt {
            let bot_msg_id = MessageId(bot_msg_id_int);
            match bot.edit_message_text(chat_id, bot_msg_id, prompt_text.clone()).reply_markup(kb.clone()).await {
               Ok(_) => {
                    {
                        let mut state_guard = state_storage.write().await;
                         if let Some(current_state @ UserState::AwaitingHedgeAssetSelection { .. }) = state_guard.get_mut(&chat_id) {
                             *current_state = UserState::AwaitingHedgeSum {
                                 symbol: ticker_input.clone(),
                                 last_bot_message_id: Some(bot_msg_id.0),
                            };
                            info!("User state for {} set to AwaitingHedgeSum for {}", chat_id, ticker_input);
                        } else {
                             warn!("State changed for {} before setting AwaitingHedgeSum after ticker input", chat_id);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to edit message {} to prompt sum: {}. Sending new.", bot_msg_id, e);
                    // Если редактирование не удалось, отправляем новое сообщение
                    let new_bot_msg = bot.send_message(chat_id, prompt_text).reply_markup(kb).await?;
                    {
                        let mut state_guard = state_storage.write().await;
                        state_guard.insert(chat_id, UserState::AwaitingHedgeSum {
                             symbol: ticker_input.clone(),
                             last_bot_message_id: Some(new_bot_msg.id.0),
                         });
                         info!("User state for {} set to AwaitingHedgeSum for {} (new message sent)", chat_id, ticker_input);
                    }
                }
            }
        } else {
             warn!("No previous bot message id found for chat_id {} to edit for sum prompt. Sending new.", chat_id);
             let bot_msg = bot.send_message(chat_id, prompt_text).reply_markup(kb).await?;
              {
                 let mut state_guard = state_storage.write().await;
                state_guard.insert(chat_id, UserState::AwaitingHedgeSum {
                     symbol: ticker_input.clone(),
                     last_bot_message_id: Some(bot_msg.id.0),
                 });
                 info!("User state for {} set to AwaitingHedgeSum for {}", chat_id, ticker_input);
               }
        }
    } else {
        let error_text = format!("❌ Символ '{}' не найден или не подходит для хеджирования. Попробуйте другой.", ticker_input);
        if let Some(bot_msg_id_int) = previous_bot_message_id_opt {
             let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
        } else {
             let _ = bot.send_message(chat_id, error_text).await;
        }
    }
    Ok(())
}

/// Обработчик ввода суммы хеджирования
pub async fn handle_sum_input(
    bot: Bot,
    msg: Message, // Это всегда Message
    state_storage: StateStorage,
    cfg: Arc<Config>,
) -> Result<()> {
     let chat_id = msg.chat.id;
    let user_message_id = msg.id;
    let text_input = msg.text().unwrap_or("").trim();

    let (symbol, previous_bot_message_id_opt) = {
        let state_guard = state_storage.read().await;
        match state_guard.get(&chat_id) {
            Some(UserState::AwaitingHedgeSum { symbol, last_bot_message_id }) => (symbol.clone(), *last_bot_message_id),
            _ => {
                if let Err(e) = bot.delete_message(chat_id, user_message_id).await { warn!("Failed to delete unexpected sum message: {}", e); }
                return Ok(());
            }
        }
    };

     if let Err(e) = bot.delete_message(chat_id, user_message_id).await { warn!("Failed to delete user sum message: {}", e); }

    match text_input.parse::<f64>() {
         Ok(sum) if sum > 0.0 => {
             info!("User {} entered sum {} for hedge {}", chat_id, sum, symbol);
             let prompt_text = format!("Введите ожидаемую волатильность для {} {} (%):", sum, cfg.quote_currency);
             let kb = make_dialog_keyboard();

             if let Some(bot_msg_id_int) = previous_bot_message_id_opt {
                 let bot_msg_id = MessageId(bot_msg_id_int);
                 match bot.edit_message_text(chat_id, bot_msg_id, prompt_text.clone()).reply_markup(kb.clone()).await {
                    Ok(_) => {
                         {
                             let mut state_guard = state_storage.write().await;
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
                         error!("Failed to edit message {} to prompt volatility: {}. Sending new.", bot_msg_id, e);
                         let new_bot_msg = bot.send_message(chat_id, prompt_text).reply_markup(kb).await?;
                         {
                            let mut state_guard = state_storage.write().await;
                            state_guard.insert(chat_id, UserState::AwaitingHedgeVolatility {
                                symbol: symbol.clone(),
                                sum,
                                last_bot_message_id: Some(new_bot_msg.id.0),
                           });
                           info!("User state for {} set to AwaitingHedgeVolatility (new message sent)", chat_id);
                         }
                    }
                 }
             } else {
                 warn!("No previous bot message id found for chat_id {} to edit for volatility prompt. Sending new.", chat_id);
                 let bot_msg = bot.send_message(chat_id, prompt_text).reply_markup(kb).await?;
                  {
                     let mut state_guard = state_storage.write().await;
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
             warn!("User {} entered non-positive sum: {}", chat_id, text_input);
              if let Some(bot_msg_id_int) = previous_bot_message_id_opt {
                   let error_text = format!("⚠️ Сумма должна быть положительной. Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                   let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
              }
         }
         Err(_) => {
             warn!("User {} entered invalid sum format: {}", chat_id, text_input);
             if let Some(bot_msg_id_int) = previous_bot_message_id_opt {
                 let error_text = format!("⚠️ Неверный формат суммы. Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                 let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
             }
         }
    }
     Ok(())
}

/// Обработчик ввода волатильности хеджирования
pub async fn handle_volatility_input<E>(
    bot: Bot,
    msg: Message, // Это всегда Message
    exchange: Arc<E>,
    state_storage: StateStorage,
    _running_operations: RunningOperations,
    cfg: Arc<Config>,
    _db: Arc<Db>,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let user_message_id = msg.id;
    let text_input = msg.text().unwrap_or("").trim();

    let (symbol, sum, previous_bot_message_id_opt) = {
        let state_guard = state_storage.read().await;
        match state_guard.get(&chat_id) {
            Some(UserState::AwaitingHedgeVolatility { symbol, sum, last_bot_message_id }) => (symbol.clone(), *sum, *last_bot_message_id),
            _ => {
                if let Err(e) = bot.delete_message(chat_id, user_message_id).await { warn!("Failed to delete unexpected volatility message: {}", e); }
                return Ok(());
            }
        }
    };

     if let Err(e) = bot.delete_message(chat_id, user_message_id).await { warn!("Failed to delete user volatility message: {}", e); }

    match text_input.trim_end_matches('%').trim().parse::<f64>() {
        Ok(volatility_percent) if volatility_percent >= 0.0 => {
            info!("User {} entered volatility {}% for hedge {} {}", chat_id, volatility_percent, sum, symbol);
            let volatility_fraction = volatility_percent / 100.0;

            let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction };
            let hedger = Hedger::new((*exchange).clone(), (*cfg).clone());

            let calc_indicator_text = "⏳ Расчет параметров хеджирования...";
            let mut bot_msg_id_for_confirm_opt = previous_bot_message_id_opt.map(MessageId);

            if let Some(bot_msg_id) = bot_msg_id_for_confirm_opt {
                 let _ = bot.edit_message_text(chat_id, bot_msg_id, calc_indicator_text)
                    .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                    .await;
            } else {
                 bot_msg_id_for_confirm_opt = Some(bot.send_message(chat_id, calc_indicator_text).await?.id);
            }
            // Это ID сообщения, которое будет содержать кнопки подтверждения
            let bot_msg_id_for_confirmation = bot_msg_id_for_confirm_opt.ok_or_else(|| anyhow!("Failed to get/send bot message ID for calculation status"))?;


            match hedger.calculate_hedge_params(&hedge_request).await {
                Ok(params) => {
                    info!("Hedge parameters calculated for {}: {:?}", chat_id, params);
                    let confirmation_text = format!(
                        "Подтвердите параметры хеджирования для {}:\n\n\
                         Сумма: {:.2} {}\n\
                         Волатильность: {:.1}%\n\
                         --- Расчет ---\n\
                         Спот (брутто): ~{:.8} {}\n\
                         Фьючерс (нетто): ~{:.8} {}\n\
                         Требуемое плечо: ~{:.2}x (Макс: {:.1}x)\n\n\
                         Выберите стратегию для запуска:",
                        symbol, sum, cfg.quote_currency,
                        volatility_percent,
                        params.spot_order_qty, symbol,
                        params.fut_order_qty, symbol,
                        (params.fut_order_qty * params.current_spot_price) / params.available_collateral.max(f64::EPSILON),
                        cfg.max_allowed_leverage
                    );
                    let kb = make_hedge_confirmation_keyboard(); // Новая клавиатура с выбором стратегии
                    bot.edit_message_text(chat_id, bot_msg_id_for_confirmation, confirmation_text).reply_markup(kb).await?;

                    {
                        let mut state_guard = state_storage.write().await;
                        if let Some(current_state @ UserState::AwaitingHedgeVolatility { .. }) = state_guard.get_mut(&chat_id) {
                            *current_state = UserState::AwaitingHedgeConfirmation {
                                symbol: symbol.clone(),
                                sum,
                                volatility: volatility_fraction,
                                last_bot_message_id: Some(bot_msg_id_for_confirmation.0),
                           };
                           info!("User state for {} set to AwaitingHedgeConfirmation (with strategy selection)", chat_id);
                       } else {
                            warn!("State changed for {} before setting AwaitingHedgeConfirmation", chat_id);
                            // Можно вернуть в главное меню или просто сбросить состояние
                            state_guard.insert(chat_id, UserState::None);
                       }
                    }
                }
                Err(e) => {
                    error!("Hedge parameter calculation failed for {}: {}", chat_id, e);
                    let error_text = format!("❌ Ошибка расчета параметров: {}\nПопробуйте изменить сумму или волатильность.", e);
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("❌ Отмена", callback_data::CANCEL_DIALOG)
                    ]]);
                    bot.edit_message_text(chat_id, bot_msg_id_for_confirmation, error_text).reply_markup(kb).await?;
                    { state_storage.write().await.insert(chat_id, UserState::None); }
                }
            }
        }
        Ok(_) => {
             warn!("User {} entered non-positive volatility: {}", chat_id, text_input);
              if let Some(bot_msg_id_int) = previous_bot_message_id_opt {
                 let error_text = format!("⚠️ Волатильность должна быть не отрицательной (в %). Введите снова:");
                 let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
              }
        }
        Err(_) => {
             warn!("User {} entered invalid volatility format: {}", chat_id, text_input);
             if let Some(bot_msg_id_int) = previous_bot_message_id_opt {
                 let error_text = format!("⚠️ Неверный формат волатильности (в %). Введите снова:");
                 let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
             }
        }
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
    let query_id = q.id.clone();
    // ИЗМЕНЕНО: q.message это Option<MaybeInaccessibleMessage>
    // Мы должны его сохранить и передать в спаунеры.
    let maybe_bot_message = q.message; // Тип: Option<MaybeInaccessibleMessage>

    if let (Some(data), Some(msg_ref)) = (q.data.as_deref(), maybe_bot_message.as_ref()) { // Работаем со ссылкой для получения ID и chat_id
        let chat_id = msg_ref.chat().id;
        let message_id = msg_ref.id(); // Это MessageId, если msg_ref это Regular

        if let Some(payload) = data.strip_prefix(callback_data::PREFIX_HEDGE_CONFIRM) {
            let state_data_opt = {
                let state_guard = state_storage.read().await;
                match state_guard.get(&chat_id) {
                    Some(UserState::AwaitingHedgeConfirmation { symbol, sum, volatility, .. }) => {
                        Some((symbol.clone(), *sum, *volatility))
                    }
                    _ => {
                        warn!("User {} confirmed hedge but was in wrong state. Current state: {:?}", chat_id, state_guard.get(&chat_id));
                        None
                    }
                }
            };

            if payload == "no" {
                info!("User {} cancelled hedge at confirmation (payload 'no')", chat_id);
                bot.answer_callback_query(query_id).await?;
                if let MaybeInaccessibleMessage::Regular(regular_msg) = msg_ref {
                    navigation::handle_cancel_dialog(bot.clone(), chat_id, regular_msg.id, state_storage).await?;
                } else {
                    navigation::handle_cancel_dialog(bot.clone(), chat_id, MessageId(0), state_storage).await?; // Заглушка ID если сообщение недоступно
                    warn!("Cannot cancel dialog accurately as message is inaccessible for chat_id {}", chat_id);
                }
                return Ok(());
            }

            if let Some((symbol, sum, volatility_fraction)) = state_data_opt {
                if payload == "seq" || payload == "ws" {
                    info!("User {} confirmed hedge. Chosen strategy via payload: '{}'", chat_id, payload);
                    { state_storage.write().await.insert(chat_id, UserState::None); }

                    let chosen_strategy = if payload == "seq" {
                        HedgeStrategy::Sequential
                    } else {
                        HedgeStrategy::WebsocketChunks
                    };

                    // Важно: передаем maybe_bot_message.clone(), а не msg_ref
                    // так как msg_ref это ссылка, а нам нужно владение для передачи в другую задачу.
                    if let Some(initial_message_for_spawner) = maybe_bot_message.clone() {
                        match chosen_strategy {
                            HedgeStrategy::Sequential => {
                                 // Редактируем сообщение, если оно доступно
                                 if let MaybeInaccessibleMessage::Regular(regular_msg) = &initial_message_for_spawner {
                                     let waiting_text = format!("⏳ Запуск хеджирования (Sequential) для {}...", symbol);
                                     let _ = bot.edit_message_text(chat_id, regular_msg.id, waiting_text)
                                        .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())).await;
                                 }

                                 let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction };
                                 let hedger = Hedger::new((*exchange).clone(), (*cfg).clone());

                                 match hedger.calculate_hedge_params(&hedge_request).await {
                                     Ok(params) => {
                                         info!("Sequential hedge params OK for {}: {:?}", chat_id, params);
                                         // Передаем initial_message_for_spawner
                                         spawn_sequential_hedge_task(
                                             bot.clone(), exchange.clone(), cfg.clone(), db.clone(),
                                             running_operations.clone(), chat_id, params, sum,
                                             volatility_fraction * 100.0, initial_message_for_spawner,
                                         ).await;
                                     }
                                     Err(e) => {
                                         error!("Hedge parameter calculation failed just before execution for {}: {}", chat_id, e);
                                         let error_text = format!("❌ Ошибка расчета параметров перед запуском: {}\nПопробуйте снова.", e);
                                         if let MaybeInaccessibleMessage::Regular(regular_msg) = &initial_message_for_spawner {
                                            let _ = bot.edit_message_text(chat_id, regular_msg.id, error_text)
                                                     .reply_markup(navigation::make_main_menu_keyboard())
                                                     .await;
                                         } else { // Если сообщение недоступно, отправляем новое
                                            let _ = bot.send_message(chat_id, error_text).reply_markup(navigation::make_main_menu_keyboard()).await;
                                         }
                                     }
                                 }
                            }
                            HedgeStrategy::WebsocketChunks => {
                                if let MaybeInaccessibleMessage::Regular(regular_msg) = &initial_message_for_spawner {
                                    let waiting_text = format!("⏳ Запуск хеджирования (WebSocket) для {}...", symbol);
                                    let _ = bot.edit_message_text(chat_id, regular_msg.id, waiting_text)
                                        .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())).await;
                                }
                                 // Передаем initial_message_for_spawner
                                 let spawn_result = spawn_ws_hedge_task(
                                    bot.clone(), exchange.clone(), cfg.clone(), db.clone(),
                                    running_operations.clone(), chat_id,
                                    HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction },
                                    initial_message_for_spawner,
                                 ).await;

                                 if let Err(spawn_err) = spawn_result {
                                     error!("Failed to spawn WS hedge task for chat_id {}: {}", chat_id, spawn_err);
                                     // Сообщение об ошибке уже обработано в spawn_ws_hedge_task, если initial_message_for_spawner было Regular
                                     // Если оно было Inaccessible и отправка нового не удалась, здесь делать нечего.
                                 }
                            }
                        }
                    } else { // maybe_bot_message было None, что странно, т.к. мы вошли в if let Some(msg_ref)
                        warn!("Original CallbackQuery.message was None unexpectedly in handle_hedge_confirm_callback for chat_id {}", chat_id);
                    }
                } else {
                     warn!("Invalid payload '{}' for hedge confirmation for chat_id {}", payload, chat_id);
                }
            } else {
                 warn!("User {} confirmed hedge but was in wrong state (state_data_opt was None)", chat_id);
                 if let MaybeInaccessibleMessage::Regular(regular_msg) = msg_ref {
                    let _ = navigation::show_main_menu(&bot, chat_id, Some(regular_msg.id)).await;
                 } else {
                    let _ = navigation::show_main_menu(&bot, chat_id, None).await;
                 }
                 { state_storage.write().await.insert(chat_id, UserState::None); }
            }
        } else if data == callback_data::CANCEL_DIALOG {
             info!("User {} cancelled hedge dialog via dedicated cancel button", chat_id);
             if let MaybeInaccessibleMessage::Regular(regular_msg) = msg_ref {
                navigation::handle_cancel_dialog(bot.clone(), chat_id, regular_msg.id, state_storage).await?;
             } else {
                navigation::handle_cancel_dialog(bot.clone(), chat_id, MessageId(0), state_storage).await?;
                warn!("Cannot cancel dialog accurately as message is inaccessible for chat_id {}", chat_id);
             }
        } else {
             warn!("Invalid callback data prefix for hedge confirmation: {}", data);
        }
    } else {
        warn!("CallbackQuery missing data or message in handle_hedge_confirm_callback");
    }
    bot.answer_callback_query(query_id).await?;
    Ok(())
}