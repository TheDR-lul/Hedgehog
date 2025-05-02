// src/hedger_ws/mod.rs
pub mod common;
pub mod hedge_task;
pub mod state;
pub mod unhedge_task;

pub mod hedge_logic;
pub mod unhedge_logic;

pub use state::{HedgerWsState, HedgerWsStatus, OperationType, MarketUpdate, Leg, ChunkOrderState};
pub use hedge_task::HedgerWsHedgeTask; // <--- ДОБАВЬТЕ ЭТУ СТРОКУ
// Возможно, в будущем здесь будет какой-то общий тип или функция-диспетчер,
// которая будет запускать либо hedge_task, либо unhedge_task.
// Например:
// pub async fn run_websocket_operation(...) -> Result<()> { ... }