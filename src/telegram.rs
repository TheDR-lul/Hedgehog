// src/telegram.rs

use teloxide::prelude::*;
use teloxide::dptree;
use crate::notifier::{self, Command};
use crate::exchange::Exchange;

/// Запускает Telegram‑бота
pub async fn run<E>(bot: Bot, exchange: E)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    // Ветка, реагирующая только на сообщения‑команды
    let commands_handler = Update::filter_message()
        .filter_command::<Command>()        // парсим текст в Command
        .endpoint(move |bot: Bot, msg: Message, cmd: Command| {
            let ex = exchange.clone();
            async move {
                // передаём управление в notifier; ошибки логируем
                if let Err(err) = notifier::handler(bot, msg, cmd, ex).await {
                    tracing::error!("handler error: {:?}", err);
                }
                respond(())
            }
        });

    // Dispatcher со встроенной обработкой Ctrl‑C
    Dispatcher::builder(bot, dptree::entry().branch(commands_handler))
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}