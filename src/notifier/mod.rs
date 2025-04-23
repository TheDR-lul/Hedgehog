// src/notifier/mod.rs

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
    AwaitingSum { symbol: String, last_bot_message_id: Option<i32> }, // Для хеджирования
    AwaitingVolatility { symbol: String, sum: f64, last_bot_message_id: Option<i32> }, // Для хеджирования
    AwaitingUnhedgeQuantity { symbol: String, last_bot_message_id: Option<i32> }, // Для расхеджирования
    None,
}

/// Тип для хранения состояний пользователей
pub type StateStorage = Arc<RwLock<HashMap<ChatId, UserState>>>;

// --- ИЗМЕНЕНО: Добавлено #[derive(Debug)] ---
#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Доступные команды:")]
pub enum Command {
    #[command(description = "показать это сообщение", aliases = ["help", "?"])]
    Help,
    #[command(description = "проверить статус")]
    Status,
    #[command(description = "список всего баланса: /wallet")]
    Wallet,
    #[command(description = "баланс монеты: /balance <symbol>")]
    Balance(String),
    #[command(description = "начать диалог хеджирования: /hedge <symbol>")]
    Hedge(String),
    #[command(description = "расхеджировать напрямую: /unhedge <quantity> <symbol>")]
    Unhedge(String),
    #[command(description = "средняя ставка финансирования: /funding <symbol> [days]")]
    Funding(String),
}