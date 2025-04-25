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
use tokio::task::AbortHandle;
use tokio::sync::Mutex as TokioMutex;
// --- ДОБАВЛЕНО: Импорт HedgeOperation ---
use crate::storage::HedgeOperation;
// --- КОНЕЦ ДОБАВЛЕНИЯ ---

/// Определение состояний пользователя (для диалога)
#[derive(Debug, Clone)]
pub enum UserState {
    AwaitingAssetSelection { action_type: String, last_bot_message_id: Option<i32> }, // Добавили тип действия
    AwaitingSum { symbol: String, last_bot_message_id: Option<i32> },
    AwaitingVolatility { symbol: String, sum: f64, last_bot_message_id: Option<i32> },
    // --- ИЗМЕНЕНО: Состояние для выбора операции расхеджирования ---
    AwaitingUnhedgeSelection {
        symbol: String,
        operations: Vec<HedgeOperation>, // Храним найденные операции
        last_bot_message_id: Option<i32>
    },
    // --- УДАЛЕНО: Состояние AwaitingUnhedgeQuantity ---
    None,
}

/// Тип для хранения состояний пользователей (для диалога)
pub type StateStorage = Arc<RwLock<HashMap<ChatId, UserState>>>;

#[derive(Debug)]
pub struct RunningHedgeInfo {
    pub handle: AbortHandle,
    pub current_order_id: Arc<TokioMutex<Option<String>>>,
    pub total_filled_qty: Arc<TokioMutex<f64>>,
    pub symbol: String,
    pub bot_message_id: i32,
    pub operation_id: i64,
}

/// Тип для хранения информации о запущенных процессах хеджирования
/// Ключ: (ChatId, Symbol)
pub type RunningHedges = Arc<TokioMutex<HashMap<(ChatId, String), RunningHedgeInfo>>>;


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
    // --- ИЗМЕНЕНО: Команда /unhedge принимает символ ---
    #[command(description = "начать диалог расхеджирования: /unhedge <symbol>")]
    Unhedge(String),
    // --- КОНЕЦ ИЗМЕНЕНИЯ ---
    #[command(description = "средняя ставка финансирования: /funding <symbol> [days]")]
    Funding(String),
}