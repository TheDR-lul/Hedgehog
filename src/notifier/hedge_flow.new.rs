// src/notifier/hedge_flow.rs

// Реэкспортируем публичные обработчики, если нужно
// pub use hedge_flow_logic::handlers::handle_hedge_command;
// pub use hedge_flow_logic::handlers::handle_start_hedge_callback;
// pub use hedge_flow_logic::handlers::handle_hedge_asset_callback;
// ... и т.д.

// Или можно оставить этот файл почти пустым,
// а в src/notifier/mod.rs импортировать и использовать функции напрямую из hedge_flow_logic::handlers
// Например:
// use hedge_flow_logic::handlers;
// ...
// handlers::handle_hedge_command(...)
// ...

// --- ДОБАВЛЯЕМ ИСПОЛЬЗОВАНИЕ НОВОГО МОДУЛЯ ---
use crate::config::Config;
use crate::exchange::Exchange;
use crate::storage::Db;
use crate::notifier::{StateStorage, RunningOperations};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{Message, CallbackQuery};

mod hedge_flow_logic; // Объявляем новый подмодуль

// --- Переносим публичные функции сюда или реэкспортируем ---

// --- Обработчики команд и колбэков ---

/// Обработчик команды /hedge [SYMBOL]
pub async fn handle_hedge_command<E>(
    bot: Bot, msg: Message, symbol_arg: String, exchange: Arc<E>, state_storage: StateStorage,
    running_operations: RunningOperations, cfg: Arc<Config>, db: Arc<Db>
) -> anyhow::Result<()>
where E: Exchange + Clone + Send + Sync + 'static {
    hedge_flow_logic::handlers::handle_hedge_command(bot, msg, symbol_arg, exchange, state_storage, running_operations, cfg, db).await
}

/// Обработчик колбэка кнопки "Захеджировать" из главного меню
pub async fn handle_start_hedge_callback<E>(
    bot: Bot, q: CallbackQuery, exchange: Arc<E>, state_storage: StateStorage, cfg: Arc<Config>, db: Arc<Db>
) -> anyhow::Result<()>
where E: Exchange + Clone + Send + Sync + 'static {
    hedge_flow_logic::handlers::handle_start_hedge_callback(bot, q, exchange, state_storage, cfg, db).await
}

/// Обработчик колбэка выбора актива для хеджа
pub async fn handle_hedge_asset_callback<E>(
    bot: Bot, q: CallbackQuery, exchange: Arc<E>, state_storage: StateStorage, cfg: Arc<Config>, db: Arc<Db>
) -> anyhow::Result<()>
where E: Exchange + Clone + Send + Sync + 'static {
    hedge_flow_logic::handlers::handle_hedge_asset_callback(bot, q, exchange, state_storage, cfg, db).await
}

/// Обработчик ручного ввода тикера
pub async fn handle_asset_ticker_input<E>(
    bot: Bot, msg: Message, exchange: Arc<E>, state_storage: StateStorage, cfg: Arc<Config>, db: Arc<Db>
) -> anyhow::Result<()>
where E: Exchange + Clone + Send + Sync + 'static {
    hedge_flow_logic::handlers::handle_asset_ticker_input(bot, msg, exchange, state_storage, cfg, db).await
}

/// Обработчик ввода суммы
pub async fn handle_sum_input(
    bot: Bot, msg: Message, state_storage: StateStorage, cfg: Arc<Config>
) -> anyhow::Result<()> {
    hedge_flow_logic::handlers::handle_sum_input(bot, msg, state_storage, cfg).await
}

/// Обработчик ввода волатильности
pub async fn handle_volatility_input<E>(
    bot: Bot, msg: Message, exchange: Arc<E>, state_storage: StateStorage,
    running_operations: RunningOperations, cfg: Arc<Config>, db: Arc<Db>
) -> anyhow::Result<()>
where E: Exchange + Clone + Send + Sync + 'static {
    hedge_flow_logic::handlers::handle_volatility_input(bot, msg, exchange, state_storage, running_operations, cfg, db).await
}

/// Обработчик колбэка подтверждения хеджа
pub async fn handle_hedge_confirm_callback<E>(
    bot: Bot, q: CallbackQuery, exchange: Arc<E>, state_storage: StateStorage,
    running_operations: RunningOperations, cfg: Arc<Config>, db: Arc<Db>
) -> anyhow::Result<()>
where E: Exchange + Clone + Send + Sync + 'static {
    hedge_flow_logic::handlers::handle_hedge_confirm_callback(bot, q, exchange, state_storage, running_operations, cfg, db).await
}