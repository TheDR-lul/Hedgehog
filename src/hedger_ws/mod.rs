// src/hedger_ws/mod.rs

pub mod state;
pub mod common;
pub mod unhedge_task; // <-- Оставляем
pub mod hedge_task;
pub mod hedge_logic;
pub mod unhedge_logic; // <-- Добавляем этот модуль

pub use state::{HedgerWsState, HedgerWsStatus, OperationType, MarketUpdate, Leg, ChunkOrderState};

// Возможно, в будущем здесь будет какой-то общий тип или функция-диспетчер,
// которая будет запускать либо hedge_task, либо unhedge_task.
// Например:
// pub async fn run_websocket_operation(...) -> Result<()> { ... }