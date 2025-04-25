// src/notifier/mod.rs

// --- Подключение Модулей ---
pub mod navigation;
pub mod wallet_info;
pub mod market_info;
pub mod hedge_flow;
pub mod unhedge_flow;
pub mod active_ops;
// Заглушки
//pub mod progress;     // TODO: Реализовать
//pub mod utils;        // TODO: Реализовать

// --- Импорт Зависимостей и Типов ---
use std::sync::{Arc, RwLock};
use teloxide::types::{ChatId, Message, MessageId, CallbackQuery};
use std::collections::HashMap;
use teloxide::utils::command::BotCommands;
use tokio::task::AbortHandle;
use tokio::sync::Mutex as TokioMutex;
use crate::storage::{Db, HedgeOperation};
use crate::config::Config;
use crate::exchange::Exchange;
use teloxide::Bot;
use teloxide::prelude::Requester; // Нужен для методов типа .answer() или .edit_*
use teloxide::payloads::AnswerCallbackQuerySetters; // <<<===== ДОБАВЛЕН ИМПОРТ
use tracing::{info, warn};

// --- Общие Типы Данных Модуля Notifier ---

#[derive(Debug, Clone)]
pub enum UserState {
    // ... (остальные состояния без изменений) ...
    AwaitingHedgeAssetSelection { last_bot_message_id: Option<i32> },
    AwaitingHedgeSum { symbol: String, last_bot_message_id: Option<i32> },
    AwaitingHedgeVolatility { symbol: String, sum: f64, last_bot_message_id: Option<i32> },
    AwaitingHedgeConfirmation {
        symbol: String,
        sum: f64,
        volatility: f64,
        last_bot_message_id: Option<i32>,
    },

    // Состояния для расхеджирования
    AwaitingUnhedgeAssetSelection { last_bot_message_id: Option<i32> }, // TODO: Использовать при реализации выбора актива
    AwaitingUnhedgeOperationSelection {
        symbol: String,
        operations: Vec<HedgeOperation>,
        last_bot_message_id: Option<i32>
    },
    AwaitingUnhedgeConfirmation {
        operation_id: i64,
        last_bot_message_id: Option<i32>,
    },

    // Состояния для пагинации/поиска (Все Пары)
    ViewingAllPairs {
        current_page: usize,
        filter: Option<String>,
        pairs: Vec<String>,
        last_bot_message_id: Option<i32>
    }, // TODO: Использовать при реализации

    // Состояния для других диалогов
    AwaitingFundingSymbolInput { last_bot_message_id: Option<i32> },
    None,
}

pub type StateStorage = Arc<RwLock<HashMap<ChatId, UserState>>>;

#[derive(Debug)]
pub struct RunningOperationInfo {
    pub handle: AbortHandle,
    pub operation_id: i64,
    pub operation_type: OperationType,
    pub symbol: String,
    pub bot_message_id: i32,
    pub current_spot_order_id: Arc<TokioMutex<Option<String>>>,
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

pub async fn dispatch_command<E>( /* ... параметры ... */ ) -> anyhow::Result<()>
where E: Exchange + Clone + Send + Sync + 'static,
{
    // ... (код без изменений) ...
    Ok(())
}

pub async fn dispatch_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: Arc<E>,
    state_storage: StateStorage,
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let Some(data) = q.data.as_ref() {
        info!("Dispatching callback: {}", data);
        let (prefix, _payload) = data.split_once('_').unwrap_or((data.as_str(), ""));

        // Маршрутизация ...
        if data == callback_data::BACK_TO_MAIN {
            navigation::handle_back_to_main(bot, q, state_storage).await?;
        } else if data == callback_data::CANCEL_DIALOG {
            navigation::handle_cancel_dialog(bot, q, state_storage).await?;
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
             bot.answer_callback_query(q.id).text("Функция в разработке.").await?; // ИСПРАВЛЕНО
        } else if data.starts_with(callback_data::PREFIX_HEDGE_CONFIRM) {
             hedge_flow::handle_hedge_confirm_callback(bot, q, exchange, state_storage, running_operations, cfg, db).await?;
        } else if data == callback_data::VIEW_ALL_PAIRS {
             warn!("Handler for VIEW_ALL_PAIRS not implemented yet.");
             bot.answer_callback_query(q.id).text("Функция в разработке.").await?; // ИСПРАВЛЕНО
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
             bot.answer_callback_query(q.id).text("Навигация по страницам пока не работает").await?; // ИСПРАВЛЕНО
        } else {
             warn!("Unhandled callback data: {}", data);
             bot.answer_callback_query(q.id).text("Неизвестное действие.").await?; // ИСПРАВЛЕНО
        }
    } else {
        warn!("CallbackQuery received without data");
        bot.answer_callback_query(q.id).await?; // Просто отвечаем
    }

    Ok(())
}

/// Главный диспетчер текстовых сообщений
pub async fn dispatch_message<E>(
    bot: Bot,
    msg: Message,
    exchange: Arc<E>,
    state_storage: StateStorage,
    running_operations: RunningOperations,
    cfg: Arc<Config>,
    db: Arc<Db>,
) -> anyhow::Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let state = { state_storage.read().expect("Lock failed").get(&msg.chat.id).cloned().unwrap_or(UserState::None) };
    info!("Dispatching message in state: {:?}", state);

    // Маршрутизируем сообщение в зависимости от состояния
    match state {
        // Ввод тикера для хеджа или фильтрация списка всех пар
        UserState::AwaitingHedgeAssetSelection { .. } | UserState::ViewingAllPairs { .. } =>
            hedge_flow::handle_asset_ticker_input(bot, msg, exchange, state_storage, cfg, db).await?,

        // Диалог хеджирования
        UserState::AwaitingHedgeSum { .. } => hedge_flow::handle_sum_input(bot, msg, state_storage, cfg).await?,
        UserState::AwaitingHedgeVolatility { .. } => hedge_flow::handle_volatility_input(bot, msg, exchange, state_storage, running_operations, cfg, db).await?,

        // Ввод символа для фандинга
        UserState::AwaitingFundingSymbolInput { .. } =>
            market_info::handle_funding_symbol_input(bot, msg, exchange, state_storage, cfg, db).await?,

        // TODO: Добавить обработку для состояний расхеджирования, если они потребуют ввода текста
        UserState::AwaitingUnhedgeAssetSelection { .. } => { // Пример заглушки
            warn!("Handler for AwaitingUnhedgeAssetSelection state (text input) not implemented yet.");
            if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await { tracing::warn!("Failed to delete unhandled message: {}", e); }
        },
        UserState::AwaitingUnhedgeOperationSelection { .. } => { // Пример заглушки
            warn!("Handler for AwaitingUnhedgeOperationSelection state (text input) not implemented yet.");
             if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await { tracing::warn!("Failed to delete unhandled message: {}", e); }
        },
        UserState::AwaitingUnhedgeConfirmation { .. } => { // Пример заглушки
             warn!("Handler for AwaitingUnhedgeConfirmation state (text input) not implemented yet.");
             if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await { tracing::warn!("Failed to delete unhandled message: {}", e); }
        },


        // Если состояние не предполагает ввод текста
        _ => {
            if msg.text().map_or(false, |t| t.starts_with('/')) {
                warn!("Received command message '{}' in dispatch_message. Should have been handled by dispatch_command.", msg.text().unwrap_or(""));
                // Команды уже должны быть обработаны dispatch_command
            } else if msg.text().is_some() {
                 // Удаляем обычное текстовое сообщение, если оно не ожидается
                 warn!("Received unexpected text message in state {:?}. Deleting.", state);
                 if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await {
                     tracing::warn!("Failed to delete unexpected message: {}", e);
                 }
            }
            // Игнорируем другие типы сообщений (фото и т.д.)
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