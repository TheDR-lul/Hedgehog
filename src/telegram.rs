// src/telegram.rs

// ... другие импорты ...
use crate::notifier::{
    Command, StateStorage, RunningOperations, // Используем обновленный StateStorage
    dispatch_command, dispatch_callback, dispatch_message
};
use tokio::sync::Mutex as TokioMutex; // Tokio Mutex для RunningOperations - OK
// <<< ИЗМЕНЕНО: Убираем RwLock из std::sync >>>
// use std::sync::RwLock;
// <<< ДОБАВЛЕНО: Импортируем RwLock из tokio >>>
use tokio::sync::RwLock as TokioRwLock;
use teloxide::{ /* ... */ };
// ... другие импорты ...
use std::sync::Arc;
use std::collections::HashMap;

pub async fn run<E>(bot: Bot, exchange: E, cfg: Config, db: Db)
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let exchange = Arc::new(exchange);
    // <<< ИЗМЕНЕНО: Инициализация с TokioRwLock >>>
    let state_storage: StateStorage = Arc::new(TokioRwLock::new(HashMap::new()));
    // ---
    let running_operations: RunningOperations = Arc::new(TokioMutex::new(HashMap::new()));

    // ... остальной код run() без изменений ...
     let cfg_arc = Arc::new(cfg);
     let db_arc = Arc::new(db);

     // 1) Текстовые команды
     let commands_branch = Update::filter_message()
         .filter_command::<Command>()
         .endpoint({
             let bot_clone = bot.clone();
             let exchange = exchange.clone();
             let state_storage = state_storage.clone(); // Клонируем Arc<TokioRwLock<...>>
             let running_operations = running_operations.clone();
             let cfg = cfg_arc.clone();
             let db = db_arc.clone();

             move |msg: Message, cmd: Command| {
                 let bot = bot_clone.clone();
                 let exchange = exchange.clone();
                 let state_storage = state_storage.clone();
                 let running_operations = running_operations.clone();
                 let cfg = cfg.clone();
                 let db = db.clone();
                 async move {
                     if let Err(err) = dispatch_command(bot, msg, cmd, exchange, state_storage, running_operations, cfg, db).await {
                         tracing::error!("command handler error: {:?}", err);
                     }
                     respond(())
                 }
             }
         });

     // 2) Inline‑callbacks
     let callback_branch = Update::filter_callback_query()
         .endpoint({
             let bot_clone = bot.clone();
             let exchange = exchange.clone();
             let state_storage = state_storage.clone(); // Клонируем Arc<TokioRwLock<...>>
             let running_operations = running_operations.clone();
             let cfg = cfg_arc.clone();
             let db = db_arc.clone();

             move |q: CallbackQuery| {
                 let bot = bot_clone.clone();
                 let exchange = exchange.clone();
                 let state_storage = state_storage.clone();
                 let running_operations = running_operations.clone();
                 let cfg = cfg.clone();
                 let db = db.clone();
                 async move {
                     if let Err(err) = dispatch_callback(bot, q, exchange, state_storage, running_operations, cfg, db).await {
                         tracing::error!("callback handler error: {:?}", err);
                     }
                     respond(())
                 }
             }
         });

     // 3) Текстовые сообщения (не команды)
     let message_branch = Update::filter_message()
         .endpoint({
             let bot_clone = bot.clone();
             let exchange = exchange.clone();
             let state_storage = state_storage.clone(); // Клонируем Arc<TokioRwLock<...>>
             let running_operations = running_operations.clone();
             let cfg = cfg_arc.clone();
             let db = db_arc.clone();

             move |msg: Message| {
                 let bot = bot_clone.clone();
                 let exchange = exchange.clone();
                 let state_storage = state_storage.clone();
                 let running_operations = running_operations.clone();
                 let cfg = cfg.clone();
                 let db = db.clone();
                 async move {
                     if let Err(err) = dispatch_message(bot, msg, exchange, state_storage, running_operations, cfg, db).await {
                         tracing::error!("message handler error: {:?}", err);
                     }
                     respond(())
                 }
             }
         });

     // Собираем все ветки в Dispatcher
     Dispatcher::builder(bot, dptree::entry()
         .branch(commands_branch)
         .branch(callback_branch)
         .branch(message_branch))
         .enable_ctrlc_handler()
         .build()
         .dispatch()
         .await;
}