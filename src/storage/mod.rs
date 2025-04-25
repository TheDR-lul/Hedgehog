// src/storage/mod.rs

pub mod db;
pub mod schema; // Добавляем модуль schema

// Экспортируем нужные функции и типы
pub use db::{connect, Db};
// Экспортируем функции для работы с операциями
pub use db::{
    insert_hedge_operation,
    update_hedge_spot_order,
    update_hedge_final_status,
    get_completed_unhedged_ops_for_symbol,
    mark_hedge_as_unhedged,
    // <<<--- ДОБАВЛЕНЫ НЕДОСТАЮЩИЕ ЭКСПОРТЫ ---
    get_all_completed_unhedged_ops,
    get_hedge_operation_by_id,
    // --->>>
    // Можно также экспортировать get_running_hedge_operations, если она нужна где-то еще
    // get_running_hedge_operations,
};
// Экспортируем структуру операции
pub use schema::HedgeOperation;