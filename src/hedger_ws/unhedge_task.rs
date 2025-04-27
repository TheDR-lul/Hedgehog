// src/hedger_ws/unhedge_task.rs

use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tokio::time::sleep;
use std::time::Duration;

use crate::config::Config;
use crate::exchange::Exchange;
use crate::exchange::types::WebSocketMessage;
use crate::hedger::HedgeProgressCallback;
use crate::storage::{self, HedgeOperation};
// --- ИСПРАВЛЕНО: Убираем use hedge_logic ---
use super::unhedge_logic;
// use super::hedge_logic; // Больше не нужен здесь
use super::state::{HedgerWsState, HedgerWsStatus, Leg}; // Leg может понадобиться для статуса WaitingImbalance

pub struct HedgerWsUnhedgeTask {
    pub(crate) operation_id: i64,
    pub(crate) config: Arc<Config>,
    pub(crate) database: Arc<storage::Db>,
    pub(crate) state: HedgerWsState,
    pub(crate) ws_receiver: mpsc::Receiver<Result<WebSocketMessage>>,
    pub(crate) exchange_rest: Arc<dyn Exchange>,
    pub(crate) progress_callback: HedgeProgressCallback,
    pub(crate) original_spot_target: Decimal,
    pub(crate) original_futures_target: Decimal,
    pub(crate) actual_spot_sell_target: Decimal,
}

impl HedgerWsUnhedgeTask {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        original_operation: HedgeOperation,
        config: Arc<Config>,
        database: Arc<storage::Db>,
        exchange_rest: Arc<dyn Exchange>,
        progress_callback: HedgeProgressCallback,
        ws_receiver: mpsc::Receiver<Result<WebSocketMessage>>,
    ) -> Result<Self> {
        // Вызов из unhedge_logic
        unhedge_logic::init::initialize_task(
            original_operation, config, database, exchange_rest, progress_callback, ws_receiver
        ).await
    }

    pub async fn run(&mut self) -> Result<()> {
        info!(operation_id = self.operation_id, "Starting HedgerWsUnhedgeTask run loop...");

        // Запуск первого чанка
        if matches!(self.state.status, HedgerWsStatus::StartingChunk(1)) {
             if let Err(error) = unhedge_logic::chunk_execution::start_next_chunk(self).await {
                 error!(operation_id = self.operation_id, %error, "Failed to start initial unhedge chunk");
                 self.state.status = HedgerWsStatus::Failed(format!("Failed start unhedge chunk 1: {}", error));
                 // --- ИСПРАВЛЕНО: Вызов из unhedge_logic::helpers ---
                 unhedge_logic::helpers::update_final_db_status(self).await;
                 return Err(error);
             }
        } else if !matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) {
            warn!(operation_id = self.operation_id, status = ?self.state.status, "Unhedge task run started with unexpected status.");
            let error_message = format!("Unhedge task started in invalid state: {:?}", self.state.status);
            self.state.status = HedgerWsStatus::Failed(error_message.clone());
             // --- ИСПРАВЛЕНО: Вызов из unhedge_logic::helpers ---
            unhedge_logic::helpers::update_final_db_status(self).await;
            return Err(anyhow!(error_message));
        }

        // Цикл обработки сообщений WS
        loop {
            tokio::select! {
                maybe_result = self.ws_receiver.recv() => {
                    match maybe_result {
                        Some(Ok(message)) => {
                            // --- ИСПРАВЛЕНО: Используем обработчик WS из unhedge_logic ---
                            if let Err(error) = unhedge_logic::ws_handlers::handle_websocket_message(self, message).await {
                                error!(operation_id = self.operation_id, %error, "Error handling WebSocket message during unhedge");
                                self.state.status = HedgerWsStatus::Failed(format!("WS Handling Error (unhedge): {}", error));
                                // --- ИСПРАВЛЕНО: Вызов из unhedge_logic::helpers ---
                                unhedge_logic::helpers::update_final_db_status(self).await;
                                return Err(error);
                            }
                        }
                         Some(Err(error)) => {
                             error!(operation_id = self.operation_id, %error, "Error received from WebSocket channel (unhedge). Stopping task.");
                             self.state.status = HedgerWsStatus::Failed(format!("WS channel error (unhedge): {}", error));
                              // --- ИСПРАВЛЕНО: Вызов из unhedge_logic::helpers ---
                             unhedge_logic::helpers::update_final_db_status(self).await;
                             return Err(error);
                         }
                         None => {
                             info!(operation_id = self.operation_id, "WebSocket channel closed (unhedge). Stopping task.");
                              if !matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                                  self.state.status = HedgerWsStatus::Failed("WebSocket channel closed unexpectedly (unhedge)".to_string());
                                   // --- ИСПРАВЛЕНО: Вызов из unhedge_logic::helpers ---
                                  unhedge_logic::helpers::update_final_db_status(self).await;
                                  return Err(anyhow!("WebSocket channel closed unexpectedly (unhedge)"));
                              }
                             break;
                         }
                    }
                }
                _ = sleep(Duration::from_millis(100)) => {}
            } // конец select!

            // --- Логика после обработки сообщения или паузы ---
            let mut should_start_next_chunk = false;
            let mut should_reconcile = false;
            let mut next_chunk_to_process: Option<u32> = None;

            // --- ИСПРАВЛЕНО: Используем хелперы из unhedge_logic ---
            if matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) && unhedge_logic::helpers::check_chunk_completion(self) {
               let current_chunk = self.state.current_chunk_index.saturating_sub(1);
               debug!(operation_id=self.operation_id, chunk_index=current_chunk, "Unhedge chunk completed processing.");
               if current_chunk < self.state.total_chunks {
                    // --- ИСПРАВЛЕНО: Используем хелперы из unhedge_logic ---
                   if unhedge_logic::helpers::check_value_imbalance(self) {
                       info!(operation_id=self.operation_id, chunk_index=current_chunk, "Unhedge value imbalance detected. Waiting...");
                       let leading = if self.state.cumulative_spot_filled_value > self.state.cumulative_futures_filled_value.abs() { Leg::Spot } else { Leg::Futures };
                       self.state.status = HedgerWsStatus::WaitingImbalance { chunk_index: current_chunk, leading_leg: leading };
                   } else {
                       let next_chunk_index = current_chunk + 1;
                       self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                       should_start_next_chunk = true;
                       next_chunk_to_process = Some(next_chunk_index);
                   }
               } else {
                   info!(operation_id = self.operation_id, "All unhedge chunks processed, moving to Reconciling state.");
                   self.state.status = HedgerWsStatus::Reconciling;
                   should_reconcile = true;
               }
            } else if matches!(self.state.status, HedgerWsStatus::WaitingImbalance { .. }) {
                // --- ИСПРАВЛЕНО: Используем хелперы из unhedge_logic ---
                if !unhedge_logic::helpers::check_value_imbalance(self) {
                     info!(operation_id=self.operation_id, "Unhedge value imbalance resolved. Proceeding.");
                      let next_chunk_index = self.state.current_chunk_index;
                      self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                      should_start_next_chunk = true;
                      next_chunk_to_process = Some(next_chunk_index);
                }
           // Исправляем проверку для idx
           } else if matches!(self.state.status, HedgerWsStatus::StartingChunk(_)) {
                if let HedgerWsStatus::StartingChunk(idx) = self.state.status {
                    should_start_next_chunk = true;
                    next_chunk_to_process = Some(idx);
                } else {
                     warn!(operation_id = self.operation_id, status=?self.state.status, "Inconsistent state detected while checking StartingChunk for unhedge");
                }
           }


            // --- Выполняем запуск следующего чанка или реконсиляцию ---
            if should_reconcile {
                // Вызов из unhedge_logic
                if let Err(error) = unhedge_logic::reconciliation::reconcile(self).await {
                    error!(operation_id = self.operation_id, %error, "Unhedge reconciliation failed");
                    self.state.status = HedgerWsStatus::Failed(format!("Unhedge reconciliation failed: {}", error));
                    // --- ИСПРАВЛЕНО: Вызов из unhedge_logic::helpers ---
                    unhedge_logic::helpers::update_final_db_status(self).await;
                    return Err(error);
                }
                info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting unhedge run loop after successful reconciliation.");
                break;
            } else if should_start_next_chunk {
                 if let Some(chunk_idx) = next_chunk_to_process {
                    // Вызов из unhedge_logic
                    match unhedge_logic::chunk_execution::start_next_chunk(self).await {
                         Ok(()) => { info!(operation_id=self.operation_id, chunk=chunk_idx, "Unhedge chunk started or skipped successfully."); }
                         Err(error) => {
                              error!(operation_id = self.operation_id, chunk = chunk_idx, %error, "Failed to start next unhedge chunk");
                              self.state.status = HedgerWsStatus::Failed(format!("Failed start unhedge chunk {}: {}", chunk_idx, error));
                              // --- ИСПРАВЛЕНО: Вызов из unhedge_logic::helpers ---
                              unhedge_logic::helpers::update_final_db_status(self).await;
                              return Err(error);
                         }
                    }
                 } else {
                     warn!(operation_id=self.operation_id, status=?self.state.status, "Inconsistent state: should_start_next_chunk is true, but next_chunk_to_process is None.");
                 }
            }

             if matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                 info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting unhedge run loop due to final status.");
                 break;
             }
        } // конец loop

        if self.state.status == HedgerWsStatus::Completed { Ok(()) }
        else { Err(anyhow!("Unhedge task run loop exited with non-completed status: {:?}", self.state.status)) }
    }
}