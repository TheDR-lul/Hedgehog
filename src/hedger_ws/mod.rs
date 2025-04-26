// src/hedger_ws/mod.rs

// Объявляем подмодули согласно новой структуре
pub mod state;
pub mod common; // Для общих функций
pub mod hedge_task; // Логика хеджирования
pub mod unhedge_task; // Логика расхеджирования

// Реэкспортируем основные типы состояния для удобства использования извне
pub use state::{HedgerWsState, HedgerWsStatus, OperationType, MarketUpdate, Leg, ChunkOrderState};

// Возможно, в будущем здесь будет какой-то общий тип или функция-диспетчер,
// которая будет запускать либо hedge_task, либо unhedge_task.
// Например:
// pub async fn run_websocket_operation(...) -> Result<()> { ... }