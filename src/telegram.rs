// src/telegram.rs

use crate::config::Config;
use crate::notifier::{
    Command, StateStorage, RunningHedges,
    handle_command, handle_callback, handle_message
};
use tokio::sync::Mutex as TokioMutex;
use teloxide::{
    prelude::*,
    dptree,
    types::{CallbackQuery, Message},
};
use crate::exchange::Exchange;
// --- ДОБАВЛЕНО: Импортируем Db ---
use crate::storage::Db;
// --- Конец добавления ---
use std::sync::Arc;
use std::sync::RwLock;
use std::collections::HashMap;

// --- ИЗМЕНЕНО: Принимаем db: Db ---
pub async fn run<E>(bot: Bot, exchange: E, cfg: Config, db: Db)
// --- Конец изменений ---
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let exchange = Arc::new(exchange);
    let state_storage: StateStorage = Arc::new(RwLock::new(HashMap::new()));
    let running_hedges: RunningHedges = Arc::new(TokioMutex::new(HashMap::new()));

    let cfg_for_commands = cfg.clone();
    let cfg_for_callbacks = cfg.clone();
    let cfg_for_messages = cfg.clone();
    // --- ДОБАВЛЕНО: Клонируем пул БД ---
    let db_for_commands = db.clone();
    let db_for_callbacks = db.clone();
    let db_for_messages = db.clone();
    // --- Конец добавления ---


    // 1) Текстовые команды
    let commands_branch = Update::filter_message()
        .filter_command::<Command>()
        .endpoint({
            let exchange = exchange.clone();
            let state_storage = state_storage.clone();
            let running_hedges = running_hedges.clone();
            // --- ДОБАВЛЕНО: Захватываем db ---
            let db = db_for_commands.clone();
            // --- Конец добавления ---
            move |bot: Bot, msg: Message, cmd: Command| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                let cfg = cfg_for_commands.clone();
                let _running_hedges_clone = running_hedges.clone();
                // --- ДОБАВЛЕНО: Клонируем db ---
                let db = db.clone();
                // --- Конец добавления ---
                async move {
                    // --- ИЗМЕНЕНО: Передаем db ---
                    if let Err(err) = handle_command(bot, msg, cmd, (*exchange).clone(), state_storage, cfg, &db).await {
                    // --- Конец изменений ---
                        tracing::error!("command handler error: {:?}", err);
                    }
                    respond(())
                }
            }
        });

    // 2) Inline‑callbacks
    let callback_branch = Update::filter_callback_query()
        .endpoint({
            let exchange = exchange.clone();
            let state_storage = state_storage.clone();
            let running_hedges = running_hedges.clone();
            // --- ДОБАВЛЕНО: Захватываем db ---
            let db = db_for_callbacks.clone();
            // --- Конец добавления ---
            move |bot: Bot, q: CallbackQuery| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                let cfg = cfg_for_callbacks.clone();
                let running_hedges = running_hedges.clone();
                // --- ДОБАВЛЕНО: Клонируем db ---
                let db = db.clone();
                // --- Конец добавления ---
                async move {
                    // --- ИЗМЕНЕНО: Передаем db ---
                    if let Err(err) = handle_callback(bot, q, (*exchange).clone(), state_storage, cfg, running_hedges, &db).await {
                    // --- Конец изменений ---
                        tracing::error!("callback handler error: {:?}", err);
                    }
                    respond(())
                }
            }
        });

    // 3) Текстовые сообщения
    let message_branch = Update::filter_message()
        .endpoint({
            let exchange = exchange.clone();
            let state_storage = state_storage.clone();
            let running_hedges = running_hedges.clone();
            // --- ДОБАВЛЕНО: Захватываем db ---
            let db = db_for_messages.clone();
            // --- Конец добавления ---
            move |bot: Bot, msg: Message| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                let cfg = cfg_for_messages.clone();
                let running_hedges = running_hedges.clone();
                // --- ДОБАВЛЕНО: Клонируем db ---
                let db = db.clone();
                // --- Конец добавления ---
                async move {
                    // --- ИЗМЕНЕНО: Передаем db ---
                    if let Err(err) = handle_message(bot, msg, state_storage, (*exchange).clone(), cfg, running_hedges, &db).await {
                    // --- Конец изменений ---
                        tracing::error!("message handler error: {:?}", err);
                    }
                    respond(())
                }
            }
        });

    // Собираем все ветки в Dispatcher
    Dispatcher::builder(bot, dptree::entry()
        .branch(commands_branch)
        .branch(callback_branch)
        .branch(message_branch))
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
