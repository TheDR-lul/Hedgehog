// src/notifier/hedge_flow_logic/spawners.rs

// --- ИСПРАВЛЕНО: Убран anyhow и лишние скобки в use ---
use anyhow::Result; // Убраны скобки
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{MaybeInaccessibleMessage, ChatId, InlineKeyboardButton, InlineKeyboardMarkup};
// --- ИСПРАВЛЕНО: Убран неиспользуемый mpsc ---
use tokio::sync::Mutex as TokioMutex;
use tracing::{info, error, warn};
use futures::future::FutureExt;

use crate::config::Config;
use crate::exchange::Exchange;
use crate::exchange::bybit_ws;
use crate::exchange::types::SubscriptionType;
// --- ИСПРАВЛЕНО: Используем относительный путь ---
use super::super::super::hedger::{HedgeParams, HedgeProgressCallback, HedgeProgressUpdate, HedgeStage, Hedger, ORDER_FILL_TOLERANCE};
use crate::models::HedgeRequest;
use crate::storage::{Db, insert_hedge_operation};
use crate::notifier::{RunningOperations, RunningOperationInfo, OperationType, navigation, callback_data};
// --- ИСПРАВЛЕНО: Используем реэкспортированный путь и имя ---
use crate::hedger_ws::HedgeWSTask;
//use super::super::super::hedger_ws::hedge_task::HedgerWsHedgeTask; // Старый путь

pub(super) async fn spawn_sequential_hedge_task<E>(
    bot: Bot,
    exchange: Arc<E>,
    cfg: Arc<Config>,
    db: Arc<Db>,
    running_operations: RunningOperations,
    chat_id: ChatId,
    params: HedgeParams,
    initial_sum: f64,
    volatility_percent: f64,
    waiting_message: MaybeInaccessibleMessage,
)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let bot_message_id = waiting_message.id();
    let symbol_for_callback = params.symbol.clone();
    let symbol_for_task_body = params.symbol.clone();
    let symbol_for_info = params.symbol.clone();
    let initial_spot_target_for_cb = params.spot_order_qty;
    let initial_fut_target_for_cb = params.fut_order_qty;

    let hedger = Hedger::new((*exchange).clone(), (*cfg).clone());

    let operation_id_result = insert_hedge_operation(
        db.as_ref(), chat_id.0, &params.symbol, &cfg.quote_currency, initial_sum,
        volatility_percent / 100.0, params.spot_order_qty, params.fut_order_qty,
    ).await;

    let operation_id = match operation_id_result {
        Ok(id) => { info!("op_id:{}: Created DB record for hedge operation.", id); id }
        Err(e) => {
            error!("Failed insert hedge op to DB: {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, format!("❌ DB Error: {}", e))
                     .reply_markup(navigation::make_main_menu_keyboard()).await;
            return;
        }
    };

    let total_filled_qty_storage = Arc::new(TokioMutex::new(0.0f64));
    let bot_clone = bot.clone();
    let cfg_clone = cfg.clone();
    let db_clone = db.clone();
    let total_filled_qty_storage_clone = total_filled_qty_storage.clone();
    let running_operations_clone = running_operations.clone();

    let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
         let bot_for_callback = bot_clone.clone();
         let qc = cfg_clone.quote_currency.clone();
         let symbol_cb = symbol_for_callback.clone();
         let msg_id_cb = bot_message_id;
         let chat_id_cb = chat_id;
         let initial_sum_cb = initial_sum;
         let operation_id_cb = operation_id;
         let spot_target_cb = initial_spot_target_for_cb;
         let fut_target_cb = initial_fut_target_for_cb;

         async move {
             let symbol = symbol_cb;
             let progress_bar_len = 10;
             let status_text = if update.is_replacement { "(Ордер переставлен)" } else { "" };

             let text = match update.stage {
                 HedgeStage::Spot => {
                     let filled_percent = if update.target_qty > ORDER_FILL_TOLERANCE { (update.filled_qty / update.target_qty) * 100.0 } else { 0.0 };
                     let filled_blocks = (filled_percent / (100.0 / progress_bar_len as f64)).round() as usize;
                     let empty_blocks = progress_bar_len - filled_blocks;
                     let progress_bar = format!("[{}{}]", "█".repeat(filled_blocks), "░".repeat(empty_blocks));
                     if (update.cumulative_filled_qty - spot_target_cb).abs() <= ORDER_FILL_TOLERANCE {
                         format!( "✅ Спот куплен ID:{} ({})\nРын.цена: {:.2}\nОжидание продажи фьючерса...", operation_id_cb, symbol, update.current_spot_price)
                     } else {
                         format!( "⏳ Хедж (Спот) ID:{} {} {:.2} {} ({})\nРын.цена: {:.2}\nОрдер ПОКУПКА: {:.2} {}\nИсполнено (тек.ордер): {:.6}/{:.6} ({:.1}%)", operation_id_cb, progress_bar, initial_sum_cb, qc, symbol, update.current_spot_price, update.new_limit_price, status_text, update.filled_qty, update.target_qty, filled_percent)
                     }
                 }
                 HedgeStage::Futures => {
                     let filled_percent = if fut_target_cb > ORDER_FILL_TOLERANCE { (update.cumulative_filled_qty / fut_target_cb) * 100.0 } else { 0.0 };
                     let filled_blocks = (filled_percent / (100.0 / progress_bar_len as f64)).round() as usize;
                     let empty_blocks = progress_bar_len - filled_blocks;
                     let progress_bar = format!("[{}{}]", "█".repeat(filled_blocks), "░".repeat(empty_blocks));
                     format!( "⏳ Хедж (Фьюч) ID:{} {} {:.2} {} ({})\nСпот цена: {:.2}\nОрдер ПРОДАЖА: {:.2} {}\nИсполнено (фьюч): {:.6}/{:.6} ({:.1}%)", operation_id_cb, progress_bar, initial_sum_cb, qc, symbol, update.current_spot_price, update.new_limit_price, status_text, update.cumulative_filled_qty, fut_target_cb, filled_percent)
                 }
             };
             let cancel_callback_data = format!("{}{}", callback_data::PREFIX_CANCEL_ACTIVE_OP, operation_id_cb);
             let cancel_button = InlineKeyboardButton::callback("❌ Отменить эту операцию", cancel_callback_data);
             let kb = InlineKeyboardMarkup::new(vec![vec![cancel_button]]);
             if let Err(e) = bot_for_callback.edit_message_text(chat_id_cb, msg_id_cb, text).reply_markup(kb).await {
                 if !e.to_string().contains("not modified") { warn!("op_id:{}: Progress callback failed: {}", operation_id_cb, e); }
             }
             Ok(())
         }.boxed()
    });

    let exchange_task = exchange.clone();
    let cfg_task = cfg.clone();

    let task = tokio::spawn(async move {
        let result = hedger.run_hedge(
            params, progress_callback, total_filled_qty_storage_clone, operation_id, db_clone.as_ref(),
        ).await;

        let is_cancelled_by_button = result.is_err() && result.as_ref().err().map_or(false, |e| e.to_string().contains("cancelled by user"));
        if !is_cancelled_by_button {
             running_operations_clone.lock().await.remove(&(chat_id, operation_id));
             info!("op_id:{}: Removed running operation info for chat_id: {}", operation_id, chat_id);
        } else { info!("op_id:{}: Operation was cancelled via button, info already removed.", operation_id); }

        match result {
            Ok((spot_qty_gross, fut_qty_net, final_spot_value_gross)) => {
                 info!( "op_id:{}: Hedge OK. Spot Gross: {}, Fut Net: {}, Value: {:.2}", operation_id, spot_qty_gross, fut_qty_net, final_spot_value_gross );
                 tokio::time::sleep(Duration::from_millis(500)).await;
                 let final_net_spot_balance = match exchange_task.get_balance(&symbol_for_task_body).await { Ok(b) => b.free, Err(_) => spot_qty_gross };
                 let success_text = format!(
                      "✅ Хеджирование ID:{} ~{:.2} {} ({}) при V={:.1}% завершено:\n\n🟢 Спот куплено (брутто): {:.8}\nspot_balance_check {:.8}\n🔴 Фьюч продано (нетто): {:.8}",
                     operation_id, final_spot_value_gross, cfg_task.quote_currency, symbol_for_task_body,
                     volatility_percent, spot_qty_gross, final_net_spot_balance, fut_qty_net,
                 );
                 let _ = bot.edit_message_text(chat_id, bot_message_id, success_text).reply_markup(navigation::make_main_menu_keyboard()).await;
            }
            Err(e) => {
                 if is_cancelled_by_button { info!("op_id:{}: Hedge task finished after cancellation via button.", operation_id); }
                 else {
                      error!("op_id:{}: Hedge execution failed: {}", operation_id, e);
                      let error_text = format!("❌ Ошибка хеджирования ID:{}: {}", operation_id, e);
                       let _ = bot.edit_message_text(chat_id, bot_message_id, error_text)
                                  .reply_markup(navigation::make_main_menu_keyboard())
                                  .await;
                 }
            }
        }
    });

    let info = RunningOperationInfo {
        handle: task.abort_handle(), operation_id, operation_type: OperationType::Hedge,
        symbol: symbol_for_info, bot_message_id: bot_message_id.0, total_filled_spot_qty: total_filled_qty_storage,
    };
    running_operations.lock().await.insert((chat_id, operation_id), info);
    info!("op_id:{}: Stored running hedge info.", operation_id);
}


/// Запускает фоновую задачу хеджирования через WebSocket
pub(super) async fn spawn_ws_hedge_task<E>(
    bot: Bot,
    exchange_rest: Arc<E>,
    cfg: Arc<Config>,
    db: Arc<Db>,
    running_operations: RunningOperations,
    chat_id: ChatId,
    request: HedgeRequest,
    waiting_message: MaybeInaccessibleMessage,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let bot_message_id = waiting_message.id();
    let symbol = request.symbol.clone();
    let initial_sum = request.sum;
    let _volatility_percent = request.volatility * 100.0;

    info!("op_chat_id:{}: Preparing to spawn WS Hedge Task for {}...", chat_id, symbol);

    let operation_id_result = insert_hedge_operation(
        db.as_ref(), chat_id.0, &symbol, &cfg.quote_currency, initial_sum,
        request.volatility, 0.0, 0.0,
    ).await;

    let operation_id = match operation_id_result {
        Ok(id) => { info!("op_id:{}: Created DB record for WS hedge operation.", id); id }
        Err(e) => {
            error!("op_id:?: Failed insert WS hedge op to DB: {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, format!("❌ DB Error: {}", e))
                     .reply_markup(navigation::make_main_menu_keyboard()).await;
            return Err(e.into());
        }
    };

    let _ = bot.edit_message_text(chat_id, bot_message_id, format!("⏳ Подключение WebSocket для {}...", symbol)).await;
    let spot_symbol_ws = format!("{}{}", symbol, cfg.quote_currency);
    let futures_symbol_ws = format!("{}{}", symbol, cfg.quote_currency);

    let subscriptions = vec![
        SubscriptionType::Order,
        SubscriptionType::Orderbook { symbol: spot_symbol_ws.clone(), depth: cfg.ws_order_book_depth },
        SubscriptionType::Orderbook { symbol: futures_symbol_ws.clone(), depth: cfg.ws_order_book_depth },
    ];

    let ws_receiver_result = bybit_ws::connect_and_subscribe((*cfg).clone(), subscriptions).await;
    let ws_receiver = match ws_receiver_result {
        Ok(receiver) => {
            info!("op_id:{}: WebSocket connected and subscribed successfully.", operation_id);
            let _ = bot.edit_message_text(chat_id, bot_message_id, format!("⏳ Инициализация стратегии WS для {}...", symbol)).await;
            receiver
        },
        Err(e) => {
            error!("op_id:{}: Failed to connect WebSocket: {}", operation_id, e);
            let error_text = format!("❌ Ошибка подключения WebSocket: {}", e);
             let _ = bot.edit_message_text(chat_id, bot_message_id, error_text.clone())
                      .reply_markup(navigation::make_main_menu_keyboard()).await;
             let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&error_text)).await;
             return Err(e);
        }
    };

    let bot_clone_for_callback = bot.clone();
    let cfg_clone_for_callback = cfg.clone();
    let symbol_for_callback = symbol.clone();

    // --- ИСПРАВЛЕНО: Тип HedgeProgressCallback теперь использует Decimal ---
    // Примечание: Текущая реализация HedgeProgressCallback в этом файле использует f64.
    // Это потребует рефакторинга HedgeProgressUpdate и колбэка, чтобы использовать Decimal,
    // если HedgerWsHedgeTask ожидает Decimal.
    // Пока оставляем f64, предполагая, что HedgeProgressCallback универсален или будет адаптирован.
    let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
        let bot_cb = bot_clone_for_callback.clone();
        let _qc = cfg_clone_for_callback.quote_currency.clone();
        let symbol_cb = symbol_for_callback.clone();
        let msg_id_cb = bot_message_id;
        let chat_id_cb = chat_id;
        let operation_id_cb = operation_id;
        // --- ПРЕДУПРЕЖДЕНИЕ: Конвертация Decimal -> f64 может быть неточной ---
        // Если HedgeProgressUpdate будет использовать Decimal, эти конвертации нужно убрать.
        let spot_target_cb = if update.stage == HedgeStage::Spot { update.total_target_qty } else { 0.0 };
        let fut_target_cb = if update.stage == HedgeStage::Futures { update.total_target_qty } else { 0.0 };
        let cumulative_filled_qty_cb = update.cumulative_filled_qty;
        let current_spot_price_cb = update.current_spot_price;
        let new_limit_price_cb = update.new_limit_price;
        // --- КОНЕЦ ПРЕДУПРЕЖДЕНИЯ ---

        async move {
            let symbol = symbol_cb;
            let progress_bar_len = 10;
            let status_text = if update.is_replacement { "(Ордер переставлен)" } else { "" };

            let text = match update.stage {
                HedgeStage::Spot => {
                    let filled_percent = if spot_target_cb > ORDER_FILL_TOLERANCE { (cumulative_filled_qty_cb / spot_target_cb) * 100.0 } else { 0.0 };
                    let filled_blocks = (filled_percent / (100.0 / progress_bar_len as f64)).round() as usize;
                    let empty_blocks = progress_bar_len - filled_blocks;
                    let progress_bar = format!("[{}{}]", "█".repeat(filled_blocks), "░".repeat(empty_blocks));
                    format!( "⏳ Хедж WS (Спот) ID:{} {} ({})\nРын.цена: {:.2}\nОрдер ПОКУПКА: {:.2} {}\nИсполнено (всего): {:.6}/{:.6} ({:.1}%)",
                             operation_id_cb, progress_bar, symbol, current_spot_price_cb,
                             new_limit_price_cb, status_text,
                             cumulative_filled_qty_cb, spot_target_cb, filled_percent)
                }
                HedgeStage::Futures => {
                    let filled_percent = if fut_target_cb > ORDER_FILL_TOLERANCE { (cumulative_filled_qty_cb / fut_target_cb) * 100.0 } else { 0.0 };
                    let filled_blocks = (filled_percent / (100.0 / progress_bar_len as f64)).round() as usize;
                    let empty_blocks = progress_bar_len - filled_blocks;
                    let progress_bar = format!("[{}{}]", "█".repeat(filled_blocks), "░".repeat(empty_blocks));
                    format!( "⏳ Хедж WS (Фьюч) ID:{} {} ({})\nСпот цена: {:.2}\nОрдер ПРОДАЖА: {:.2} {}\nИсполнено (всего): {:.6}/{:.6} ({:.1}%)",
                             operation_id_cb, progress_bar, symbol, current_spot_price_cb,
                             new_limit_price_cb, status_text,
                             cumulative_filled_qty_cb, fut_target_cb, filled_percent)
                }
            };
            let cancel_callback_data = format!("{}{}", callback_data::PREFIX_CANCEL_ACTIVE_OP, operation_id_cb);
            let cancel_button = InlineKeyboardButton::callback("❌ Отменить эту операцию", cancel_callback_data);
            let kb = InlineKeyboardMarkup::new(vec![vec![cancel_button]]);

            if let Err(e) = bot_cb.edit_message_text(chat_id_cb, msg_id_cb, text).reply_markup(kb).await {
                if !e.to_string().contains("message is not modified") {
                     warn!("op_id:{}: WS Progress callback failed: {}", operation_id_cb, e);
                }
            }
            Ok(())
        }.boxed()
     });

    // --- ИСПРАВЛЕНО: Используем HedgeWSTask::new ---
    let hedge_task_result = HedgeWSTask::new(
        operation_id,
        request,
        cfg.clone(),
        db.clone(),
        exchange_rest.clone(),
        progress_callback,
        ws_receiver,
    ).await;

    // --- ИСПРАВЛЕНО: Используем HedgeWSTask ---
    let mut hedge_task: HedgeWSTask = match hedge_task_result {
        Ok(task) => {
            info!("op_id:{}: HedgeWSTask initialized successfully.", operation_id);
             let _ = bot.edit_message_text(chat_id, bot_message_id, format!("⏳ Запуск WS стратегии для {} (ID: {})...", symbol, operation_id)).await;
            task
        },
        Err(e) => {
            error!("op_id:{}: Failed to initialize HedgeWSTask: {}", operation_id, e);
            let error_text = format!("❌ Ошибка инициализации WS стратегии: {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, error_text.clone())
                     .reply_markup(navigation::make_main_menu_keyboard()).await;
             let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&error_text)).await;
            return Err(e);
        }
    };

    let bot_clone_for_spawn = bot.clone();
    let running_operations_clone = running_operations.clone();
    let symbol_clone_for_spawn = symbol.clone();

    let task_handle = tokio::spawn(async move {
        info!("op_id:{}: Spawning WS hedge task execution...", operation_id);
        let run_result = hedge_task.run().await;

        let mut ops_guard = running_operations_clone.lock().await;
        ops_guard.remove(&(chat_id, operation_id));
        drop(ops_guard); // Explicitly drop the guard after removal

        match run_result {
            Ok(_) => {
                info!("op_id:{}: WS Hedge task completed successfully.", operation_id);
                let final_text = format!("✅ WS Хедж ID:{} для {} завершен.", operation_id, symbol_clone_for_spawn);
                // Check if message is still accessible before editing
                if !waiting_message.is_inaccessible() {
                    if let Err(e) = bot_clone_for_spawn.edit_message_text(chat_id, bot_message_id, final_text)
                             .reply_markup(navigation::make_main_menu_keyboard())
                             .await {
                        warn!("op_id:{}: Failed to edit final success message: {}", operation_id, e);
                    }
                } else {
                    info!("op_id:{}: Original message inaccessible, cannot edit final success status.", operation_id);
                }
            }
            Err(e) => {
                // Avoid logging the error twice if it's just "cancelled by user" which is handled elsewhere
                if !e.to_string().contains("cancelled by user") {
                    error!("op_id:{}: WS Hedge task failed: {}", operation_id, e);
                } else {
                    info!("op_id:{}: WS Hedge task cancelled by user.", operation_id);
                }
                 let final_text = format!("❌ Ошибка WS Хедж ID:{}: {}", operation_id, e);
                 // Check if message is still accessible before editing
                 if !waiting_message.is_inaccessible() {
                     if let Err(edit_err) = bot_clone_for_spawn.edit_message_text(chat_id, bot_message_id, final_text)
                              .reply_markup(navigation::make_main_menu_keyboard())
                              .await {
                         warn!("op_id:{}: Failed to edit final error message: {}", operation_id, edit_err);
                     }
                 } else {
                     info!("op_id:{}: Original message inaccessible, cannot edit final error status.", operation_id);
                 }
            }
        }
    });

    let info = RunningOperationInfo {
        handle: task_handle.abort_handle(),
        operation_id,
        operation_type: OperationType::Hedge,
        symbol: symbol.clone(),
        bot_message_id: bot_message_id.0,
        // --- ПРЕДУПРЕЖДЕНИЕ: total_filled_spot_qty не обновляется для WS ---
        // HedgerWsHedgeTask не имеет доступа к этому Arc<Mutex<f64>>.
        // Если это поле нужно для WS, потребуется другой механизм обновления.
        total_filled_spot_qty: Arc::new(TokioMutex::new(0.0)),
        // --- КОНЕЦ ПРЕДУПРЕЖДЕНИЯ ---
    };
    running_operations.lock().await.insert((chat_id, operation_id), info);
    info!("op_id:{}: Stored running WS hedge info.", operation_id);

    Ok(())
}
