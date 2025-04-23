pub mod commands;
pub mod callbacks;
pub mod messages;

// Экспорт всех необходимых типов и функций
pub use self::commands::handle_command;
pub use self::callbacks::handle_callback;
pub use self::messages::handle_message;

use std::sync::{Arc, RwLock};
use teloxide::types::ChatId;
use std::collections::HashMap;
use teloxide::utils::command::BotCommands;

/// Определение состояний пользователя
#[derive(Debug, Clone)]
pub enum UserState {
    AwaitingAssetSelection { last_bot_message_id: Option<i32> },
    AwaitingSum { symbol: String, last_bot_message_id: Option<i32> },
    AwaitingVolatility { symbol: String, sum: f64, last_bot_message_id: Option<i32> },
    None,
}

/// Тип для хранения состояний пользователей
pub type StateStorage = Arc<RwLock<HashMap<ChatId, UserState>>>;

/// Все доступные команды бота
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Доступные команды:")]
/// Используется для генерации описания команд и их обработки
pub enum Command {
    #[command(description = "показать это сообщение", aliases = ["help", "?"])]
    Help,
    #[command(description = "проверить статус")]
    Status,
    #[command(description = "список всего баланса: /wallet")]
    Wallet,
    #[command(description = "баланс монеты: /balance <symbol>")]
    Balance(String),
    #[command(description = "захеджировать: /hedge <sum> <symbol> <volatility %>")]
    Hedge(String),
    #[command(description = "расхеджировать: /unhedge <sum> <symbol>")]
    Unhedge(String),
    #[command(description = "средняя ставка финансирования: /funding <symbol> [days]")]
    Funding(String),
}