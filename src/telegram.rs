use teloxide::prelude::*;
use crate::notifier::{Command, handler};
use crate::exchange::Exchange;

/// Запускает Telegram-бота с командами хеджирования
pub async fn run<E>(bot: AutoSend<Bot>, exchange: E)
where
    E: Exchange + Send + Sync + Clone + 'static,
{
    teloxide::commands_repl(bot, move |msg, cmd: Command| {
        let exchange = exchange.clone();
        async move {
            handler(msg, cmd, &exchange).await;
        }
    })
    .await;
}
