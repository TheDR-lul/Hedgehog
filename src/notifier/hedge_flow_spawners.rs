// src/notifier/hedge_flow_spawners.rs

use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MaybeInaccessibleMessage};
use tokio::sync::Mutex as TokioMutex;
use tracing::{info, error, warn}; 
use anyhow::{anyhow, Result}; 
use std::collections::HashSet;
use futures_util::FutureExt;

use crate::webservice_hedge::hedge_task::HedgerWsHedgeTask;
// Неиспользуемый HedgerWsStatus был удален в предыдущем шаге
use crate::config::Config;
use crate::exchange::Exchange;
use crate::exchange::bybit_ws;
use crate::exchange::types::{SubscriptionType, WebSocketMessage};
use crate::hedger::{HedgeParams, HedgeProgressCallback, HedgeProgressUpdate, HedgeStage, Hedger, ORDER_FILL_TOLERANCE};
use crate::models::HedgeRequest;
use crate::storage::{Db, insert_hedge_operation};
use crate::notifier::{RunningOperations, RunningOperationInfo, OperationType, navigation, callback_data};

// Функция spawn_sequential_hedge_task остается без изменений
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
    initial_bot_message: MaybeInaccessibleMessage,
)
where
E: Exchange + Clone + Send + Sync + 'static,
{
    let bot_message_id = match initial_bot_message {
        MaybeInaccessibleMessage::Regular(msg) => msg.id,
        MaybeInaccessibleMessage::Inaccessible(_) => {
            error!("op_chat_id:{}: Cannot start sequential hedge: initial waiting message is inaccessible. Sending new status message.", chat_id);
            if let Ok(new_msg) = bot.send_message(chat_id, "⚠️ Ошибка: Исходное сообщение для обновления статуса недоступно. Хеджирование запускается...").await {
                new_msg.id
            } else {
                error!("op_chat_id:{}: Failed to send new status message after initial message became inaccessible.", chat_id);
                return;
            }
        }
    };

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
    let cfg_for_callback = cfg.clone();
    let cfg_for_spawn = cfg.clone();
    let db_clone = db.clone();
    let total_filled_qty_storage_clone = total_filled_qty_storage.clone();
    let running_operations_clone = running_operations.clone();
    let exchange_task_clone = exchange.clone();

    let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
         let bot_for_callback = bot_clone.clone();
         let qc = cfg_for_callback.quote_currency.clone();
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
                 let final_net_spot_balance = match exchange_task_clone.get_balance(&symbol_for_task_body).await { Ok(b) => b.free, Err(_) => spot_qty_gross };
                 let success_text = format!(
                      "✅ Хеджирование ID:{} ~{:.2} {} ({}) при V={:.1}% завершено:\n\n🟢 Спот куплено (брутто): {:.8}\nspot_balance_check {:.8}\n🔴 Фьюч продано (нетто): {:.8}",
                     operation_id, final_spot_value_gross, cfg_for_spawn.quote_currency, symbol_for_task_body,
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
        symbol: symbol_for_info, bot_message_id: bot_message_id.0,
        total_filled_spot_qty: total_filled_qty_storage,
    };
    running_operations.lock().await.insert((chat_id, operation_id), info);
    info!("op_id:{}: Stored running hedge info.", operation_id);
}

pub(super) async fn spawn_ws_hedge_task<E>(
    bot: Bot,
    exchange_rest: Arc<E>, 
    cfg: Arc<Config>,
    db: Arc<Db>,
    running_operations: RunningOperations,
    chat_id: ChatId,
    request: HedgeRequest,
    initial_bot_message: MaybeInaccessibleMessage,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let bot_message_id = match initial_bot_message {
        MaybeInaccessibleMessage::Regular(msg) => msg.id,
        MaybeInaccessibleMessage::Inaccessible(_) => {
            error!("op_chat_id:{}: Cannot start WS hedge: initial waiting message is inaccessible.", chat_id);
            if let Ok(new_msg) = bot.send_message(chat_id, "⚠️ Исходное сообщение для обновления статуса WS хеджирования недоступно. Запускаю WS хеджирование...").await {
                new_msg.id
            } else {
                error!("op_chat_id:{}: Failed to send new status message for WS hedge.", chat_id);
                return Err(anyhow!("Failed to secure a message ID for WS hedge status updates."));
            }
        }
    };

    let symbol = request.symbol.clone();
    let initial_sum = request.sum;
    let volatility_fraction = request.volatility;

    info!("op_chat_id:{}: Preparing to spawn WS Hedge Task for {}...", chat_id, symbol);

    let operation_id = match insert_hedge_operation(
        db.as_ref(), chat_id.0, &symbol, &cfg.quote_currency, initial_sum,
        volatility_fraction, 0.0, 0.0, 
    ).await {
        Ok(id) => { info!("op_id:{}: Created DB record for WS hedge operation.", id); id }
        Err(e) => {
            error!("op_id:?: Failed insert WS hedge op to DB: {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, format!("❌ DB Error: {}", e))
                         .reply_markup(navigation::make_main_menu_keyboard()).await;
            return Err(e.into());
        }
    };

    let _ = bot.edit_message_text(chat_id, bot_message_id, format!("⏳ Подключение WebSocket для {} (ID: {})...", symbol, operation_id))
               .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())) 
               .await;

    let private_subscriptions = vec![SubscriptionType::Order];
    // ИЗМЕНЕНО: Передаем (*cfg).clone() вместо cfg.clone()
    let mut private_ws_receiver = match bybit_ws::connect_and_subscribe((*cfg).clone(), private_subscriptions).await {
        Ok(receiver) => {
            info!("op_id:{}: Private WebSocket connected for 'order' topic.", operation_id);
            receiver
        },
        Err(e) => {
            error!("op_id:{}: Failed to connect Private WebSocket: {}", operation_id, e);
            let err_text = format!("❌ Ошибка подключения приватного WebSocket: {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
            let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
            return Err(e);
        }
    };
    if !wait_for_specific_subscriptions(operation_id, &mut private_ws_receiver, &["order".to_string()], Duration::from_secs(10)).await {
        let err_text = "❌ Не удалось подтвердить подписку 'order' на приватном WebSocket.".to_string();
        error!("op_id:{}: {}", operation_id, err_text);
        let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
        let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
        return Err(anyhow!(err_text));
    }
    info!("op_id:{}: 'order' subscription confirmed on private WebSocket.", operation_id);

    let spot_symbol_ws = format!("{}{}", symbol, cfg.quote_currency);
    let ws_order_book_depth = cfg.ws_order_book_depth;
    let orderbook_subscriptions = vec![
        SubscriptionType::Orderbook { symbol: spot_symbol_ws.clone(), depth: ws_order_book_depth }
    ];
    
    let public_stream_category = "spot"; 

    // ИЗМЕНЕНО: Передаем (*cfg).clone() вместо cfg.clone()
    let mut public_ws_receiver = match bybit_ws::connect_public_stream((*cfg).clone(), public_stream_category, orderbook_subscriptions).await {
        Ok(receiver) => {
            info!("op_id:{}: Public WebSocket connected for 'orderbook' topic (category: {}).", operation_id, public_stream_category);
            receiver
        },
        Err(e) => {
            error!("op_id:{}: Failed to connect Public WebSocket (category: {}): {}", operation_id, public_stream_category, e);
            let err_text = format!("❌ Ошибка подключения публичного WebSocket ({}): {}", public_stream_category, e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
            let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
            return Err(e);
        }
    };
    let expected_orderbook_topic = format!("orderbook.{}.{}", ws_order_book_depth, spot_symbol_ws);
    if !wait_for_specific_subscriptions(operation_id, &mut public_ws_receiver, &[expected_orderbook_topic.clone()], Duration::from_secs(10)).await {
        let err_text = format!("❌ Не удалось подтвердить подписку '{}' на публичном WebSocket.", expected_orderbook_topic);
        error!("op_id:{}: {}", operation_id, err_text);
        let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
        let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
        return Err(anyhow!(err_text));
    }
    info!("op_id:{}: '{}' subscription confirmed on public WebSocket.", operation_id, expected_orderbook_topic);

    let _ = bot.edit_message_text(chat_id, bot_message_id, format!("⏳ Инициализация WS стратегии для {} (ID: {})...", symbol, operation_id)).await;
    
    let bot_clone_for_callback = bot.clone();
    let cfg_for_callback = cfg.clone();
    let symbol_for_callback = symbol.clone();

    let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
        let bot_cb = bot_clone_for_callback.clone();
        let qc_cb = cfg_for_callback.quote_currency.clone();
        let symbol_cb = symbol_for_callback.clone();
        let msg_id_cb = bot_message_id;
        let chat_id_cb = chat_id;
        let operation_id_cb = operation_id;
        // ... (остальная часть замыкания коллбэка без изменений) ...
        let stage_cb = update.stage;
        let current_price_cb = update.current_spot_price;
        let limit_price_cb = update.new_limit_price;
        let is_replacement_cb = update.is_replacement;
        let filled_qty_current_order_cb = update.filled_qty;
        let target_qty_current_order_cb = update.target_qty;
        let cumulative_filled_qty_stage_cb = update.cumulative_filled_qty;
        let total_target_qty_stage_cb = update.total_target_qty;

        async move {
            let progress_bar_len = 10;
            let status_text_suffix = if is_replacement_cb { "(Ордер переставлен)" } else { "" };

            let current_order_filled_percent = if target_qty_current_order_cb > ORDER_FILL_TOLERANCE {
                (filled_qty_current_order_cb / target_qty_current_order_cb) * 100.0
            } else {
                0.0
            };

            let overall_stage_filled_percent = if total_target_qty_stage_cb > ORDER_FILL_TOLERANCE {
                (cumulative_filled_qty_stage_cb / total_target_qty_stage_cb) * 100.0
            } else {
                 if cumulative_filled_qty_stage_cb > ORDER_FILL_TOLERANCE { 100.0 } else { 0.0 }
            };

            let filled_blocks_overall = (overall_stage_filled_percent.min(100.0) / (100.0 / progress_bar_len as f64)).round() as usize;
            let empty_blocks_overall = progress_bar_len - filled_blocks_overall;
            let progress_bar_overall = format!("[{}{}]", "█".repeat(filled_blocks_overall), "░".repeat(empty_blocks_overall));

            let stage_name = match stage_cb {
                HedgeStage::Spot => "Спот (покупка)",
                HedgeStage::Futures => "Фьючерс (продажа)",
            };
            let market_price_label = match stage_cb {
                 HedgeStage::Spot => "Спот",
                 HedgeStage::Futures => "Фьюч",
            };

            let text = format!(
                 "⏳ Хедж WS (Этап: {}) ID:{} {} ({})\n\
                  Рыночная цена ({}) ~{:.2} {}\n\
                  Тек. лимит. ордер: {:.6} @ {:.2} {} {}\n\
                  Исполнено (тек.ордер): {:.1}%\n\
                  Исполнено (всего этап): {:.6}/{:.6} ({:.1}%)",
                 stage_name, operation_id_cb, progress_bar_overall, symbol_cb,
                 market_price_label, current_price_cb, qc_cb,
                 target_qty_current_order_cb, limit_price_cb, qc_cb,
                 status_text_suffix,
                 current_order_filled_percent,
                 cumulative_filled_qty_stage_cb, total_target_qty_stage_cb,
                 overall_stage_filled_percent
            );

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

    // ИСПРАВЛЕН ВЫЗОВ HedgerWsHedgeTask::new
    let mut hedge_task = match HedgerWsHedgeTask::new(
        operation_id,
        request, 
        cfg.clone(),
        db.clone(),
        exchange_rest.clone(), 
        progress_callback,
        private_ws_receiver,   
        public_ws_receiver,    
    ).await {
        Ok(task) => {
            info!("op_id:{}: HedgerWsHedgeTask initialized successfully with dual WebSocket streams.", operation_id);
            task
        },
        Err(e) => {
            let error_text = format!("❌ Ошибка инициализации WS стратегии (dual stream): {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, error_text.clone())
                         .reply_markup(navigation::make_main_menu_keyboard()).await;
            return Err(e);
        }
    };

    let bot_clone_for_spawn = bot.clone();
    let running_operations_clone = running_operations.clone();
    let symbol_clone_for_spawn = symbol.clone();
    let cfg_clone_for_spawn = cfg.clone();

    let task_handle = tokio::spawn(async move {
        info!("op_id:{}: Spawning WS hedge task execution (dual stream)...", operation_id);
        
        let run_result = hedge_task.run().await;

        let mut ops_guard = running_operations_clone.lock().await;
        if ops_guard.contains_key(&(chat_id, operation_id)) {
            ops_guard.remove(&(chat_id, operation_id));
            info!("op_id:{}: Removed running WS operation info after task completion.", operation_id);
        } else {
            info!("op_id:{}: Running WS operation info already removed (likely due to cancellation).", operation_id);
        }
        drop(ops_guard); 

        match run_result {
            Ok(_) => {
                info!("op_id:{}: WS Hedge task completed successfully (status: {:?}).", operation_id, hedge_task.state.status);
                let final_spot_filled = hedge_task.state.cumulative_spot_filled_quantity.to_string();
                let final_futures_filled = hedge_task.state.cumulative_futures_filled_quantity.to_string();
                let final_text = format!(
                    "✅ WS Хедж ID:{} для {} завершен.\n\
                     Куплено спота: ~{} {}\n\
                     Продано фьючерса: ~{} {}",
                    operation_id, symbol_clone_for_spawn, final_spot_filled,
                    cfg_clone_for_spawn.quote_currency,
                    final_futures_filled, cfg_clone_for_spawn.quote_currency
                );
                if let Err(e) = bot_clone_for_spawn.edit_message_text(chat_id, bot_message_id, final_text)
                         .reply_markup(navigation::make_main_menu_keyboard())
                         .await {
                    warn!("op_id:{}: Failed to edit final success message: {}", operation_id, e);
                }
            }
            Err(e) => {
                let final_status_str = format!("{:?}", hedge_task.state.status);
                if !e.to_string().to_lowercase().contains("cancelled by user") && !final_status_str.to_lowercase().contains("cancelled") {
                    error!("op_id:{}: WS Hedge task failed: {} (final status: {})", operation_id, e, final_status_str);
                } else {
                    info!("op_id:{}: WS Hedge task cancelled by user or from within (final status: {}). Error: {}", operation_id, final_status_str, e);
                }
                 let final_text = format!("❌ Ошибка/Отмена WS Хедж ID:{}: {} (Статус: {})", operation_id, e, final_status_str);
                 if let Err(edit_err) = bot_clone_for_spawn.edit_message_text(chat_id, bot_message_id, final_text)
                          .reply_markup(navigation::make_main_menu_keyboard())
                          .await {
                     warn!("op_id:{}: Failed to edit final error/cancel message: {}", operation_id, edit_err);
                 }
            }
        }
    });

    let operation_info = RunningOperationInfo {
        handle: task_handle.abort_handle(),
        operation_id,
        operation_type: OperationType::Hedge, 
        symbol: symbol.clone(),
        bot_message_id: bot_message_id.0,
        total_filled_spot_qty: Arc::new(TokioMutex::new(0.0)), 
    };
    running_operations.lock().await.insert((chat_id, operation_id), operation_info);
    info!("op_id:{}: Stored running WS hedge info (dual stream).", operation_id);

    Ok(())
}

// ИЗМЕНЕНА ЛОГИКА wait_for_specific_subscriptions
async fn wait_for_specific_subscriptions(
    operation_id: i64,
    // ИЗМЕНЕНО: Тип на tokio::sync::mpsc::Receiver
    ws_receiver: &mut tokio::sync::mpsc::Receiver<Result<WebSocketMessage>>,
    expected_topics: &[String],
    timeout_duration: Duration,
) -> bool {
    let mut confirmed_topics = HashSet::new();
    let expected_set: HashSet<_> = expected_topics.iter().map(|s| s.as_str()).collect();

    info!("op_id:{}: Waiting for specific subscriptions: {:?}...", operation_id, expected_topics);

    match tokio::time::timeout(timeout_duration, async {
        while confirmed_topics.len() < expected_topics.len() {
            match ws_receiver.recv().await {
                Some(Ok(WebSocketMessage::SubscriptionResponse { success, topic })) => {
                    info!("op_id:{}: Received subscription response: success={}, topic='{}'", operation_id, success, topic);
                    if success {
                        if expected_set.contains(topic.as_str()) {
                            confirmed_topics.insert(topic);
                        } else if topic.is_empty() && expected_set.contains("order") {
                            // Специальный случай для "order", если Bybit возвращает пустой топик при успехе
                            info!("op_id:{}: Assuming 'order' subscription success due to empty topic in response.", operation_id);
                            confirmed_topics.insert("order".to_string()); 
                        } else if topic.is_empty() && !expected_set.is_empty() {
                            warn!("op_id:{}: Subscription success with empty topic, but expected_set is {:?}. This might be an issue if not 'order'.", operation_id, expected_set);
                            // Если мы ожидали что-то конкретное, а пришел пустой топик, это может быть проблемой.
                            // Для 'orderbook' мы ожидаем непустой топик.
                            if !expected_set.contains("order") { // Если это не была подписка на 'order'
                                error!("op_id:{}: Subscription success with empty topic, but expected non-empty topic(s): {:?}", operation_id, expected_set);
                                return false;
                            }
                        }
                        // Если topic не пустой и не в expected_set, это просто неожиданная успешная подписка, логируем выше.
                    } else { // if !success
                        if expected_set.contains(topic.as_str()) {
                            error!("op_id:{}: Failed subscription for expected topic: {}", operation_id, topic);
                            return false; 
                        } else if topic.is_empty() && expected_set.contains("order") {
                             error!("op_id:{}: Failed subscription for 'order' (received empty topic and success=false).", operation_id);
                             return false;
                        } else if topic.is_empty() && !expected_set.is_empty() {
                             error!("op_id:{}: Subscription failed with empty topic, expected {:?}.", operation_id, expected_set);
                             return false;
                        }
                        // Если topic не пустой и не в expected_set, это проваленная подписка на что-то неожиданное.
                    }
                }
                Some(Ok(WebSocketMessage::Error(e))) => {
                    error!("op_id:{}: WebSocket error during specific subscription wait: {}", operation_id, e);
                    return false;
                }
                Some(Err(e)) => {
                    error!("op_id:{}: MPSC channel error during specific subscription wait: {}", operation_id, e);
                    return false;
                }
                None => { 
                    error!("op_id:{}: MPSC channel closed during specific subscription wait.", operation_id);
                    return false;
                }
                _ => {} 
            }
        }
        // Проверяем, что все *ожидаемые* топики действительно были подтверждены
        expected_set.iter().all(|expected| confirmed_topics.contains(*expected))
    }).await {
        // Ok(true) означает, что цикл завершился и все ожидаемые подписки подтверждены
        // Ok(false) означает, что цикл завершился, но НЕ все ожидаемые подписки подтверждены или была ошибка
        Ok(all_confirmed_successfully) => {
            if !all_confirmed_successfully {
                 error!("op_id:{}: Not all expected subscriptions were confirmed. Confirmed: {:?}, Expected: {:?}", 
                    operation_id, confirmed_topics, expected_topics);
            }
            all_confirmed_successfully
        }
        Err(_) => { // Таймаут
            error!("op_id:{}: Timed out waiting for specific subscriptions. Confirmed: {:?}, Expected: {:?}", 
                operation_id, confirmed_topics, expected_topics);
            false
        }
    }
}