// src/notifier/hedge_flow_spawners.rs

use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MaybeInaccessibleMessage, MessageId}; // Добавили MessageId
use tokio::sync::Mutex as TokioMutex;
use tracing::{info, error, warn};
use futures::future::FutureExt;
use anyhow::{anyhow, Result};
use std::collections::HashSet;

use crate::webservice_hedge::hedge_task::HedgerWsHedgeTask;
use crate::webservice_hedge::state::HedgerWsStatus; // Для проверки статуса
use crate::config::Config;
use crate::exchange::Exchange;
use crate::exchange::bybit_ws;
use crate::exchange::types::{SubscriptionType, WebSocketMessage}; // Добавили WebSocketMessage для проверки
use crate::hedger::{HedgeParams, HedgeProgressCallback, HedgeProgressUpdate, HedgeStage, Hedger, ORDER_FILL_TOLERANCE};
use crate::models::HedgeRequest;
use crate::storage::{Db, insert_hedge_operation};
use crate::notifier::{RunningOperations, RunningOperationInfo, OperationType, navigation, callback_data};


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
            error!("op_chat_id:{}: Cannot start WS hedge: initial waiting message is inaccessible. Sending new status message.", chat_id);
            if let Ok(new_msg) = bot.send_message(chat_id, "⚠️ Ошибка: Исходное сообщение для обновления статуса WS хеджирования недоступно. Хеджирование запускается...").await {
                new_msg.id
            } else {
                error!("op_chat_id:{}: Failed to send new status message after initial message became inaccessible for WS hedge.", chat_id);
                return Err(anyhow!("Initial bot message inaccessible and failed to send new one for WS hedge."));
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
               .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())) // Пустая клавиатура на время загрузки
               .await;

    let spot_symbol_ws = format!("{}{}", symbol, cfg.quote_currency);
    let futures_symbol_ws = format!("{}{}", symbol, cfg.quote_currency);
    let ws_order_book_depth = cfg.ws_order_book_depth;

    let mut unique_subscriptions = HashSet::new();
    unique_subscriptions.insert(SubscriptionType::Order);
    unique_subscriptions.insert(SubscriptionType::Orderbook { symbol: spot_symbol_ws.clone(), depth: ws_order_book_depth });
    unique_subscriptions.insert(SubscriptionType::Orderbook { symbol: futures_symbol_ws.clone(), depth: ws_order_book_depth });
    let subscriptions_vec: Vec<SubscriptionType> = unique_subscriptions.into_iter().collect();

    let ws_receiver = match bybit_ws::connect_and_subscribe((*cfg).clone(), subscriptions_vec).await {
        Ok(receiver) => {
            info!("op_id:{}: WebSocket connected and subscribed successfully.", operation_id);
            // Проверяем, что все критичные подписки действительно вернули успех
            // Это потребует дополнительной логики ожидания SubscriptionResponse или изменения connect_and_subscribe
            // Пока что просто продолжаем.
            let _ = bot.edit_message_text(chat_id, bot_message_id, format!("⏳ Инициализация WS стратегии для {} (ID: {})...", symbol, operation_id)).await;
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
    let cfg_for_callback = cfg.clone();
    let symbol_for_callback = symbol.clone();

    let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
        // ... (логика колбэка остается без изменений) ...
        let bot_cb = bot_clone_for_callback.clone();
        let qc_cb = cfg_for_callback.quote_currency.clone();
        let symbol_cb = symbol_for_callback.clone();
        let msg_id_cb = bot_message_id;
        let chat_id_cb = chat_id;
        let operation_id_cb = operation_id;
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

    let mut hedge_task = match HedgerWsHedgeTask::new(
        operation_id, request, cfg.clone(), db.clone(),
        exchange_rest.clone(), progress_callback, ws_receiver,
    ).await {
        Ok(task) => {
            info!("op_id:{}: HedgerWsHedgeTask initialized successfully.", operation_id);
            task
        },
        Err(e) => {
            // Ошибка уже залогирована в HedgerWsHedgeTask::new, если get_fee_rate упал
            // Здесь мы просто обновляем сообщение и выходим
            let error_text = format!("❌ Ошибка инициализации WS стратегии: {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, error_text.clone())
                     .reply_markup(navigation::make_main_menu_keyboard()).await;
            // Статус в БД уже должен быть обновлен на Failed внутри new, если там ошибка
            // Но на всякий случай, можно еще раз, если HedgerWsHedgeTask::new не обновляет
            // let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&error_text)).await;
            return Err(e);
        }
    };

    let bot_clone_for_spawn = bot.clone();
    let running_operations_clone = running_operations.clone();
    let symbol_clone_for_spawn = symbol.clone();
    let cfg_clone_for_spawn = cfg.clone();

    let task_handle = tokio::spawn(async move {
        info!("op_id:{}: Spawning WS hedge task execution...", operation_id);
        
        // --- УЛУЧШЕНИЕ: Ожидание начальных данных стакана перед первым запуском чанка ---
        if matches!(hedge_task.state.status, HedgerWsStatus::StartingChunk(1)) {
            let mut price_data_ready = false;
            for attempt in 0..15 { // 15 попыток * 200 мс = 3 секунды
                // Проверяем наличие цен в состоянии задачи.
                // `handle_websocket_message` (вызываемый из `read_loop`) должен обновлять `task.state`
                if hedge_task.state.spot_market_data.best_bid_price.is_some() && 
                   hedge_task.state.spot_market_data.best_ask_price.is_some() &&
                   hedge_task.state.futures_market_data.best_bid_price.is_some() && // Проверяем и фьючерсы
                   hedge_task.state.futures_market_data.best_ask_price.is_some()
                {
                    price_data_ready = true;
                    info!(operation_id = hedge_task.operation_id, "Market data for initial chunk is ready.");
                    break;
                }
                warn!(operation_id = hedge_task.operation_id, attempt = attempt + 1, "Waiting for initial market data before starting chunk 1...");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }

            if !price_data_ready {
                let error_msg = "Failed to get initial market data for chunk calculation after several attempts.";
                error!(operation_id = hedge_task.operation_id, "{}", error_msg);
                hedge_task.state.status = HedgerWsStatus::Failed(error_msg.to_string());
                // Обновляем БД и выходим из задачи (не из spawn_ws_hedge_task, а из этого tokio::spawn)
                crate::webservice_hedge::hedge_logic::helpers::update_final_db_status(&hedge_task).await;

                // Обновляем сообщение в телеграме
                let final_text = format!("❌ Ошибка WS Хедж ID:{}: {}", hedge_task.operation_id, error_msg);
                if let Err(edit_err) = bot_clone_for_spawn.edit_message_text(chat_id, bot_message_id, final_text)
                         .reply_markup(navigation::make_main_menu_keyboard())
                         .await {
                    warn!("op_id:{}: Failed to edit final error message: {}", hedge_task.operation_id, edit_err);
                }
                // Удаляем из running_operations
                running_operations_clone.lock().await.remove(&(chat_id, hedge_task.operation_id));
                return; // Завершаем этот tokio::spawn
            }
        }
        // --- КОНЕЦ УЛУЧШЕНИЯ ---

        let run_result = hedge_task.run().await;

        // Логика после завершения run_result остается такой же
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
    info!("op_id:{}: Stored running WS hedge info.", operation_id);

    Ok(())
}