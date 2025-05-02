// src/hedger_ws/hedge_task.rs

use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tokio::time::sleep;
use std::time::Duration;

// Основные зависимости остаются
use crate::config::Config;
use crate::exchange::Exchange;
use crate::exchange::types::WebSocketMessage;
use crate::hedger::HedgeProgressCallback;
use crate::models::HedgeRequest;
use crate::storage;

// --- ДОБАВЛЕНО: Импортируем функции напрямую ---
use super::hedge_logic::{
    chunk_execution::start_next_chunk,
    helpers::{check_chunk_completion, check_value_imbalance, update_final_db_status},
    init::initialize_task,
    reconciliation::reconcile,
    ws_handlers::handle_websocket_message,
};
// --- КОНЕЦ ДОБАВЛЕНИЙ ---

// Импортируем нужные типы состояний и вспомогательные типы
use super::state::{HedgerWsState, HedgerWsStatus, Leg};

// Определение структуры остается, но поля делаем pub(crate)
// чтобы они были доступны функциям в подмодуле hedge_logic
pub struct HedgerWsHedgeTask {
    pub(crate) operation_id: i64,
    pub(crate) config: Arc<Config>,
    pub(crate) database: Arc<storage::Db>,
    pub(crate) state: HedgerWsState,
    pub(crate) ws_receiver: mpsc::Receiver<Result<WebSocketMessage>>,
    pub(crate) exchange_rest: Arc<dyn Exchange>,
    pub(crate) progress_callback: HedgeProgressCallback,
}

impl HedgerWsHedgeTask {
    // Конструктор теперь вызывает функцию инициализации из подмодуля
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        operation_id: i64,
        request: HedgeRequest,
        config: Arc<Config>,
        database: Arc<storage::Db>,
        exchange_rest: Arc<dyn Exchange>,
        progress_callback: HedgeProgressCallback,
        ws_receiver: mpsc::Receiver<Result<WebSocketMessage>>,
    ) -> Result<Self> {
        // --- ИЗМЕНЕНО: Прямой вызов ---
        initialize_task(
            operation_id, request, config, database, exchange_rest, progress_callback, ws_receiver
        ).await
    }

    // Основной цикл остается здесь, но вызывает функции из подмодулей
    pub async fn run(&mut self) -> Result<()> {
        info!(operation_id = self.operation_id, "Starting HedgerWsHedgeTask run loop...");

        // --- Запуск первого чанка ---
        if matches!(self.state.status, HedgerWsStatus::StartingChunk(1)) {
            // --- ИЗМЕНЕНО: Прямой вызов ---
            match start_next_chunk(self).await {
                Ok(skipped) => {
                    if skipped {
                        info!(operation_id = self.operation_id, chunk=1, "Initial chunk skipped.");
                    } else {
                        info!(operation_id = self.operation_id, chunk=1, "Initial chunk started.");
                    }
                }
                Err(error) => {
                    error!(operation_id = self.operation_id, %error, "Failed to start initial chunk");
                    self.state.status = HedgerWsStatus::Failed(format!("Failed start chunk 1: {}", error));
                    // --- ИЗМЕНЕНО: Прямой вызов ---
                    update_final_db_status(self).await;
                    return Err(error);
                }
            }
        } else if !matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) {
            warn!(operation_id = self.operation_id, status = ?self.state.status, "Task run started with unexpected status.");
            let error_message = format!("Task started in invalid state: {:?}", self.state.status);
            self.state.status = HedgerWsStatus::Failed(error_message.clone());
            // --- ИЗМЕНЕНО: Прямой вызов ---
            update_final_db_status(self).await;
            return Err(anyhow!(error_message));
        }

        // --- Цикл обработки сообщений WS ---
        loop {
            tokio::select! {
                // Чтение сообщения из канала WebSocket
                maybe_result = self.ws_receiver.recv() => {
                    match maybe_result {
                        Some(Ok(message)) => {
                            // --- ИЗМЕНЕНО: Прямой вызов ---
                            if let Err(error) = handle_websocket_message(self, message).await {
                                error!(operation_id = self.operation_id, %error, "Error handling WebSocket message");
                                self.state.status = HedgerWsStatus::Failed(format!("WS Handling Error: {}", error));
                                // --- ИЗМЕНЕНО: Прямой вызов ---
                                update_final_db_status(self).await;
                                return Err(error);
                            }
                        }
                        Some(Err(error)) => {
                            error!(operation_id = self.operation_id, %error, "Error received from WebSocket channel. Stopping task.");
                            self.state.status = HedgerWsStatus::Failed(format!("WS channel error: {}", error));
                            // --- ИЗМЕНЕНО: Прямой вызов ---
                            update_final_db_status(self).await;
                            return Err(error);
                        }
                        None => {
                            info!(operation_id = self.operation_id, "WebSocket channel closed. Stopping task.");
                            if !matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                                self.state.status = HedgerWsStatus::Failed("WebSocket channel closed unexpectedly".to_string());
                                // --- ИЗМЕНЕНО: Прямой вызов ---
                                update_final_db_status(self).await;
                                return Err(anyhow!("WebSocket channel closed unexpectedly"));
                            }
                            break;
                        }
                    }
                }
                // Небольшая пауза
                _ = sleep(Duration::from_millis(100)) => {}
            } // конец select!

            // --- Логика после обработки сообщения или паузы ---
            let mut should_start_next_chunk = false;
            let mut should_reconcile = false;
            let mut next_chunk_to_process: Option<u32> = None;

            // --- ИЗМЕНЕНО: Прямые вызовы ---
            if matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) && check_chunk_completion(self) {
                let current_chunk = self.state.current_chunk_index.saturating_sub(1);
                debug!(operation_id=self.operation_id, chunk_index=current_chunk, "Chunk completed processing.");

                if current_chunk < self.state.total_chunks {
                    // --- ИЗМЕНЕНО: Прямой вызов ---
                    if check_value_imbalance(self) {
                        info!(operation_id=self.operation_id, chunk_index=current_chunk, "Significant value imbalance detected. Waiting for lagging leg.");
                        let leading = if self.state.cumulative_spot_filled_value > self.state.cumulative_futures_filled_value.abs() { Leg::Spot } else { Leg::Futures };
                        self.state.status = HedgerWsStatus::WaitingImbalance { chunk_index: current_chunk, leading_leg: leading };
                    } else {
                        let next_chunk_index = current_chunk + 1;
                        self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                        should_start_next_chunk = true;
                        next_chunk_to_process = Some(next_chunk_index);
                    }
                } else {
                    info!(operation_id = self.operation_id, "All chunks processed, moving to Reconciling state.");
                    self.state.status = HedgerWsStatus::Reconciling;
                    should_reconcile = true;
                }
            } else if matches!(self.state.status, HedgerWsStatus::WaitingImbalance { .. }) {
                // --- ИЗМЕНЕНО: Прямой вызов ---
                if !check_value_imbalance(self) {
                    info!(operation_id=self.operation_id, "Value imbalance resolved. Proceeding to next chunk.");
                    let next_chunk_index = self.state.current_chunk_index;
                    self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                    should_start_next_chunk = true;
                    next_chunk_to_process = Some(next_chunk_index);
                }
            } else if matches!(self.state.status, HedgerWsStatus::StartingChunk(_)) {
                if let HedgerWsStatus::StartingChunk(idx) = self.state.status {
                    should_start_next_chunk = true;
                    next_chunk_to_process = Some(idx);
                } else {
                    warn!(operation_id = self.operation_id, status=?self.state.status, "Inconsistent state detected while checking StartingChunk");
                }
            }

            // --- Выполняем запуск следующего чанка или реконсиляцию ---
            if should_reconcile {
                // --- ИЗМЕНЕНО: Прямой вызов ---
                if let Err(error) = reconcile(self).await {
                    error!(operation_id = self.operation_id, %error, "Reconciliation failed");
                    self.state.status = HedgerWsStatus::Failed(format!("Reconciliation failed: {}", error));
                    // --- ИЗМЕНЕНО: Прямой вызов ---
                    update_final_db_status(self).await;
                    return Err(error);
                }
                info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting run loop after reconciliation attempt.");
                break;
            } else if should_start_next_chunk {
                if let Some(chunk_idx) = next_chunk_to_process {
                    // --- ИЗМЕНЕНО: Прямой вызов ---
                    match start_next_chunk(self).await {
                        Ok(skipped) => {
                            if skipped { info!(operation_id=self.operation_id, chunk=chunk_idx, "Chunk skipped, run loop will continue."); }
                            else { info!(operation_id=self.operation_id, chunk=chunk_idx, "Chunk started successfully."); }
                        }
                        Err(error) => {
                            error!(operation_id = self.operation_id, chunk = chunk_idx, %error, "Failed to start next chunk");
                            self.state.status = HedgerWsStatus::Failed(format!("Failed start chunk {}: {}", chunk_idx, error));
                            // --- ИЗМЕНЕНО: Прямой вызов ---
                            update_final_db_status(self).await;
                            return Err(error);
                        }
                    }
                } else {
                    warn!(operation_id=self.operation_id, status=?self.state.status, "Inconsistent state: should_start_next_chunk is true, but next_chunk_to_process is None.");
                }
            }

            // --- Проверка финального статуса для выхода ---
            if matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting run loop due to final status.");
                break;
            }
        } // конец loop

        // Возвращаем результат в зависимости от финального статуса
        if self.state.status == HedgerWsStatus::Completed { Ok(()) }
        else { Err(anyhow!("Hedger task run loop exited with non-completed status: {:?}", self.state.status)) }
    }

} // --- Конец impl HedgerWsHedgeTask ---