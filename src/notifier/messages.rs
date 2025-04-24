// src/notifier/messages.rs
use crate::config::Config;
use crate::exchange::Exchange;
use crate::storage::{Db, insert_hedge_operation};
use super::{UserState, StateStorage, RunningHedges, RunningHedgeInfo};
use tokio::sync::Mutex as TokioMutex;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{Message, MessageId, InlineKeyboardButton, InlineKeyboardMarkup, ChatId};
use tracing::{warn, error, info};
// --- ИЗМЕНЕНО: Убран неиспользуемый UnhedgeRequest ---
use crate::models::HedgeRequest;
// --- КОНЕЦ ИЗМЕНЕНИЯ ---
use crate::hedger::{Hedger, HedgeProgressUpdate, HedgeProgressCallback};
use futures::future::FutureExt;

// Вспомогательная функция для "чистки" чата
async fn cleanup_chat(bot: &Bot, chat_id: ChatId, user_msg_id: MessageId, bot_msg_id: Option<i32>) {
    if let Some(id_int) = bot_msg_id {
        if let Err(e) = bot.delete_message(chat_id, MessageId(id_int)).await {
            warn!("Failed to delete previous bot message {}: {}", id_int, e);
        }
    }
    if let Err(e) = bot.delete_message(chat_id, user_msg_id).await {
        warn!("Failed to delete user message {}: {}", user_msg_id, e);
    }
}

// Обработчик текстовых сообщений
pub async fn handle_message<E>(
    bot: Bot,
    msg: Message,
    state_storage: StateStorage,
    exchange: E,
    cfg: Config,
    running_hedges: RunningHedges,
    db: &Db,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let message_id = msg.id;
    let text = msg.text().unwrap_or("").trim();
    if text.is_empty() { if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed delete empty msg: {}", e); } return Ok(()); }

    let user_state = { state_storage.read().expect("Lock failed").get(&chat_id).cloned() };

    match user_state {
        // --- Обработка ввода суммы для ХЕДЖИРОВАНИЯ ---
        Some(UserState::AwaitingSum { symbol, last_bot_message_id }) => {
             if let Ok(sum) = text.parse::<f64>() {
                 if sum <= 0.0 { bot.send_message(chat_id, "⚠️ Сумма должна быть положительной.").await?; return Ok(()); }
                 cleanup_chat(&bot, chat_id, message_id, last_bot_message_id).await;
                 let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),]]);
                 let bot_msg = bot.send_message(chat_id, format!("Введите ожидаемую волатильность для хеджирования {} {} (%):", sum, cfg.quote_currency)).reply_markup(kb).await?;
                 let mut message_to_delete_if_state_changed: Option<MessageId> = None;
                 { let mut state = state_storage.write().expect("Lock failed"); if let Some(current_state @ UserState::AwaitingSum { .. }) = state.get_mut(&chat_id) { *current_state = UserState::AwaitingVolatility { symbol: symbol.clone(), sum, last_bot_message_id: Some(bot_msg.id.0) }; info!("State -> AwaitingVolatility"); } else { warn!("State changed unexpectedly"); message_to_delete_if_state_changed = Some(bot_msg.id); } }
                 if let Some(delete_id) = message_to_delete_if_state_changed { if let Err(e) = bot.delete_message(chat_id, delete_id).await { warn!("Failed delete obsolete msg: {}", e); } }
             } else { bot.send_message(chat_id, "⚠️ Неверный формат суммы.").await?; }
        }

        // --- Обработка ввода волатильности для ХЕДЖИРОВАНИЯ ---
        Some(UserState::AwaitingVolatility { symbol, sum, last_bot_message_id }) => {
             if let Ok(vol_raw) = text.trim_end_matches('%').parse::<f64>() {
                 if vol_raw < 0.0 { bot.send_message(chat_id, "⚠️ Волатильность не может быть отрицательной.").await?; return Ok(()); }
                 let vol = vol_raw / 100.0;
                 cleanup_chat(&bot, chat_id, message_id, last_bot_message_id).await;
                 { let mut state = state_storage.write().expect("Lock failed"); state.insert(chat_id, UserState::None); }
                 { let hedges_guard = running_hedges.lock().await; if hedges_guard.contains_key(&(chat_id, symbol.clone())) { warn!("Hedge already running"); bot.send_message(chat_id, format!("⚠️ Хеджирование для {} уже запущено.", symbol)).await?; return Ok(()); } }

                 let hedger = Hedger::new(exchange.clone(), cfg.clone());
                 let hedge_request = HedgeRequest { sum, symbol: symbol.clone(), volatility: vol };
                 info!("Starting hedge calculation for chat_id: {}, request: {:?}", chat_id, hedge_request);
                 let hedge_params_result = hedger.calculate_hedge_params(&hedge_request).await;
                 let cancel_callback_data = format!("cancel_hedge_active_{}", symbol);
                 let cancel_button = InlineKeyboardButton::callback("❌ Отмена", cancel_callback_data);
                 let initial_kb = InlineKeyboardMarkup::new(vec![vec![cancel_button.clone()]]);
                 let waiting_msg = match hedge_params_result { Ok(ref params) => bot.send_message(chat_id, format!("⏳ Запускаю хеджирование {} {} ({})... \nРыночная цена: {:.2}\nОжидаемая цена покупки: {:.2}", sum, cfg.quote_currency, params.symbol, params.current_spot_price, params.initial_limit_price)).reply_markup(initial_kb.clone()).await?, Err(ref e) => { error!("Hedge calculation failed: {}", e); bot.send_message(chat_id, format!("❌ Ошибка расчета: {}", e)).await?; return Ok(()); } };

                 if let Ok(params) = hedge_params_result {
                     info!("Hedge calculation successful. Running hedge execution for chat_id: {}", chat_id);
                     let operation_id = match insert_hedge_operation(db, chat_id.0, &params.symbol, &cfg.quote_currency, sum, vol, params.spot_order_qty, params.fut_order_qty).await { Ok(id)=>{info!("Created DB record id: {}",id);id} Err(e)=>{error!("DB insert failed: {}",e); bot.edit_message_text(chat_id, waiting_msg.id, format!("❌ Критическая ошибка БД: {}", e)).reply_markup(InlineKeyboardMarkup::new(vec![vec![]])).await?; return Ok(());}};
                     let current_order_id_storage = Arc::new(TokioMutex::new(None::<String>)); let total_filled_qty_storage = Arc::new(TokioMutex::new(0.0f64));
                     let bot_for_task = bot.clone(); let waiting_msg_id = waiting_msg.id; let initial_sum_for_callback = sum; let initial_symbol = params.symbol.clone(); let symbol_for_remove = initial_symbol.clone(); let symbol_for_messages = initial_symbol.clone(); let running_hedges_clone = running_hedges.clone(); let hedger_clone = hedger.clone(); let vol_raw_clone = vol_raw; let current_order_id_storage_clone = current_order_id_storage.clone(); let total_filled_qty_storage_clone = total_filled_qty_storage.clone(); let cfg_clone = cfg.clone(); let db_clone = db.clone(); let exchange_clone = exchange.clone();

                     let task = tokio::spawn(async move {
                         let bot_for_callback = bot_for_task.clone(); let symbol_for_button_callback = symbol_for_messages.clone(); let qc_for_callback_inner = cfg_clone.quote_currency.clone();
                         let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| { let bot = bot_for_callback.clone(); let msg_id = waiting_msg_id; let chat_id = chat_id; let sum = initial_sum_for_callback; let symbol = symbol_for_button_callback.clone(); let qc = qc_for_callback_inner.clone(); async move { let cancel_callback_data = format!("cancel_hedge_active_{}", symbol); let cancel_button = InlineKeyboardButton::callback("❌ Отмена", cancel_callback_data); let kb = InlineKeyboardMarkup::new(vec![vec![cancel_button]]); let status_text = if update.is_replacement { "(Ордер переставлен)" } else { "" }; let filled_percent = if update.target_qty > 0.0 { (update.filled_qty / update.target_qty) * 100.0 } else { 0.0 }; let fill_status = format!("\nИсполнено: {:.6}/{:.6} ({:.1}%)", update.filled_qty, update.target_qty, filled_percent); let text = format!("⏳ Хеджирование {} {} ({}) в процессе...\nРыночная цена: {:.2}\nОрдер на покупку: {:.2} {}{}", sum, qc, symbol, update.current_spot_price, update.new_limit_price, status_text, fill_status ); if let Err(e) = bot.edit_message_text(chat_id, msg_id, text).reply_markup(kb).await { if !e.to_string().contains("not modified") { warn!("Failed progress update: {}", e); } } Ok(()) }.boxed() });
                         let result = hedger_clone.run_hedge( params, progress_callback, current_order_id_storage_clone, total_filled_qty_storage_clone, operation_id, &db_clone ).await;
                         let is_cancelled = result.is_err() && result.as_ref().err().map_or(false, |e| e.to_string().contains("cancelled"));
                         if !is_cancelled { running_hedges_clone.lock().await.remove(&(chat_id, symbol_for_remove)); info!("Removed running hedge info for chat_id: {}, symbol: {}", chat_id, symbol_for_messages); }
                         match result {
                             Ok((spot_qty_gross, fut_qty_net, final_spot_value_gross)) => { info!( "Hedge OK: {}, Spot Gross: {}, Fut Net: {}, Value: {:.2}", chat_id, spot_qty_gross, fut_qty_net, final_spot_value_gross ); let actual_net_spot_balance = match exchange_clone.get_balance(&symbol_for_messages).await { Ok(b)=>{info!("Fetched actual balance: {}", b.free); b.free} Err(e)=>{warn!("Failed get balance after hedge: {}. Using gross.", e); spot_qty_gross}}; let _ = bot_for_task.edit_message_text( chat_id, waiting_msg_id, format!( "✅ Хеджирование ID:{} ~{:.2} {} ({}) при V={:.1}% завершено:\n\n🟢 Спот баланс (нетто): {:.8}\n🔴 Фьюч продано (нетто): {:.8}", operation_id, final_spot_value_gross, cfg_clone.quote_currency, symbol_for_messages, vol_raw_clone, actual_net_spot_balance, fut_qty_net, ) ).reply_markup(InlineKeyboardMarkup::new(vec![vec![]])).await; }
                             Err(e) => { if is_cancelled { info!("Hedge task cancelled op_id: {}", operation_id); } else { error!("Hedge failed op_id: {}: {}", operation_id, e); let _ = bot_for_task.edit_message_text( chat_id, waiting_msg_id, format!("❌ Ошибка хеджирования ID:{}: {}", operation_id, e) ).reply_markup(InlineKeyboardMarkup::new(vec![vec![]])).await; } }
                         }
                     });
                     { let mut hedges_guard = running_hedges.lock().await; hedges_guard.insert((chat_id, initial_symbol.clone()), RunningHedgeInfo { handle: task.abort_handle(), current_order_id: current_order_id_storage, total_filled_qty: total_filled_qty_storage, symbol: initial_symbol.clone(), bot_message_id: waiting_msg.id.0, operation_id }); info!("Stored running hedge info op_id: {}", operation_id); }
                 }
             } else { bot.send_message(chat_id, "⚠️ Неверный формат волатильности.").await?; }
        }

        // --- Обработка других состояний (когда пользователь пишет текст вместо нажатия кнопки) ---
        Some(UserState::AwaitingAssetSelection { .. }) | Some(UserState::AwaitingUnhedgeSelection { .. }) => {
            if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed delete unexpected msg: {}", e); }
            info!("User {} sent text while awaiting button press.", chat_id);
        }

        // --- Нет активного состояния ---
        None | Some(UserState::None) => {
            if let Err(e) = bot.delete_message(chat_id, message_id).await { warn!("Failed delete unexpected msg: {}", e); }
        }
    } // Конец match user_state

    Ok(())
}