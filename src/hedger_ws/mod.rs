// src/hedger_ws/mod.rs

pub mod state;
pub mod common;
pub mod unhedge_task; // <-- Оставляем
pub mod hedge_task;

mod hedge_logic;
mod unhedge_logic; // <-- Добавляем этот модуль

pub use state::{HedgerWsState, HedgerWsStatus, OperationType, MarketUpdate, Leg, ChunkOrderState};

pub use hedge_task::HedgerWsHedgeTask as HedgeWSTask; // <-- Переименовываем

// Возможно, в будущем здесь будет какой-то общий тип или функция-диспетчер,
// которая будет запускать либо hedge_task, либо unhedge_task.
// Например:
// pub async fn run_websocket_operation(...) -> Result<()> { ... }