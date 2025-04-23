// src/telegram.rs

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

// --- ИЗМЕНЕНО: Принимаем quote_currency ---
pub async fn run<E>(bot: Bot, exchange: E, quote_currency: String)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let exchange = Arc::new(exchange);
    let state_storage: StateStorage = Arc::new(RwLock::new(HashMap::new()));
    // --- ИЗМЕНЕНО: Клонируем quote_currency для передачи в замыкания ---
    let qc_for_commands = quote_currency.clone();
    let qc_for_callbacks = quote_currency.clone();
    let qc_for_messages = quote_currency.clone();
    // --- Конец изменений ---

    // 1) Текстовые команды
    let commands_branch = Update::filter_message()
        .filter_command::<Command>()
        .endpoint({
            let exchange = exchange.clone();
            let state_storage = state_storage.clone();
            // --- ИЗМЕНЕНО: Захватываем qc_for_commands ---
            move |bot: Bot, msg: Message, cmd: Command| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                let quote_currency = qc_for_commands.clone(); // Клонируем для async блока
                // --- Конец изменений ---
                async move {
                    // --- ИЗМЕНЕНО: Передаем quote_currency ---
                    if let Err(err) = handle_command(bot, msg, cmd, (*exchange).clone(), state_storage, quote_currency).await {
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
             // --- ИЗМЕНЕНО: Захватываем qc_for_callbacks ---
            move |bot: Bot, q: CallbackQuery| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                let quote_currency = qc_for_callbacks.clone(); // Клонируем для async блока
                // --- Конец изменений ---
                async move {
                     // --- ИЗМЕНЕНО: Передаем quote_currency ---
                    if let Err(err) = handle_callback(bot, q, (*exchange).clone(), state_storage, quote_currency).await {
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
            // --- ИЗМЕНЕНО: Захватываем qc_for_messages ---
            move |bot: Bot, msg: Message| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                let quote_currency = qc_for_messages.clone(); // Клонируем для async блока
                // --- Конец изменений ---
                async move {
                    // --- ИЗМЕНЕНО: Передаем quote_currency ---
                    if let Err(err) = handle_message(bot, msg, state_storage, (*exchange).clone(), quote_currency).await {
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
