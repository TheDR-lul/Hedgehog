// src/notifier/mod.rs

// --- Подключение Модулей ---
pub mod navigation;
pub mod wallet_info;
pub mod market_info;
pub mod hedge_flow;
pub mod unhedge_flow;
pub mod active_ops;
// Заглушки
//pub mod progress;      // TODO: Реализовать
//pub mod utils;         // TODO: Реализовать

// --- Импорт Зависимостей и Типов ---
use std::sync::Arc;
// <<< ИЗМЕНЕНО: Убираем RwLock из std::sync >>>
// use std::sync::RwLock;
// <<< ДОБАВЛЕНО: Импортируем RwLock из tokio >>>
use tokio::sync::RwLock as TokioRwLock;
use teloxide::types::{ChatId, Message, CallbackQuery}; // Убран MessageId
use std::collections::HashMap;
use teloxide::utils::command::BotCommands;
use tokio::task::AbortHandle;
use tokio::sync::Mutex as TokioMutex; // Tokio Mutex для RunningOperations - OK
use crate::storage::{Db, HedgeOperation};
use crate::config::Config;
use crate::exchange::Exchange;
use teloxide::Bot;
use teloxide::prelude::Requester;
use teloxide::payloads::AnswerCallbackQuerySetters;
use tracing::{info, warn}; // Убран error

// --- Общие Типы Данных Модуля Notifier ---

#[derive(Debug, Clone)]
pub enum UserState {
    AwaitingHedgeAssetSelection { last_bot_message_id: Option<i32> },
    AwaitingHedgeSum { symbol: String, last_bot_message_id: Option<i32> },
    AwaitingHedgeVolatility { symbol: String, sum: f64, last_bot_message_id: Option<i32> },
    AwaitingHedgeConfirmation {
        symbol: String,
        sum: f64,
        volatility: f64,
        last_bot_message_id: Option<i32>,
    },
    AwaitingUnhedgeAssetSelection { last_bot_message_id: Option<i32> },
    AwaitingUnhedgeOperationSelection {
        symbol: String,
        operations: Vec<HedgeOperation>,
        last_bot_message_id: Option<i32>
    },
    AwaitingUnhedgeConfirmation {
        operation_id: i64,
        last_bot_message_id: Option<i32>,
    },
    ViewingAllPairs {
        current_page: usize,
        filter: Option<String>,
        pairs: Vec<String>,
        last_bot_message_id: Option<i32>
    },
    AwaitingFundingSymbolInput { last_bot_message_id: Option<i32> },
    None,
}

// <<< ИЗМЕНЕНО: Используем TokioRwLock >>>
pub type StateStorage = Arc<TokioRwLock<HashMap<ChatId, UserState>>>;
// ---

#[derive(Debug)]
pub struct RunningOperationInfo {
    pub handle: AbortHandle,
    pub operation_id: i64,
    pub operation_type: OperationType,
    pub symbol: String,
    pub bot_message_id: i32,
    pub total_filled_spot_qty: Arc<TokioMutex<f64>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationType {
    Hedge,
    Unhedge,
}

pub type RunningOperations = Arc<TokioMutex<HashMap<(ChatId, i64), RunningOperationInfo>>>;

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Доступные команды:")]
pub enum Command {
    #[command(description = "Начало работы и главное меню")]
    Start,
    #[command(description = "Статус бота и API")]
    Status,
    #[command(description = "Баланс кошелька")]
    Wallet,
    #[command(description = "Баланс монеты: /balance <SYMBOL>")]
    Balance(String),
    #[command(description = "Начать хеджирование: /hedge <SYMBOL> (опционально)")]
    Hedge(String),
    #[command(description = "Начать расхеджирование: /unhedge <SYMBOL> (опционально)")]
    Unhedge(String),
    #[command(description = "Средняя ставка финансирования: /funding <SYMBOL> [days]")]
    Funding(String),
    #[command(description = "Показать активные операции")]
    Active,
}

// --- Главные Диспетчеры ---

pub async fn dispatch_command<E>(
    bot: Bot,
    msg: Message,
    cmd: Command,
    exchange: Arc<E>,
    state_storage: StateStorage, // Теперь это Arc<TokioRwLock<...>>
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where E: Exchange + Clone + Send + Sync + 'static,
{
    match cmd {
        Command::Start => navigation::handle_start(bot, msg, exchange, state_storage, cfg, db).await?,
        Command::Hedge(symbol) => hedge_flow::handle_hedge_command(bot, msg, symbol, exchange, state_storage, running_operations, cfg, db).await?,
        Command::Unhedge(symbol) => unhedge_flow::handle_unhedge_command(bot, msg, symbol, exchange, state_storage, running_operations, cfg, db).await?,
        Command::Wallet => wallet_info::handle_wallet_command(bot, msg, exchange, state_storage, cfg, db).await?,
        Command::Balance(symbol) => wallet_info::handle_balance_command(bot, msg, symbol, exchange, state_storage, cfg, db).await?,
        Command::Status => market_info::handle_status_command(bot, msg, exchange, state_storage, cfg, db).await?,
        Command::Funding(params) => market_info::handle_funding_command(bot, msg, params, exchange, state_storage, cfg, db).await?,
        Command::Active => active_ops::handle_active_command(bot, msg, exchange, state_storage, running_operations, cfg, db).await?,
    }
    Ok(())
}


pub async fn dispatch_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: Arc<E>,
    state_storage: StateStorage, // Теперь это Arc<TokioRwLock<...>>
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let query_id = q.id.clone();

    if let Some(data) = q.data.as_deref() {
        info!("Dispatching callback: {}", data);

        if data == callback_data::BACK_TO_MAIN {
            navigation::handle_back_to_main(bot, q, state_storage).await?;
        } else if data == callback_data::CANCEL_DIALOG {
            if let Some(msg) = q.message.as_ref() {
                let chat_id = msg.chat().id;
                let message_id = msg.id();
                bot.answer_callback_query(query_id).await?;
                navigation::handle_cancel_dialog(bot, chat_id, message_id, state_storage).await?;
            } else {
                warn!("CallbackQuery missing message for CANCEL_DIALOG");
                bot.answer_callback_query(query_id).text("Не удалось отменить: сообщение недоступно.").show_alert(true).await?;
            }
        } else if data == callback_data::MENU_WALLET {
            wallet_info::handle_menu_wallet_callback(bot, q, exchange, cfg, db).await?;
        } else if data == callback_data::START_HEDGE {
            hedge_flow::handle_start_hedge_callback(bot, q, exchange, state_storage, cfg, db).await?;
        } else if data == callback_data::START_UNHEDGE {
            unhedge_flow::handle_start_unhedge_callback(bot, q, exchange, state_storage, cfg, db).await?;
        } else if data == callback_data::MENU_INFO {
            market_info::handle_menu_info_callback(bot, q, exchange, cfg, db).await?;
        } else if data == callback_data::MENU_ACTIVE_OPS {
            active_ops::handle_menu_active_ops_callback(bot, q, running_operations, state_storage).await?;
        } else if data.starts_with(callback_data::PREFIX_CANCEL_ACTIVE_OP) {
              active_ops::handle_cancel_active_op_callback(bot, q, exchange, state_storage, running_operations, cfg, db).await?;
        } else if data.starts_with(callback_data::PREFIX_HEDGE_ASSET) {
              hedge_flow::handle_hedge_asset_callback(bot, q, exchange, state_storage, cfg, db).await?;
        } else if data.starts_with(callback_data::PREFIX_HEDGE_PAIR) {
              warn!("Handler for PREFIX_HEDGE_PAIR not implemented yet.");
              bot.answer_callback_query(query_id).text("Функция пока не реализована.").show_alert(false).await?;
        } else if data.starts_with(callback_data::PREFIX_HEDGE_CONFIRM) {
              hedge_flow::handle_hedge_confirm_callback(bot, q, exchange, state_storage, running_operations, cfg, db).await?;
        } else if data == callback_data::VIEW_ALL_PAIRS {
              warn!("Handler for VIEW_ALL_PAIRS not implemented yet.");
              bot.answer_callback_query(query_id).text("Функция пока не реализована.").show_alert(false).await?;
        } else if data.starts_with(callback_data::PREFIX_UNHEDGE_ASSET) {
              unhedge_flow::handle_unhedge_asset_callback(bot, q, exchange, state_storage, cfg, db).await?;
        } else if data.starts_with(callback_data::PREFIX_UNHEDGE_OP_SELECT) {
              unhedge_flow::handle_unhedge_select_op_callback(bot, q, exchange, state_storage, running_operations, cfg, db).await?;
        } else if data.starts_with(callback_data::PREFIX_UNHEDGE_CONFIRM) {
              unhedge_flow::handle_unhedge_confirm_callback(bot, q, exchange, state_storage, running_operations, cfg, db).await?;
        } else if data == callback_data::SHOW_STATUS {
              market_info::handle_show_status_callback(bot, q, exchange, cfg, db).await?;
        } else if data == callback_data::SHOW_FUNDING {
              market_info::handle_show_funding_callback(bot, q, state_storage).await?;
        } else if data.starts_with(callback_data::PREFIX_PAGE_NEXT) || data.starts_with(callback_data::PREFIX_PAGE_PREV) {
              warn!("Pagination callback '{}' not implemented yet.", data);
              bot.answer_callback_query(query_id).text("Навигация по страницам пока не работает").show_alert(false).await?;
        } else {
              warn!("Unhandled callback data: {}", data);
              bot.answer_callback_query(query_id).text("Неизвестное действие.").show_alert(false).await?;
        }
    } else {
        warn!("CallbackQuery received without data");
        bot.answer_callback_query(query_id).await?;
    }

    Ok(())
}

/// Главный диспетчер текстовых сообщений
pub async fn dispatch_message<E>(
    bot: Bot,
    msg: Message,
    exchange: Arc<E>,
    state_storage: StateStorage, // Теперь это Arc<TokioRwLock<...>>
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    // <<< ИЗМЕНЕНО: Асинхронное чтение состояния >>>
    let state = { state_storage.read().await.get(&msg.chat.id).cloned().unwrap_or(UserState::None) };
    info!("Dispatching message for chat {} in state: {:?}", msg.chat.id, state);

    match state {
        UserState::AwaitingHedgeAssetSelection { .. } | UserState::ViewingAllPairs { .. } =>
            hedge_flow::handle_asset_ticker_input(bot, msg, exchange, state_storage, cfg, db).await?,
        UserState::AwaitingHedgeSum { .. } => hedge_flow::handle_sum_input(bot, msg, state_storage, cfg).await?,
        UserState::AwaitingHedgeVolatility { .. } => hedge_flow::handle_volatility_input(bot, msg, exchange, state_storage, running_operations, cfg, db).await?,
        UserState::AwaitingFundingSymbolInput { .. } =>
            market_info::handle_funding_symbol_input(bot, msg, exchange, state_storage, cfg, db).await?,
        UserState::AwaitingUnhedgeAssetSelection { .. } |
        UserState::AwaitingUnhedgeOperationSelection { .. } |
        UserState::AwaitingUnhedgeConfirmation { .. } => {
            warn!("Handler for state {:?} (text input) not implemented yet. Deleting message.", state);
            if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await {
                 tracing::warn!("Failed to delete unhandled message: {}", e);
            }
        },
        _ => {
            if msg.text().map_or(false, |t| t.starts_with('/')) {
                warn!("Received command message '{}' in dispatch_message. Should have been handled by dispatch_command.", msg.text().unwrap_or(""));
            } else if msg.text().is_some() {
                warn!("Received unexpected text message in state {:?}. Deleting.", state);
                if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await {
                    tracing::warn!("Failed to delete unexpected message: {}", e);
                }
            }
        }
    }

    Ok(())
}


// --- Константы для Callback Data ---
pub mod callback_data {
    // Навигация
    pub const BACK_TO_MAIN: &str = "back_main";
    pub const CANCEL_DIALOG: &str = "cancel_dialog";

    // Главное Меню
    pub const MENU_WALLET: &str = "menu_wallet";
    pub const MENU_MANAGE: &str = "menu_manage"; // Если будет подменю
    pub const MENU_INFO: &str = "menu_info";
    pub const MENU_ACTIVE_OPS: &str = "menu_active";

    // Управление Позициями
    pub const START_HEDGE: &str = "start_hedge";
    pub const START_UNHEDGE: &str = "start_unhedge";
    pub const VIEW_ALL_PAIRS: &str = "view_all_pairs";

    // Префиксы выбора для Хеджирования
    pub const PREFIX_HEDGE_ASSET: &str = "h_asset_";
    pub const PREFIX_HEDGE_PAIR: &str = "h_pair_";

    // Префиксы выбора для Расхеджирования
    pub const PREFIX_UNHEDGE_ASSET: &str = "u_asset_";
    pub const PREFIX_UNHEDGE_OP_SELECT: &str = "u_opsel_";

    // Префиксы Подтверждения
    pub const PREFIX_HEDGE_CONFIRM: &str = "h_conf_";
    pub const PREFIX_UNHEDGE_CONFIRM: &str = "u_conf_";

    // Префиксы Отмены Активных Операций
    pub const PREFIX_CANCEL_ACTIVE_OP: &str = "cancel_op_";

    // Информация
    pub const SHOW_STATUS: &str = "show_status";
    pub const SHOW_FUNDING: &str = "show_funding";

    // Пагинация (context_page_num)
    pub const PREFIX_PAGE_NEXT: &str = "page_next_";
    pub const PREFIX_PAGE_PREV: &str = "page_prev_";
}