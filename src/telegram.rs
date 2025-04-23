// src/telegram.rs

use crate::config::Config; // Добавляем импорт Config
use crate::notifier::{Command, StateStorage, handle_command, handle_callback, handle_message};
use teloxide::{
    prelude::*,
    dptree,
    types::{CallbackQuery, Message},
};
use crate::exchange::Exchange;
use std::sync::Arc;
use std::sync::RwLock;
use std::collections::HashMap;

// --- ИЗМЕНЕНО: Принимаем cfg: Config ---
pub async fn run<E>(bot: Bot, exchange: E, cfg: Config)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let exchange = Arc::new(exchange);
    let state_storage: StateStorage = Arc::new(RwLock::new(HashMap::new()));
    // --- ИЗМЕНЕНО: Клонируем cfg для передачи в замыкания ---
    let cfg_for_commands = cfg.clone();
    let cfg_for_callbacks = cfg.clone();
    let cfg_for_messages = cfg.clone();
    // --- Конец изменений ---

    // 1) Текстовые команды
    let commands_branch = Update::filter_message()
        .filter_command::<Command>()
        .endpoint({
            let exchange = exchange.clone();
            let state_storage = state_storage.clone();
            // --- ИЗМЕНЕНО: Захватываем cfg_for_commands ---
            move |bot: Bot, msg: Message, cmd: Command| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                let cfg = cfg_for_commands.clone(); // Клонируем для async блока
                // --- Конец изменений ---
                async move {
                    // --- ИЗМЕНЕНО: Передаем cfg ---
                    if let Err(err) = handle_command(bot, msg, cmd, (*exchange).clone(), state_storage, cfg).await {
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
            // --- ИЗМЕНЕНО: Захватываем cfg_for_callbacks ---
            move |bot: Bot, q: CallbackQuery| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                let cfg = cfg_for_callbacks.clone(); // Клонируем для async блока
                // --- Конец изменений ---
                async move {
                    // --- ИЗМЕНЕНО: Передаем cfg ---
                    if let Err(err) = handle_callback(bot, q, (*exchange).clone(), state_storage, cfg).await {
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
            // --- ИЗМЕНЕНО: Захватываем cfg_for_messages ---
            move |bot: Bot, msg: Message| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                let cfg = cfg_for_messages.clone(); // Клонируем для async блока
                // --- Конец изменений ---
                async move {
                    // --- ИЗМЕНЕНО: Передаем cfg ---
                    if let Err(err) = handle_message(bot, msg, state_storage, (*exchange).clone(), cfg).await {
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