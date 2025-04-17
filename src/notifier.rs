
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;  // <- derive для команд
use crate::exchange::Exchange;
use crate::models::{HedgeRequest, UnhedgeRequest};

#[derive(BotCommands, Clone)]
#[command(rename = "lowercase", description = "Хедж бот команды:")]
pub enum Command {
    #[command(description = "показать статус")]
    Status,
    #[command(description = "захеджировать: /hedge <sum> <symbol> <volatility %>")]
    Hedge(String),
    #[command(description = "расхеджировать: /unhedge <sum> <symbol>")]
    Unhedge(String),
}

pub async fn handler(
    cx: UpdateWithCx<AutoSend<Bot>, Message>,
    cmd: Command,
    exchange: &impl Exchange,
) {
    match cmd {
        Command::Status => {
            cx.answer("Бот работает").await.unwrap();
        }
        Command::Hedge(args) => {
            // TODO: парсинг args → HedgeRequest → hedger.run_hedge
        }
        Command::Unhedge(args) => {
            // TODO: парсинг args → UnhedgeRequest → hedger.run_unhedge
        }
    }
}
