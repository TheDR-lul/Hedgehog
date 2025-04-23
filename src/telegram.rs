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

pub async fn run<E>(bot: Bot, exchange: E)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let exchange = Arc::new(exchange);
    let state_storage: StateStorage = Arc::new(RwLock::new(HashMap::new()));

    // 1) Текстовые команды
    let commands_branch = Update::filter_message()
        .filter_command::<Command>()
        .endpoint({
            let exchange = exchange.clone();
            let state_storage = state_storage.clone();
            move |bot: Bot, msg: Message, cmd: Command| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                async move {
                    if let Err(err) = handle_command(bot, msg, cmd, (*exchange).clone(), state_storage).await {
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
            move |bot: Bot, q: CallbackQuery| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                async move {
                    if let Err(err) = handle_callback(bot, q, (*exchange).clone(), state_storage).await {
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
            move |bot: Bot, msg: Message| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone();
                async move {
                    if let Err(err) = handle_message(bot, msg, state_storage, (*exchange).clone()).await {
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