// src/telegram.rs

use std::sync::Arc;
use teloxide::{
    prelude::*,
    dptree,
    types::CallbackQuery,
};
use tokio::sync::RwLock;
use std::collections::HashMap;
use crate::notifier::{self, Command};
use crate::exchange::Exchange;

pub async fn run<E>(bot: Bot, exchange: E)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    // Оборачиваем exchange в Arc, чтобы можно было клонировать его в замыканиях
    let exchange = Arc::new(exchange);

    // Создаем state_storage для хранения состояний пользователей
    let state_storage: notifier::StateStorage = Arc::new(RwLock::new(HashMap::new()));
    
    // 1) Текстовые команды
    let commands_branch = Update::filter_message()
        .filter_command::<Command>()
        .endpoint({
            let exchange = exchange.clone();
            let state_storage = state_storage.clone(); // Клонируем state_storage
            move |bot: Bot, msg: Message, cmd: Command| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone(); // Передаем state_storage
                async move {
                    if let Err(err) = notifier::handle_command(
                        bot.clone(),
                        msg.clone(),
                        cmd,
                        (*exchange).clone(),
                        state_storage.clone(), // Передаем state_storage
                    )
                    .await
                    {
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
            let state_storage = state_storage.clone(); // Клонируем state_storage
            move |bot: Bot, q: CallbackQuery| {
                let exchange = exchange.clone();
                let state_storage = state_storage.clone(); // Передаем state_storage
                async move {
                    if let Err(err) = notifier::handle_callback(
                        bot.clone(),
                        q.clone(),
                        (*exchange).clone(),
                        state_storage.clone(), // Передаем state_storage
                    )
                    .await
                    {
                        tracing::error!("callback handler error: {:?}", err);
                    }
                    respond(())
                }
            }
        });

    Dispatcher::builder(bot, dptree::entry()
        .branch(commands_branch)
        .branch(callback_branch))
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}