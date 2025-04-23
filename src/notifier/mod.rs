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
// --- ДОБАВЛЕНО: Для отмены ---
use tokio::task::AbortHandle;
use tokio::sync::Mutex as TokioMutex; // Используем Tokio Mutex для async
// --- Конец добавления ---

/// Определение состояний пользователя (для диалога)
#[derive(Debug, Clone)]
pub enum UserState {
    AwaitingAssetSelection { last_bot_message_id: Option<i32> },
    AwaitingSum { symbol: String, last_bot_message_id: Option<i32> },
    AwaitingVolatility { symbol: String, sum: f64, last_bot_message_id: Option<i32> },
    AwaitingUnhedgeQuantity { symbol: String, last_bot_message_id: Option<i32> },
    None,
}

/// Тип для хранения состояний пользователей (для диалога)
pub type StateStorage = Arc<RwLock<HashMap<ChatId, UserState>>>;

// --- ДОБАВЛЕНО: Информация о запущенном хеджировании ---
#[derive(Debug)] // Не Clone, т.к. AbortHandle не Clone
pub struct RunningHedgeInfo {
    pub handle: AbortHandle,
    pub current_order_id: Arc<TokioMutex<Option<String>>>, // ID текущего спот ордера
    pub total_filled_qty: Arc<TokioMutex<f64>>, // Сколько уже куплено на споте
    pub symbol: String, // Символ актива
    pub bot_message_id: i32, // ID сообщения с кнопкой отмены
}

/// Тип для хранения информации о запущенных процессах хеджирования
/// Ключ: (ChatId, Symbol)
pub type RunningHedges = Arc<TokioMutex<HashMap<(ChatId, String), RunningHedgeInfo>>>;
// --- Конец добавления ---


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

// --- ЗАМЕТКА: Использование Базы Данных ---
// Глобальная переменная DB (OnceCell<storage::Db>) инициализируется в main.rs,
// но на данный момент не используется в модулях notifier или hedger.
// В будущем ее можно использовать для:
// 1. Сохранения истории операций хеджирования/расхеджирования.
// 2. Персистентного хранения состояния пользователя (вместо StateStorage в памяти).
// 3. Хранения информации об активных ордерах для восстановления после перезапуска.
// --- Конец заметки ---
