// src/hedger_ws/mod.rs

pub mod state;
pub mod common;
pub mod hedge_task; // Оставляем для основной структуры и run
pub mod unhedge_task; // Аналогично для unhedge
mod hedge_logic; // <-- Добавляем объявление нового модуля
// Возможно, понадобится модуль и для unhedge_logic

// Реэкспортируем основные типы состояния для удобства использования извне
pub use state::{HedgerWsState, HedgerWsStatus, OperationType, MarketUpdate, Leg, ChunkOrderState};

// Возможно, в будущем здесь будет какой-то общий тип или функция-диспетчер,
// которая будет запускать либо hedge_task, либо unhedge_task.
// Например:
// pub async fn run_websocket_operation(...) -> Result<()> { ... }