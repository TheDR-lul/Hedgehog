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
use crate::config::Config;
use crate::exchange::Exchange;
use crate::exchange::bybit_ws;
use crate::exchange::types::{SubscriptionType, WebSocketMessage};
use crate::hedger::{HedgeParams, HedgeProgressCallback, HedgeProgressUpdate, HedgeStage, /* Hedger, */ ORDER_FILL_TOLERANCE}; // Убрал Hedger, т.к. он не используется в этой функции
use crate::models::HedgeRequest;
use crate::storage::{Db, insert_hedge_operation};
use crate::notifier::{RunningOperations, RunningOperationInfo, OperationType, navigation, callback_data};

// Функция spawn_sequential_hedge_task (если используется, префиксы _ для неиспользуемых переменных)
pub(super) async fn spawn_sequential_hedge_task<E>(
    bot: Bot,
    _exchange: Arc<E>, // Добавлен _ если не используется напрямую
    _cfg: Arc<Config>,    // Добавлен _ если не используется напрямую
    _db: Arc<Db>,        // Добавлен _ если не используется напрямую
    _running_operations: RunningOperations, // Добавлен _ если не используется напрямую
    chat_id: ChatId,
    params: HedgeParams,
    _initial_sum: f64, // Добавлен _ если не используется напрямую
    _volatility_percent: f64, // Добавлен _ если не используется напрямую
    initial_bot_message: MaybeInaccessibleMessage,
)
where
E: Exchange + Clone + Send + Sync + 'static,
{
    let _bot_message_id = match initial_bot_message { // Добавлен _ если не используется напрямую
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

    let _symbol_for_callback = params.symbol.clone(); // Добавлен _ если не используется напрямую
    warn!("spawn_sequential_hedge_task: Заглушка, реальная логика должна быть здесь, если функция используется.");
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

    info!("op_chat_id:{}: Preparing to spawn WS Hedge Task for {}...", chat_id, symbol);

    let operation_id = match insert_hedge_operation(
        db.as_ref(), chat_id.0, &symbol, &cfg.quote_currency, initial_sum,
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

    // --- Подключение к приватному WebSocket ---
    let private_subscriptions = vec![SubscriptionType::Order];
    // ИЗМЕНЕНИЕ ЗДЕСЬ: передаем (*cfg).clone() вместо cfg.clone()
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
    if !wait_for_specific_subscriptions(operation_id, &mut private_ws_receiver, &["order".to_string()], Duration::from_secs(10), "Private").await {
        let err_text = "❌ Не удалось подтвердить подписку 'order' на приватном WebSocket.".to_string();
        error!("op_id:{}: {}", operation_id, err_text);
        let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
        let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
        return Err(anyhow!(err_text));
    }
    info!("op_id:{}: 'order' subscription confirmed on private WebSocket.", operation_id);

    // --- Подключение к публичному СПОТ WebSocket ---
    let spot_symbol_ws = format!("{}{}", symbol, cfg.quote_currency);
    let ws_order_book_depth = cfg.ws_order_book_depth;
    let spot_orderbook_subscriptions = vec![
        SubscriptionType::Orderbook { symbol: spot_symbol_ws.clone(), depth: ws_order_book_depth }
    ];
    // ИЗМЕНЕНИЕ ЗДЕСЬ: передаем (*cfg).clone()
    let mut public_spot_receiver = match bybit_ws::connect_public_stream(
        (*cfg).clone(),
        "spot",
        spot_orderbook_subscriptions
    ).await {
        Ok(receiver) => {
            info!("op_id:{}: Public SPOT WebSocket connected for 'orderbook' topic.", operation_id);
            receiver
        },
        Err(e) => {
            error!("op_id:{}: Failed to connect Public SPOT WebSocket: {}", operation_id, e);
            let err_text = format!("❌ Ошибка подключения публичного SPOT WebSocket: {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
            let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
            return Err(e);
        }
    };
    let expected_spot_orderbook_topic = format!("orderbook.{}.{}", ws_order_book_depth, spot_symbol_ws);
    if !wait_for_specific_subscriptions(operation_id, &mut public_spot_receiver, &[expected_spot_orderbook_topic.clone()], Duration::from_secs(10), "Public SPOT").await {
        let err_text = format!("❌ Не удалось подтвердить подписку '{}' на публичном SPOT WebSocket.", expected_spot_orderbook_topic);
        error!("op_id:{}: {}", operation_id, err_text);
        let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
        let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
        return Err(anyhow!(err_text));
    }
    info!("op_id:{}: '{}' subscription confirmed on public SPOT WebSocket.", operation_id, expected_spot_orderbook_topic);

    // --- Подключение к публичному ЛИНЕАРНОМУ WebSocket ---
    let linear_symbol_ws = format!("{}{}", symbol, cfg.quote_currency);
    let linear_orderbook_subscriptions = vec![
        SubscriptionType::Orderbook { symbol: linear_symbol_ws.clone(), depth: ws_order_book_depth }
    ];
    // ИЗМЕНЕНИЕ ЗДЕСЬ: передаем (*cfg).clone()
    let mut public_linear_receiver = match bybit_ws::connect_public_stream(
        (*cfg).clone(),
        "linear",
        linear_orderbook_subscriptions
    ).await {
        Ok(receiver) => {
            info!("op_id:{}: Public LINEAR WebSocket connected for 'orderbook' topic.", operation_id);
            receiver
        },
        Err(e) => {
            error!("op_id:{}: Failed to connect Public LINEAR WebSocket: {}", operation_id, e);
            let err_text = format!("❌ Ошибка подключения публичного LINEAR WebSocket: {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
            let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
            return Err(e);
        }
    };
    let expected_linear_orderbook_topic = format!("orderbook.{}.{}", ws_order_book_depth, linear_symbol_ws);
    if !wait_for_specific_subscriptions(operation_id, &mut public_linear_receiver, &[expected_linear_orderbook_topic.clone()], Duration::from_secs(10), "Public LINEAR").await {
        let err_text = format!("❌ Не удалось подтвердить подписку '{}' на публичном LINEAR WebSocket.", expected_linear_orderbook_topic);
        error!("op_id:{}: {}", operation_id, err_text);
        let _ = bot.edit_message_text(chat_id, bot_message_id, err_text.clone()).reply_markup(navigation::make_main_menu_keyboard()).await;
        let _ = crate::storage::update_hedge_final_status(db.as_ref(), operation_id, "Failed", None, 0.0, Some(&err_text)).await;
        return Err(anyhow!(err_text));
    }
    info!("op_id:{}: '{}' subscription confirmed on public LINEAR WebSocket.", operation_id, expected_linear_orderbook_topic);


    let _ = bot.edit_message_text(chat_id, bot_message_id, format!("⏳ Инициализация WS стратегии для {} (ID: {})...", symbol, operation_id)).await;

    let bot_clone_for_callback = bot.clone();
    let cfg_for_callback = cfg.clone(); // Этот cfg уже Arc<Config>
    let symbol_for_callback = symbol.clone();

    let progress_callback: HedgeProgressCallback = Box::new(move |update: HedgeProgressUpdate| {
        let bot_cb = bot_clone_for_callback.clone();
        let qc_cb = cfg_for_callback.quote_currency.clone(); // cfg_for_callback это Arc<Config>, доступ к полям через .
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
            } else { 0.0 };

            let overall_stage_filled_percent = if total_target_qty_stage_cb > ORDER_FILL_TOLERANCE {
                (cumulative_filled_qty_stage_cb / total_target_qty_stage_cb) * 100.0
            } else { if cumulative_filled_qty_stage_cb > ORDER_FILL_TOLERANCE { 100.0 } else { 0.0 }};

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
        operation_id,
        request,
        cfg.clone(), // cfg здесь Arc<Config>
        db.clone(),
        exchange_rest.clone(),
        progress_callback,
        private_ws_receiver,
        public_spot_receiver,
        public_linear_receiver,
    ).await {
        Ok(task) => {
            info!("op_id:{}: HedgerWsHedgeTask initialized successfully with tri-WebSocket streams.", operation_id);
            task
        },
        Err(e) => {
            let error_text = format!("❌ Ошибка инициализации WS стратегии (tri-stream): {}", e);
            let _ = bot.edit_message_text(chat_id, bot_message_id, error_text.clone())
                         .reply_markup(navigation::make_main_menu_keyboard()).await;
            return Err(e);
        }
    };

    let bot_clone_for_spawn = bot.clone();
    let running_operations_clone = running_operations.clone();
    let symbol_clone_for_spawn = symbol.clone();
    let cfg_clone_for_spawn = cfg.clone(); // cfg здесь Arc<Config>

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
                    cfg_clone_for_spawn.quote_currency, // Используем quote_currency из cfg
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
    info!("op_id:{}: Stored running WS hedge info (tri-stream).", operation_id);

    Ok(())
}

async fn wait_for_specific_subscriptions(
    operation_id: i64,
    ws_receiver: &mut tokio::sync::mpsc::Receiver<Result<WebSocketMessage>>,
    expected_topics: &[String],
    timeout_duration: Duration,
    stream_name: &str,
) -> bool {
    let mut confirmed_topics_set = HashSet::new();
    let expected_set: HashSet<_> = expected_topics.iter().cloned().collect();

    info!("op_id:{}: Waiting for specific subscriptions on {} stream: {:?}...", operation_id, stream_name, expected_topics);

    match tokio::time::timeout(timeout_duration, async {
        while confirmed_topics_set.len() < expected_set.len() {
            match ws_receiver.recv().await {
                Some(Ok(WebSocketMessage::SubscriptionResponse { success, topic })) => {
                    info!("op_id:{}: Received subscription response on {} stream: success={}, topic='{}'", operation_id, stream_name, success, topic);
                    if success {
                        if !topic.is_empty() {
                            if expected_set.contains(&topic) {
                                confirmed_topics_set.insert(topic);
                            } else {
                                warn!("op_id:{}: Received unexpected successful subscription on {} stream for topic: '{}'", operation_id, stream_name, topic);
                            }
                        } else {
                            if expected_set.len() == 1 && confirmed_topics_set.is_empty() {
                                if let Some(single_expected_topic) = expected_set.iter().next() {
                                    if !single_expected_topic.is_empty() {
                                        info!("op_id:{}: Assuming subscription success for '{}' on {} stream due to success=true and empty topic in response (single expectation).", operation_id, single_expected_topic, stream_name);
                                        confirmed_topics_set.insert(single_expected_topic.clone());
                                    } else {
                                        warn!("op_id:{}: Expected single topic was empty string on {} stream, but received success=true with empty topic. Ambiguous.", operation_id, stream_name);
                                    }
                                }
                            } else if expected_set.len() > 1 && confirmed_topics_set.len() < expected_set.len() {
                                warn!("op_id:{}: Received success=true with empty topic on {} stream, but expected multiple topics ({:?}) and not all confirmed yet. Cannot map empty topic to a specific one.", operation_id, stream_name, expected_set);
                            } else {
                                warn!("op_id:{}: Received success=true with empty topic on {} stream, but state is: expected_set {:?}, confirmed_topics_set {:?}.", operation_id, stream_name, expected_set, confirmed_topics_set);
                            }
                        }
                    } else {
                        if !topic.is_empty() && expected_set.contains(&topic) {
                            error!("op_id:{}: Failed subscription on {} stream for expected topic: {}", operation_id, stream_name, topic);
                            return false;
                        } else if topic.is_empty() && !expected_set.is_empty() {
                             error!("op_id:{}: Subscription failed on {} stream with empty topic, expected {:?}.", operation_id, stream_name, expected_set);
                             return false;
                        }
                        warn!("op_id:{}: Received failed subscription response on {} stream for unexpected/unhandled topic: '{}'", operation_id, stream_name, topic);
                    }
                }
                Some(Ok(WebSocketMessage::Error(e))) => {
                    error!("op_id:{}: WebSocket error on {} stream during specific subscription wait: {}", operation_id, stream_name, e);
                    return false;
                }
                Some(Err(e)) => {
                    error!("op_id:{}: MPSC channel error on {} stream during specific subscription wait: {}", operation_id, stream_name, e);
                    return false;
                }
                None => {
                    error!("op_id:{}: MPSC channel closed on {} stream during specific subscription wait.", operation_id, stream_name);
                    return false;
                }
                _ => {}
            }
        }
        expected_set.iter().all(|expected| confirmed_topics_set.contains(expected))
    }).await {
        Ok(all_confirmed_successfully) => {
            if !all_confirmed_successfully {
                 error!("op_id:{}: Not all expected subscriptions were confirmed on {} stream after loop. Confirmed: {:?}, Expected: {:?}",
                    operation_id, stream_name, confirmed_topics_set, expected_topics);
            }
            all_confirmed_successfully
        }
        Err(_) => { // Таймаут
            error!("op_id:{}: Timed out waiting for specific subscriptions on {} stream. Confirmed: {:?}, Expected: {:?}",
                operation_id, stream_name, confirmed_topics_set, expected_topics);
            false
        }
    }
}