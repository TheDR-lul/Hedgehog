// src/notifier/hedge_flow_spawners.rs

use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MaybeInaccessibleMessage};
use tokio::sync::mpsc;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::timeout;
use tracing::{info, error, warn, trace};
use anyhow::{anyhow, Result};
use futures_util::FutureExt;

use crate::webservice_hedge::hedge_task::HedgerWsHedgeTask;
use crate::config::Config;
use crate::exchange::Exchange;
use crate::exchange::bybit_ws;
use crate::exchange::types::{SubscriptionType, WebSocketMessage};
use crate::hedger::Hedger;
use crate::hedger::{HedgeParams, HedgeProgressCallback, HedgeProgressUpdate, HedgeStage, ORDER_FILL_TOLERANCE};
use crate::models::HedgeRequest;
use crate::storage::{Db, insert_hedge_operation, update_hedge_final_status};
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
    waiting_message: MaybeInaccessibleMessage,
)
where
E: Exchange + Clone + Send + Sync + 'static,
{
    let bot_message_id = match &waiting_message {
        MaybeInaccessibleMessage::Regular(msg) => msg.id,
        MaybeInaccessibleMessage::Inaccessible(_) => {
            error!("Cannot start sequential hedge: initial waiting message is inaccessible.");
            let _ = bot.send_message(chat_id, "❌ Ошибка: Не удалось получить доступ к исходному сообщению для обновления статуса хеджирования.")
                       .reply_markup(navigation::make_main_menu_keyboard())
                       .await;
            return;
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
        symbol: symbol_for_info, bot_message_id: bot_message_id.0,
        total_filled_spot_qty: total_filled_qty_storage,
    };
    running_operations.lock().await.insert((chat_id, operation_id), info);
    info!("op_id:{}: Stored running hedge info.", operation_id);
}

async fn wait_for_subscription_confirmation_by_req_id(
    operation_id: i64,
    ws_receiver: &mut mpsc::Receiver<Result<WebSocketMessage>>,
    sent_req_id: &str,
    timeout_duration: Duration,
    stream_name: &str,
) -> Result<Vec<WebSocketMessage>, anyhow::Error> {
    if sent_req_id.is_empty() {
        let err_msg = format!("op_id:{}: ({}) Ожидаемый req_id для проверки подписки пуст. Это ошибка в логике получения req_id.", operation_id, stream_name);
        error!("{}", err_msg);
        return Err(anyhow!(err_msg));
    }

    info!(op_id = operation_id, %stream_name, expected_req_id = %sent_req_id, "Ожидание подтверждения подписки по req_id (таймаут: {:?})...", timeout_duration);

    let start_time = std::time::Instant::now();
    let mut buffered_messages: Vec<WebSocketMessage> = Vec::new();

    loop {
        if start_time.elapsed() > timeout_duration {
            let err_msg = format!("op_id:{}: ({}) Таймаут ожидания подтверждения подписки по req_id ({:?}) для req_id: {}.", operation_id, stream_name, timeout_duration, sent_req_id);
            error!("{}", err_msg);
            return Err(anyhow!(err_msg));
        }

        match timeout(Duration::from_millis(500), ws_receiver.recv()).await {
            Ok(Some(Ok(ws_message))) => {
                match ws_message {
                    WebSocketMessage::SubscriptionResponse { success, ref topic } => {
                        info!(op_id = operation_id, %stream_name, %success, response_req_id = %topic, expected_req_id = %sent_req_id, "Получен SubscriptionResponse.");
                        if *topic == sent_req_id {
                            if success {
                                info!(op_id = operation_id, %stream_name, %sent_req_id, "Подписка успешно подтверждена по req_id.");
                                return Ok(buffered_messages);
                            } else {
                                let err_msg = format!("op_id:{}: ({}) Подписка НЕ подтверждена (success=false) для req_id: {}.", operation_id, stream_name, sent_req_id);
                                error!("{}", err_msg);
                                return Err(anyhow!(err_msg));
                            }
                        } else {
                            warn!(op_id = operation_id, %stream_name, received_req_id = %topic, expected_req_id = %sent_req_id, "Получен SubscriptionResponse, но с НЕОЖИДАННЫМ req_id. Буферизация.");
                            buffered_messages.push(WebSocketMessage::SubscriptionResponse { success, topic: topic.clone() }); // Убрали * перед success
                        }
                    }
                    WebSocketMessage::OrderBookL2 { .. } |
                    WebSocketMessage::OrderUpdate(_) |
                    WebSocketMessage::PublicTrade { .. } => {
                        trace!(op_id = operation_id, %stream_name, "Буферизация сообщения {:?} во время ожидания подтверждения подписки.", ws_message);
                        buffered_messages.push(ws_message);
                    }
                    WebSocketMessage::Pong | WebSocketMessage::Connected => {
                        trace!(op_id = operation_id, %stream_name, "Получено системное сообщение {:?} при ожидании. Игнорируется.", ws_message);
                    }
                    WebSocketMessage::Authenticated(auth_success) => { // Добавлена эта ветка
                        trace!(op_id = operation_id, %stream_name, "Получено сообщение Authenticated(success={}) при ожидании подтверждения подписки. Буферизация (если необходимо).", auth_success);
                        // Можно добавить в буфер, если это важно для последующей логики
                        // buffered_messages.push(WebSocketMessage::Authenticated(auth_success));
                    }
                    WebSocketMessage::Error(e) => {
                        warn!(op_id = operation_id, %stream_name, error = %e, "Получена ошибка WebSocket при ожидании подтверждения подписки. Буферизация.");
                        buffered_messages.push(WebSocketMessage::Error(e));
                    }
                    WebSocketMessage::Disconnected => {
                        warn!(op_id = operation_id, %stream_name, "Получено сообщение Disconnected при ожидании подтверждения. Подписка, вероятно, не удастся.");
                        return Err(anyhow!("Соединение разорвано ({}) во время ожидания подтверждения подписки", stream_name));
                    }
                }
            }
            Ok(Some(Err(e))) => {
                error!(op_id = operation_id, %stream_name, error = %e, "Ошибка MPSC канала при ожидании подтверждения подписки.");
                return Err(anyhow!("Ошибка канала MPSC ({}) при ожидании подтверждения: {}", stream_name, e));
            }
            Ok(None) => {
                error!(op_id = operation_id, %stream_name, "MPSC канал закрыт при ожидании подтверждения подписки.");
                return Err(anyhow!("Канал MPSC ({}) закрыт при ожидании подтверждения", stream_name));
            }
            Err(_) => {
                trace!(op_id = operation_id, %stream_name, "Таймаут попытки чтения из MPSC (500мс) при ожидании подписки. Продолжаем ожидание.");
            }
        }
    }
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
            let new_msg = bot.send_message(chat_id, "⚠️ Исходное сообщение для обновления статуса WS хеджирования недоступно. Запускаю WS хеджирование...").await
                .map_err(|e| {
                    error!("op_chat_id:{}: Failed to send new status message for WS hedge: {}", chat_id, e);
                    anyhow!("Failed to secure a message ID for WS hedge status updates.")
                })?;
            new_msg.id
        }
    };

    let symbol = request.symbol.clone();
    info!("op_chat_id:{}: Preparing to spawn WS Hedge Task for {}...", chat_id, symbol);

    let operation_id = match insert_hedge_operation(
        db.as_ref(), chat_id.0, &symbol, &cfg.quote_currency, request.sum,
        request.volatility, 0.0, 0.0,
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

    let pending_private_messages: Vec<WebSocketMessage>; // Убран mut
    let pending_spot_messages: Vec<WebSocketMessage>;    // Убран mut
    let pending_linear_messages: Vec<WebSocketMessage>;  // Убран mut

    let private_subscriptions = vec![SubscriptionType::Order];
    let (mut private_ws_receiver, private_req_id_opt) =
        match bybit_ws::connect_and_subscribe((*cfg).clone(), private_subscriptions).await {
            Ok(result_tuple) => { info!("op_id:{}: Private WebSocket connected. Subscription ReqID: {:?}", operation_id, result_tuple.1); result_tuple },
            Err(e) => {
                let err_text = format!("❌ Ошибка подключения приватного WebSocket: {}", e);
                error!("op_id:{}: {}", operation_id, err_text);
                let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
                let _ = update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
                return Err(e);
            }
        };
    let private_req_id = private_req_id_opt.ok_or_else(|| anyhow!("Не получен req_id для приватной подписки 'order'"))?;
    pending_private_messages = match wait_for_subscription_confirmation_by_req_id(operation_id, &mut private_ws_receiver, &private_req_id, Duration::from_secs(15), "Private (order)").await {
        Ok(messages) => { info!("op_id:{}: Подписка 'order' (req_id: {}) подтверждена. {} буферизованных сообщений.", operation_id, private_req_id, messages.len()); messages },
        Err(e) => {
            let err_text = format!("❌ Не удалось подтвердить подписку 'order' (req_id: {}) на приватном WebSocket: {}", private_req_id, e);
            error!("op_id:{}: {}", operation_id, err_text);
            let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
            let _ = update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
            return Err(e);
        }
    };

    let spot_symbol_ws = format!("{}{}", symbol, cfg.quote_currency);
    let ws_order_book_depth = cfg.ws_order_book_depth;
    let spot_orderbook_subscriptions = vec![SubscriptionType::Orderbook { symbol: spot_symbol_ws.clone(), depth: ws_order_book_depth }];
    let (mut public_spot_receiver, spot_req_id_opt) = match bybit_ws::connect_public_stream(
        (*cfg).clone(), "spot", spot_orderbook_subscriptions.clone()
    ).await {
        Ok(result_tuple) => { info!("op_id:{}: Public SPOT WebSocket connected. Subscription ReqID: {:?}", operation_id, result_tuple.1); result_tuple },
        Err(e) => {
            let err_text = format!("❌ Ошибка подключения публичного SPOT WebSocket: {}", e);
            error!("op_id:{}: {}", operation_id, err_text);
            let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
            let _ = update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
            return Err(e);
         }
    };
    let spot_req_id = spot_req_id_opt.ok_or_else(|| anyhow!("Не получен req_id для публичной SPOT подписки на '{}'", spot_symbol_ws))?;
    pending_spot_messages = match wait_for_subscription_confirmation_by_req_id(operation_id, &mut public_spot_receiver, &spot_req_id, Duration::from_secs(15), "Public SPOT (orderbook)").await {
        Ok(messages) => { info!("op_id:{}: Подписка на ордербук SPOT (req_id: {}) подтверждена. {} буферизованных сообщений.", operation_id, spot_req_id, messages.len()); messages },
        Err(e) => {
            let err_text = format!("❌ Не удалось подтвердить подписку на ордербук (req_id: {}) на публичном SPOT WebSocket: {}", spot_req_id, e);
            error!("op_id:{}: {}", operation_id, err_text);
            let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
            let _ = update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
            return Err(e);
        }
    };

    let linear_symbol_ws = format!("{}{}", symbol, cfg.quote_currency);
    let linear_orderbook_subscriptions = vec![SubscriptionType::Orderbook { symbol: linear_symbol_ws.clone(), depth: ws_order_book_depth }];
    let (mut public_linear_receiver, linear_req_id_opt) = match bybit_ws::connect_public_stream(
        (*cfg).clone(), "linear", linear_orderbook_subscriptions.clone()
    ).await {
        Ok(result_tuple) => { info!("op_id:{}: Public LINEAR WebSocket connected. Subscription ReqID: {:?}", operation_id, result_tuple.1); result_tuple },
        Err(e) => {
            let err_text = format!("❌ Ошибка подключения публичного LINEAR WebSocket: {}", e);
            error!("op_id:{}: {}", operation_id, err_text);
            let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
            let _ = update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
            return Err(e);
         }
    };
     let linear_req_id = linear_req_id_opt.ok_or_else(|| anyhow!("Не получен req_id для публичной LINEAR подписки на '{}'", linear_symbol_ws))?;
    pending_linear_messages = match wait_for_subscription_confirmation_by_req_id(operation_id, &mut public_linear_receiver, &linear_req_id, Duration::from_secs(15), "Public LINEAR (orderbook)").await {
        Ok(messages) => { info!("op_id:{}: Подписка на ордербук LINEAR (req_id: {}) подтверждена. {} буферизованных сообщений.", operation_id, linear_req_id, messages.len()); messages },
        Err(e) => {
            let err_text = format!("❌ Не удалось подтвердить подписку на ордербук (req_id: {}) на публичном LINEAR WebSocket: {}", linear_req_id, e);
            error!("op_id:{}: {}", operation_id, err_text);
            let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
            let _ = update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
            return Err(e);
        }
    };

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

        async move {
            let progress_bar_len = 10;
            let status_text_suffix = if update.is_replacement { "(Ордер переставлен)" } else { "" };
            let current_order_filled_percent = if update.target_qty > ORDER_FILL_TOLERANCE { (update.filled_qty / update.target_qty) * 100.0 } else { 0.0 };
            let overall_stage_filled_percent = if update.total_target_qty > ORDER_FILL_TOLERANCE { (update.cumulative_filled_qty / update.total_target_qty) * 100.0 } else { if update.cumulative_filled_qty > ORDER_FILL_TOLERANCE { 100.0 } else { 0.0 }};
            let filled_blocks_overall = (overall_stage_filled_percent.min(100.0) / (100.0 / progress_bar_len as f64)).round() as usize;
            let empty_blocks_overall = progress_bar_len - filled_blocks_overall;
            let progress_bar_overall = format!("[{}{}]", "█".repeat(filled_blocks_overall), "░".repeat(empty_blocks_overall));
            let stage_name = match update.stage { HedgeStage::Spot => "Спот (покупка)", HedgeStage::Futures => "Фьючерс (продажа)" };
            let market_price_label = match update.stage { HedgeStage::Spot => "Спот", HedgeStage::Futures => "Фьюч" };

            let text = format!(
                 "⏳ Хедж WS (Этап: {}) ID:{} {} ({})\n\
                  Рыночная цена ({}) ~{:.2} {}\n\
                  Тек. лимит. ордер: {:.6} @ {:.2} {} {}\n\
                  Исполнено (тек.ордер): {:.1}%\n\
                  Исполнено (всего этап): {:.6}/{:.6} ({:.1}%)",
                 stage_name, operation_id_cb, progress_bar_overall, symbol_cb,
                 market_price_label, update.current_spot_price, qc_cb,
                 update.target_qty, update.new_limit_price, qc_cb, status_text_suffix,
                 current_order_filled_percent,
                 update.cumulative_filled_qty, update.total_target_qty, overall_stage_filled_percent
            );
            let cancel_callback_data = format!("{}{}", callback_data::PREFIX_CANCEL_ACTIVE_OP, operation_id_cb);
            let cancel_button = InlineKeyboardButton::callback("❌ Отменить эту операцию", cancel_callback_data);
            let kb = InlineKeyboardMarkup::new(vec![vec![cancel_button]]);
            if let Err(e) = bot_cb.edit_message_text(chat_id_cb, msg_id_cb, text).reply_markup(kb).await {
                if !e.to_string().contains("message is not modified") { warn!("op_id:{}: WS Progress callback failed: {}", operation_id_cb, e); }
            }
            Ok(())
        }.boxed()
    });

    let mut hedge_task = match HedgerWsHedgeTask::new(
        operation_id, request, cfg.clone(), db.clone(), exchange_rest.clone(), progress_callback,
        private_ws_receiver, public_spot_receiver, public_linear_receiver,
    ).await {
        Ok(task) => { info!("op_id:{}: HedgerWsHedgeTask initialized successfully with tri-WebSocket streams.", operation_id); task },
        Err(e) => {
            let error_text = format!("❌ Ошибка инициализации WS стратегии (tri-stream): {}", e);
            error!("op_id:{}: {}", operation_id, error_text);
            let _ = bot.edit_message_text(chat_id, bot_message_id, error_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
            let _ = update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&error_text)).await;
            return Err(e);
        }
    };

    for msg in pending_private_messages {
        if let Err(e) = hedge_task.handle_ws_message_with_category(msg, "private").await {
            warn!("op_id:{}: Ошибка обработки буферизованного приватного сообщения перед запуском задачи: {}", operation_id, e);
        }
    }
    for msg in pending_spot_messages {
        if let Err(e) = hedge_task.handle_ws_message_with_category(msg, "spot").await {
            warn!("op_id:{}: Ошибка обработки буферизованного SPOT сообщения перед запуском задачи: {}", operation_id, e);
        }
    }
    for msg in pending_linear_messages {
        if let Err(e) = hedge_task.handle_ws_message_with_category(msg, "linear").await {
            warn!("op_id:{}: Ошибка обработки буферизованного LINEAR сообщения перед запуском задачи: {}", operation_id, e);
        }
    }

    let bot_clone_for_spawn = bot.clone();
    let running_operations_clone = running_operations.clone();
    let symbol_clone_for_spawn = symbol.clone();
    let cfg_clone_for_spawn = cfg.clone();

    let task_handle = tokio::spawn(async move {
        info!("op_id:{}: Spawning WS hedge task execution (tri-stream)...", operation_id);
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
                     final_futures_filled, symbol_clone_for_spawn
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
                    error!("op_id:{}: WS Hedge task failed: {} (final status from task state: {})", operation_id, e, final_status_str);
                } else {
                    info!("op_id:{}: WS Hedge task cancelled (final status from task state: {}). Error from run: {}", operation_id, final_status_str, e);
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
    info!("op_id:{}: Stored running WS hedge info (tri-stream).", operation_id);

    Ok(())
}