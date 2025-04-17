// src/telegram.rs

use teloxide::prelude::*;
use teloxide::commands::repl;                // <- импорт repl
use teloxide::utils::command::BotCommands;
use crate::notifier::{Command, handler};
use crate::exchange::Exchange;

/// Запускает бот на основе встроенного REPL команд
pub async fn run<E>(bot: AutoSend<Bot>, exchange: E)
where
    E: Exchange + Send + Sync + Clone + 'static,
{
    repl(bot, "hedgehog_bot", move |cx, cmd: Command| {
        let ex = exchange.clone();
        async move {
            handler(cx, cmd, &ex).await;
            Ok(())
        }
    })
    .await;
}