// src/telegram.rs

use teloxide::prelude::*;
use teloxide::dptree;
use teloxide::utils::command::BotCommands;
use crate::notifier::{self, Command};
use crate::exchange::Exchange;

pub async fn run<E>(bot: Bot, exchange: E)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    // only handle text messages that parse as our Command
    let handler = Update::filter_message()
        .filter_command::<Command>()
        .endpoint(
            move |bot: Bot, msg: Message, cmd: Command| {
                let ex = exchange.clone();
                async move {
                    if let Err(err) = notifier::handler(bot, msg, cmd, ex).await {
                        tracing::error!("handler error: {:?}", err);
                    }
                    respond(())
                }
            },
        );

    Dispatcher::builder(bot, dptree::entry().branch(handler))
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
