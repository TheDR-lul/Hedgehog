// src/telegram.rs

use std::sync::Arc;
use teloxide::{
    prelude::*,
    dptree,
    types::CallbackQuery,
    utils::command::BotCommands,
};
use crate::notifier::{self, Command};
use crate::exchange::Exchange;

pub async fn run<E>(bot: Bot, exchange: E)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    // Оборачиваем exchange в Arc, чтобы его можно было клонировать в замыканиях
    let exchange = Arc::new(exchange);

    // 1) Обработка текстовых команд /help, /status, /hedge, /unhedge
    let commands_branch = Update::filter_message()
        .filter_command::<Command>()
        .endpoint({
            let exchange = exchange.clone();
            move |bot: Bot, msg: Message, cmd: Command| {
                let exchange = exchange.clone();
                async move {
                    if let Err(err) = notifier::handle_command(
                        bot.clone(),
                        msg.clone(),
                        cmd,
                        (*exchange).clone(),
                    )
                    .await
                    {
                        tracing::error!("command handler error: {:?}", err);
                    }
                    respond(())
                }
            }
        });

    // 2) Обработка inline‑callback кнопок
    let callback_branch = Update::filter_callback_query()
        .endpoint({
            let exchange = exchange.clone();
            move |bot: Bot, q: CallbackQuery| {
                let exchange = exchange.clone();
                async move {
                    if let Err(err) = notifier::handle_callback(
                        bot.clone(),
                        q.clone(),
                        (*exchange).clone(),
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
