// src/notifier/hedge_flow_logic/handlers.rs

use super::ui::{make_dialog_keyboard, make_hedge_confirmation_keyboard, prompt_asset_selection};
use super::spawners::{spawn_sequential_hedge_task, spawn_ws_hedge_task};
use crate::notifier::{StateStorage, UserState, RunningOperations, callback_data, navigation};
use crate::config::{Config, HedgeStrategy};
use crate::exchange::Exchange;
use crate::storage::Db;
use crate::hedger::Hedger;
use crate::models::HedgeRequest;
use std::sync::Arc;
use teloxide::prelude::*;
// --- ИСПРАВЛЕНО: Удалены ChatId и MaybeInaccessibleMessage ---
use teloxide::types::{Message, CallbackQuery, MessageId, InlineKeyboardMarkup, InlineKeyboardButton};
use tracing::{info, warn, error};
use anyhow::{Result, anyhow};

// ... (handle_hedge_command, handle_start_hedge_callback, handle_hedge_asset_callback, handle_asset_ticker_input, handle_sum_input без изменений) ...

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

    // Получаем ID предыдущего сообщения бота, если оно было, чтобы удалить его
    let mut previous_bot_message_id: Option<i32> = None;
    {
        let mut state_guard = state_storage.write().await;
        // Получаем предыдущее состояние, чтобы взять ID сообщения
        if let Some(old_state) = state_guard.get(&chat_id) {
             previous_bot_message_id = match old_state {
                 UserState::AwaitingHedgeAssetSelection { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingHedgeSum { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingHedgeVolatility { last_bot_message_id, .. } => *last_bot_message_id,
                 UserState::AwaitingHedgeConfirmation { last_bot_message_id, .. } => *last_bot_message_id,
                 _ => None,
             };
        }
        // Сбрасываем состояние в любом случае при вызове команды
        if !matches!(state_guard.get(&chat_id), Some(UserState::None) | None) {
            info!("Resetting state for {} due to /hedge command", chat_id);
            state_guard.insert(chat_id, UserState::None);
        }
    } // Блокировка state_guard освобождается здесь

    // Удаляем предыдущее сообщение бота (если было)
    if let Some(bot_msg_id) = previous_bot_message_id {
        if let Err(e) = bot.delete_message(chat_id, MessageId(bot_msg_id)).await {
            warn!("Failed delete prev bot msg {}: {}", bot_msg_id, e);
        }
    }
    // Удаляем сообщение пользователя с командой
    if let Err(e) = bot.delete_message(chat_id, msg.id).await {
        warn!("Failed delete user command msg: {}", e);
    }


    if symbol.is_empty() {
        info!("Processing /hedge command without symbol for chat_id: {}", chat_id);
        // Запрашиваем выбор актива, т.к. символ не указан
        prompt_asset_selection(&bot, chat_id, &state_storage, exchange, cfg, db, None).await?;
    } else {
        info!("Processing /hedge command for chat_id: {}, symbol: {}", chat_id, symbol);
        // Сразу запрашиваем сумму
        let text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
        let kb = make_dialog_keyboard();
        let bot_msg = bot.send_message(chat_id, text).reply_markup(kb).await?;
        {
            let mut state_guard = state_storage.write().await;
            // Устанавливаем состояние ожидания суммы
            state_guard.insert(chat_id, UserState::AwaitingHedgeSum {
                symbol: symbol.clone(),
                last_bot_message_id: Some(bot_msg.id.0), // Сохраняем ID сообщения бота
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
    if let Some(msg) = q.message.as_ref() {
        let chat_id = msg.chat().id;
        info!("Processing '{}' callback for chat_id: {}", callback_data::START_HEDGE, chat_id);
        bot.answer_callback_query(q.id).await?;
        // Запускаем процесс выбора актива, передавая ID текущего сообщения для редактирования
        prompt_asset_selection(&bot, chat_id, &state_storage, exchange, cfg, db, Some(msg.id())).await?;
    } else {
        warn!("CallbackQuery missing message in handle_start_hedge_callback");
        bot.answer_callback_query(q.id).await?; // Отвечаем на колбэк в любом случае
    }
    Ok(())
}

/// Обработчик колбэка выбора актива для хеджа
pub async fn handle_hedge_asset_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    _exchange: Arc<E>, // Не используется напрямую здесь
    state_storage: StateStorage,
    cfg: Arc<Config>,
    _db: Arc<Db>, // Не используется напрямую здесь
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
     if let (Some(data), Some(msg)) = (q.data.as_deref(), q.message.as_ref()) {
         let chat_id = msg.chat().id;
         if let Some(symbol) = data.strip_prefix(callback_data::PREFIX_HEDGE_ASSET) {
              info!("User {} selected asset {} for hedge via callback", chat_id, symbol);
             // Проверяем, что пользователь в правильном состоянии
             let is_correct_state = {
                  let state_guard = state_storage.read().await;
                  matches!(state_guard.get(&chat_id), Some(UserState::AwaitingHedgeAssetSelection { .. }))
             };

             if is_correct_state {
                 // Запрашиваем сумму
                 let text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                 let kb = make_dialog_keyboard(); // Клавиатура с кнопкой "Отмена"
                 bot.edit_message_text(chat_id, msg.id(), text).reply_markup(kb).await?;
                 // Обновляем состояние пользователя
                 {
                     let mut state_guard = state_storage.write().await;
                     if let Some(current_state @ UserState::AwaitingHedgeAssetSelection { .. }) = state_guard.get_mut(&chat_id) {
                          *current_state = UserState::AwaitingHedgeSum {
                              symbol: symbol.to_string(),
                              last_bot_message_id: Some(msg.id().0), // Сохраняем ID отредакт. сообщения
                          };
                          info!("User state for {} set to AwaitingHedgeSum for {}", chat_id, symbol);
                     } else {
                         // Состояние могло измениться между read() и write() - редкий случай
                         warn!("State changed unexpectedly for {} before setting AwaitingHedgeSum", chat_id);
                         // Возвращаем в главное меню
                         let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await;
                         // Сбрасываем состояние на всякий случай
                         state_guard.insert(chat_id, UserState::None);
                     }
                 }
             } else {
                 // Пользователь был не в том состоянии (возможно, нажал старую кнопку)
                 warn!("User {} clicked hedge asset button but was in wrong state", chat_id);
                 { state_storage.write().await.insert(chat_id, UserState::None); } // Сбрасываем состояние
                 let _ = navigation::show_main_menu(&bot, chat_id, Some(msg.id())).await;
                 bot.answer_callback_query(q.id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                 return Ok(());
             }
         } else {
              warn!("Invalid callback data format for hedge asset selection: {}", data);
              bot.answer_callback_query(q.id).await?; // Отвечаем, чтобы убрать часики
              return Ok(()); // Выходим без ошибки
         }
     } else {
          warn!("CallbackQuery missing data or message in handle_hedge_asset_callback");
          bot.answer_callback_query(q.id).await?; // Отвечаем
          return Ok(()); // Выходим
     }
      bot.answer_callback_query(q.id).await?; // Отвечаем на колбэк
      Ok(())
}

/// Обработчик ручного ввода тикера
pub async fn handle_asset_ticker_input<E>(
    bot: Bot,
    msg: Message,
    _exchange: Arc<E>, // Не используется напрямую
    state_storage: StateStorage,
    cfg: Arc<Config>,
    _db: Arc<Db>, // Не используется напрямую
) -> Result<()>
where
     E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let message_id = msg.id; // ID сообщения пользователя
    let ticker_input = msg.text().unwrap_or("").trim().to_uppercase();

    // Игнорируем пустые сообщения или команды
    if ticker_input.is_empty() || ticker_input.starts_with('/') {
        if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete ignored message: {}", e); }
        return Ok(());
    }

    // Получаем ID сообщения бота, которое нужно редактировать
    let previous_bot_message_id = {
         let state_guard = state_storage.read().await;
         match state_guard.get(&chat_id) {
            // Убеждаемся, что пользователь в нужном состоянии
            Some(UserState::AwaitingHedgeAssetSelection { last_bot_message_id }) => *last_bot_message_id,
            _ => {
                 // Пользователь не в том состоянии, удаляем его сообщение и выходим
                if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete unexpected text message: {}", e); }
                return Ok(());
            }
         }
    };

    info!("User {} entered ticker '{}' for hedge", chat_id, ticker_input);

    // Удаляем сообщение пользователя с тикером
    if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete user ticker message: {}", e); }

    // TODO: Добавить валидацию тикера через exchange.get_spot_instrument_info или подобное
    let is_valid_ticker = true; // Заглушка валидации

    if is_valid_ticker {
        // Запрашиваем сумму
        let prompt_text = format!("Введите сумму {} для хеджирования {}:", cfg.quote_currency, ticker_input);
        let kb = make_dialog_keyboard();

        if let Some(bot_msg_id_int) = previous_bot_message_id {
            let bot_msg_id = MessageId(bot_msg_id_int);
            match bot.edit_message_text(chat_id, bot_msg_id, prompt_text).reply_markup(kb).await {
               Ok(_) => {
                    // Устанавливаем новое состояние - ожидание суммы
                    {
                        let mut state_guard = state_storage.write().await;
                         if let Some(current_state @ UserState::AwaitingHedgeAssetSelection { .. }) = state_guard.get_mut(&chat_id) {
                             *current_state = UserState::AwaitingHedgeSum {
                                 symbol: ticker_input.clone(), // Сохраняем введенный тикер
                                 last_bot_message_id: Some(bot_msg_id.0),
                            };
                            info!("User state for {} set to AwaitingHedgeSum for {}", chat_id, ticker_input);
                        } else {
                             warn!("State changed for {} before setting AwaitingHedgeSum after ticker input", chat_id);
                        }
                    }
                }
                Err(e) => {
                    // Если не удалось отредактировать, пробуем отправить новое и показываем меню
                    error!("Failed to edit message {} to prompt sum: {}", bot_msg_id, e);
                    let _ = navigation::show_main_menu(&bot, chat_id, None).await;
                    { state_storage.write().await.insert(chat_id, UserState::None); } // Сбрасываем состояние
                }
            }
        } else {
             warn!("No previous bot message id found for chat_id {} to edit for sum prompt", chat_id);
             // Отправляем новое сообщение с запросом суммы
             let bot_msg = bot.send_message(chat_id, prompt_text).reply_markup(kb).await?;
              {
                 let mut state_guard = state_storage.write().await;
                 // Устанавливаем состояние ожидания суммы
                state_guard.insert(chat_id, UserState::AwaitingHedgeSum {
                     symbol: ticker_input.clone(),
                     last_bot_message_id: Some(bot_msg.id.0),
                 });
                 info!("User state for {} set to AwaitingHedgeSum for {}", chat_id, ticker_input);
               }
        }
    } else {
        // Тикер невалидный
        let error_text = format!("❌ Символ '{}' не найден или не подходит для хеджирования. Попробуйте другой.", ticker_input);
        if let Some(bot_msg_id_int) = previous_bot_message_id {
             let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
        } else {
             // Если вдруг нет предыдущего сообщения бота
             let _ = bot.send_message(chat_id, error_text).await;
        }
        // Состояние пользователя остается AwaitingHedgeAssetSelection
    }
    Ok(())
}


/// Обработчик ввода суммы хеджирования
pub async fn handle_sum_input(
    bot: Bot,
    msg: Message,
    state_storage: StateStorage,
    cfg: Arc<Config>,
) -> Result<()> {
     let chat_id = msg.chat.id;
    let message_id = msg.id; // ID сообщения пользователя
    let text = msg.text().unwrap_or("").trim();

    // Получаем символ и ID сообщения бота из предыдущего состояния
    let (symbol, previous_bot_message_id) = {
        let state_guard = state_storage.read().await;
        match state_guard.get(&chat_id) {
            // Убеждаемся, что пользователь в состоянии ожидания суммы
            Some(UserState::AwaitingHedgeSum { symbol, last_bot_message_id }) => (symbol.clone(), *last_bot_message_id),
            _ => {
                if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete unexpected sum message: {}", e); }
                return Ok(());
            }
        }
    };

     // Удаляем сообщение пользователя с суммой
     if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete user sum message: {}", e); }

    // Пытаемся распарсить сумму
    match text.parse::<f64>() {
         Ok(sum) if sum > 0.0 => {
             info!("User {} entered sum {} for hedge {}", chat_id, sum, symbol);
             // Запрашиваем волатильность
             let prompt_text = format!("Введите ожидаемую волатильность для {} {} (%):", sum, cfg.quote_currency);
             let kb = make_dialog_keyboard(); // Клавиатура с отменой

             if let Some(bot_msg_id_int) = previous_bot_message_id {
                 let bot_msg_id = MessageId(bot_msg_id_int);
                 match bot.edit_message_text(chat_id, bot_msg_id, prompt_text).reply_markup(kb).await {
                    Ok(_) => {
                         // Устанавливаем новое состояние - ожидание волатильности
                         {
                             let mut state_guard = state_storage.write().await;
                              if let Some(current_state @ UserState::AwaitingHedgeSum { .. }) = state_guard.get_mut(&chat_id) {
                                  *current_state = UserState::AwaitingHedgeVolatility {
                                      symbol: symbol.clone(), // Сохраняем символ
                                      sum,                    // Сохраняем сумму
                                      last_bot_message_id: Some(bot_msg_id.0), // Обновляем ID сообщения бота
                                 };
                                 info!("User state for {} set to AwaitingHedgeVolatility", chat_id);
                             } else {
                                 warn!("State changed for {} before setting AwaitingHedgeVolatility", chat_id);
                             }
                         }
                    }
                    Err(e) => {
                         error!("Failed to edit message {} to prompt volatility: {}", bot_msg_id, e);
                         let _ = navigation::show_main_menu(&bot, chat_id, None).await;
                         { state_storage.write().await.insert(chat_id, UserState::None); }
                    }
                 }
             } else {
                 warn!("No previous bot message id found for chat_id {} to edit for volatility prompt", chat_id);
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
             // Сумма не положительная
             warn!("User {} entered non-positive sum: {}", chat_id, text);
              if let Some(bot_msg_id_int) = previous_bot_message_id {
                   let error_text = format!("⚠️ Сумма должна быть положительной. Введите сумму {} для хеджирования {}:", cfg.quote_currency, symbol);
                   let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
              }
         }
         Err(_) => {
             // Неверный формат суммы
             warn!("User {} entered invalid sum format: {}", chat_id, text);
             if let Some(bot_msg_id_int) = previous_bot_message_id {
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
    msg: Message,
    exchange: Arc<E>,
    state_storage: StateStorage,
    // --- ИСПРАВЛЕНО: Добавили `_` ---
    _running_operations: RunningOperations, // Не используется здесь
    cfg: Arc<Config>,
    // --- ИСПРАВЛЕНО: Добавили `_` ---
    _db: Arc<Db>, // Не используется здесь
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let message_id = msg.id;
    let text = msg.text().unwrap_or("").trim();

    // Получаем символ и сумму из состояния
    let (symbol, sum, previous_bot_message_id) = {
        let state_guard = state_storage.read().await;
        match state_guard.get(&chat_id) {
            Some(UserState::AwaitingHedgeVolatility { symbol, sum, last_bot_message_id }) => (symbol.clone(), *sum, *last_bot_message_id),
            _ => {
                if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete unexpected volatility message: {}", e); }
                return Ok(());
            }
        }
    };

     // Удаляем сообщение пользователя
     if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed to delete user volatility message: {}", e); }

    // Парсим волатильность
    match text.trim_end_matches('%').trim().parse::<f64>() {
        Ok(volatility_percent) if volatility_percent >= 0.0 => {
            info!("User {} entered volatility {}% for hedge {} {}", chat_id, volatility_percent, sum, symbol);
            let volatility_fraction = volatility_percent / 100.0;

            // Создаем запрос хеджирования
            let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction };
            // Создаем экземпляр старого Hedger для расчета параметров
            let hedger = Hedger::new((*exchange).clone(), (*cfg).clone());

            let calc_indicator_text = "⏳ Расчет параметров хеджирования...";
            let mut bot_msg_id_opt = previous_bot_message_id.map(MessageId);

            // Показываем индикатор расчета
            if let Some(bot_msg_id) = bot_msg_id_opt {
                 let _ = bot.edit_message_text(chat_id, bot_msg_id, calc_indicator_text)
                    .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())) // Убираем кнопки
                    .await;
            } else {
                 bot_msg_id_opt = Some(bot.send_message(chat_id, calc_indicator_text).await?.id);
            }
            let bot_msg_id = bot_msg_id_opt.ok_or_else(|| anyhow!("Failed to get bot message ID for calculation status"))?;

            // Рассчитываем параметры
            match hedger.calculate_hedge_params(&hedge_request).await {
                Ok(params) => {
                    info!("Hedge parameters calculated for {}: {:?}", chat_id, params);
                    // Формируем текст подтверждения
                    let confirmation_text = format!(
                        "Подтвердите параметры хеджирования для {}:\n\n\
                         Сумма: {:.2} {}\n\
                         Волатильность: {:.1}%\n\
                         --- Расчет ---\n\
                         Спот (брутто): ~{:.8} {}\n\
                         Фьючерс (нетто): ~{:.8} {}\n\
                         Требуемое плечо: ~{:.2}x (Макс: {:.1}x)\n\n\
                         Запустить хеджирование?",
                        symbol, sum, cfg.quote_currency,
                        volatility_percent,
                        params.spot_order_qty, symbol,
                        params.fut_order_qty, symbol,
                        // Расчет плеча
                        (params.fut_order_qty * params.current_spot_price) / params.available_collateral.max(f64::EPSILON),
                        cfg.max_allowed_leverage
                    );
                    // Создаем клавиатуру подтверждения
                    let kb = make_hedge_confirmation_keyboard();
                    bot.edit_message_text(chat_id, bot_msg_id, confirmation_text).reply_markup(kb).await?;

                    // Устанавливаем состояние ожидания подтверждения
                    {
                        let mut state_guard = state_storage.write().await;
                        if let Some(current_state @ UserState::AwaitingHedgeVolatility { .. }) = state_guard.get_mut(&chat_id) {
                            *current_state = UserState::AwaitingHedgeConfirmation {
                                symbol: symbol.clone(),
                                sum,
                                volatility: volatility_fraction,
                                last_bot_message_id: Some(bot_msg_id.0),
                           };
                           info!("User state for {} set to AwaitingHedgeConfirmation", chat_id);
                       } else {
                            warn!("State changed for {} before setting AwaitingHedgeConfirmation", chat_id);
                       }
                    }
                }
                Err(e) => {
                    // Ошибка расчета параметров
                    error!("Hedge parameter calculation failed for {}: {}", chat_id, e);
                    let error_text = format!("❌ Ошибка расчета параметров: {}\nПопробуйте изменить сумму или волатильность.", e);
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("❌ Отмена", callback_data::CANCEL_DIALOG)
                    ]]);
                    bot.edit_message_text(chat_id, bot_msg_id, error_text).reply_markup(kb).await?;
                }
            }
        }
        Ok(_) => {
             // Волатильность не положительная
             warn!("User {} entered non-positive volatility: {}", chat_id, text);
              if let Some(bot_msg_id_int) = previous_bot_message_id {
                 let error_text = format!("⚠️ Волатильность должна быть не отрицательной (в %). Введите снова:");
                 let _ = bot.edit_message_text(chat_id, MessageId(bot_msg_id_int), error_text).await;
              }
        }
        Err(_) => {
             // Неверный формат волатильности
             warn!("User {} entered invalid volatility format: {}", chat_id, text);
             if let Some(bot_msg_id_int) = previous_bot_message_id {
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
    let query_id = q.id.clone(); // Клонируем ID для возможного использования в конце

    if let (Some(data), Some(msg_ref)) = (q.data.as_deref(), q.message.as_ref()) {
        let chat_id = msg_ref.chat().id;
        let message_id = msg_ref.id();

        if let Some(payload) = data.strip_prefix(callback_data::PREFIX_HEDGE_CONFIRM) {
            if payload == "yes" {
                // --- q.message перемещается сюда для передачи в спавнер ---
                if let Some(msg_owned) = q.message {
                    // --- Логика выбора стратегии ---
                    let chosen_strategy = cfg.hedge_strategy_default;
                    info!("User {} confirmed hedge operation. Chosen strategy: {:?}", chat_id, chosen_strategy);

                    // --- Получаем данные из состояния ---
                    let (symbol, sum, volatility_fraction) = {
                        let state_guard = state_storage.read().await;
                        match state_guard.get(&chat_id) {
                            Some(UserState::AwaitingHedgeConfirmation { symbol, sum, volatility, .. }) => (symbol.clone(), *sum, *volatility),
                            _ => {
                                warn!("User {} confirmed hedge but was in wrong state", chat_id);
                                bot.answer_callback_query(query_id).text("Состояние изменилось, начните заново.").show_alert(true).await?;
                                let _ = navigation::show_main_menu(&bot, chat_id, Some(message_id)).await;
                                { state_storage.write().await.insert(chat_id, UserState::None); }
                                return Ok(());
                            }
                        }
                    };
                    // --- Сбрасываем state ПОСЛЕ извлечения данных ---
                    { state_storage.write().await.insert(chat_id, UserState::None); }

                    // --- Запуск выбранной стратегии ---
                    match chosen_strategy {
                        HedgeStrategy::Sequential => {
                             let waiting_text = format!("⏳ Запуск хеджирования (Sequential) для {}...", symbol);
                             bot.edit_message_text(chat_id, message_id, waiting_text)
                                .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())).await?;

                             let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction };
                             let hedger = Hedger::new((*exchange).clone(), (*cfg).clone());

                             match hedger.calculate_hedge_params(&hedge_request).await {
                                 Ok(params) => {
                                     info!("Sequential hedge params OK for {}: {:?}", chat_id, params);
                                     spawn_sequential_hedge_task(
                                         bot.clone(), exchange.clone(), cfg.clone(), db.clone(),
                                         running_operations.clone(), chat_id, params, sum,
                                         volatility_fraction * 100.0, msg_owned,
                                     ).await;
                                     // Успешный спавн, отвечаем на колбэк
                                     bot.answer_callback_query(query_id).await?;
                                 }
                                 Err(e) => {
                                     error!("Hedge parameter calculation failed just before execution for {}: {}", chat_id, e);
                                     let error_text = format!("❌ Ошибка расчета параметров перед запуском: {}\nПопробуйте снова.", e);
                                     let _ = bot.edit_message_text(chat_id, message_id, error_text)
                                              .reply_markup(navigation::make_main_menu_keyboard())
                                              .await;
                                     bot.answer_callback_query(query_id).await?;
                                     return Ok(());
                                 }
                             }
                        }
                        HedgeStrategy::WebsocketChunks => {
                            let waiting_text = format!("⏳ Запуск хеджирования (WebSocket) для {}...", symbol);
                             bot.edit_message_text(chat_id, message_id, waiting_text)
                                .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())).await?;

                             let spawn_result = spawn_ws_hedge_task(
                                bot.clone(), // Клонируем бот
                                exchange.clone(),
                                cfg.clone(),
                                db.clone(),
                                running_operations.clone(),
                                chat_id,
                                HedgeRequest { sum, symbol: symbol.clone(), volatility: volatility_fraction },
                                msg_owned,
                             ).await;

                             if let Err(spawn_err) = spawn_result {
                                 error!("Failed to spawn WS hedge task for chat_id {}: {}", chat_id, spawn_err);
                                 // Сообщение об ошибке уже отправлено в spawn_ws_hedge_task
                                 bot.answer_callback_query(query_id).await?;
                                 return Ok(());
                             }
                             // Успешный спавн, отвечаем на колбэк
                             bot.answer_callback_query(query_id).await?;
                        }
                    }
                } else {
                    warn!("CallbackQuery missing message on 'yes' confirmation for {}", query_id);
                    bot.answer_callback_query(query_id).await?;
                }
            } else if payload == "no" {
                info!("User {} cancelled hedge at confirmation", chat_id);
                // --- ИСПРАВЛЕНО: Клонируем bot ---
                navigation::handle_cancel_dialog(bot.clone(), chat_id, message_id, state_storage).await?;
                bot.answer_callback_query(query_id).await?; // Отвечаем после handle_cancel_dialog
            } else {
                warn!("Invalid payload for hedge confirmation callback: {}", payload);
                bot.answer_callback_query(query_id).await?;
            }
        } else if data == callback_data::CANCEL_DIALOG {
             info!("User cancelled hedge dialog via cancel button");
             // --- ИСПРАВЛЕНО: Клонируем bot ---
             navigation::handle_cancel_dialog(bot.clone(), chat_id, message_id, state_storage).await?;
             bot.answer_callback_query(query_id).await?; // Отвечаем после handle_cancel_dialog
        } else {
             warn!("Invalid callback data format for hedge confirmation prefix: {}", data);
             bot.answer_callback_query(query_id).await?;
        }
    } else {
        warn!("CallbackQuery missing data or message in handle_hedge_confirm_callback");
        bot.answer_callback_query(query_id).await?;
    }
    // --- ИСПРАВЛЕНО: Удален финальный answer_callback_query ---
    // let _ = bot.answer_callback_query(query_id).await;
    Ok(())
}