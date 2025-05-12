// src/webservice_hedge/hedge_task.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::{debug, error, info, warn};
use tokio::time::sleep;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

use crate::config::Config;
use crate::exchange::Exchange;
use crate::exchange::types::WebSocketMessage;
use crate::hedger::HedgeProgressCallback;
use crate::models::HedgeRequest;
use crate::storage;

use crate::webservice_hedge::hedge_logic::{
    chunk_execution::start_next_chunk,
    helpers::{check_chunk_completion, check_value_imbalance, update_final_db_status},
    init::create_initial_hedger_ws_state,
    reconciliation::reconcile,
    // ws_handlers::handle_websocket_message, // Будет вызываться через handle_ws_message_with_category
};

use crate::webservice_hedge::state::{HedgerWsState, HedgerWsStatus, Leg};

pub struct HedgerWsHedgeTask {
    pub operation_id: i64,
    pub config: Arc<Config>,
    pub database: Arc<storage::Db>,
    pub state: HedgerWsState,
    pub private_ws_receiver: Receiver<Result<WebSocketMessage>>,
    pub public_spot_receiver: Receiver<Result<WebSocketMessage>>,
    pub public_linear_receiver: Receiver<Result<WebSocketMessage>>,
    pub exchange_rest: Arc<dyn Exchange>,
    pub progress_callback: HedgeProgressCallback,
}

impl HedgerWsHedgeTask {
    pub async fn new(
        operation_id: i64,
        request: HedgeRequest,
        config: Arc<Config>,
        database: Arc<storage::Db>,
        exchange_rest: Arc<dyn Exchange>,
        progress_callback: HedgeProgressCallback,
        private_ws_receiver: Receiver<Result<WebSocketMessage>>,
        public_spot_receiver: Receiver<Result<WebSocketMessage>>,
        public_linear_receiver: Receiver<Result<WebSocketMessage>>,
    ) -> Result<Self> {

        let mut initial_state = match create_initial_hedger_ws_state(
            operation_id,
            request.clone(),
            config.clone(),
            exchange_rest.clone(),
            database.clone(),
        ).await {
            Ok(s) => s,
            Err(e) => {
                let error_msg = format!("Failed to create initial HedgerWsState: {}", e);
                error!(operation_id, "{}", error_msg);
                let _ = storage::update_hedge_final_status(
                    database.as_ref(),
                    operation_id,
                    "Failed",
                    None,
                    0.0,
                    Some(&error_msg)
                ).await.map_err(|db_err| {
                    warn!(operation_id, "Failed to update DB status for failed state init: {}", db_err);
                });
                return Err(anyhow!(error_msg));
            }
        };

        initial_state.status = HedgerWsStatus::Initializing;

        Ok(Self {
            operation_id,
            config,
            database,
            state: initial_state,
            private_ws_receiver,
            public_spot_receiver,
            public_linear_receiver,
            exchange_rest,
            progress_callback,
        })
    }

    async fn handle_ws_message_with_category(&mut self, message: WebSocketMessage, category: &str) -> Result<()> {
        crate::webservice_hedge::hedge_logic::ws_handlers::handle_websocket_message(self, message, category).await
    }

    pub async fn run(&mut self) -> Result<()> {
        info!(operation_id = self.operation_id, "Starting HedgerWsHedgeTask run loop (tri-stream)...");

        // --- ЭТАП 0: Ожидание начальных рыночных данных ---
        if matches!(self.state.status, HedgerWsStatus::Initializing) {
            info!(operation_id = self.operation_id, "Waiting for initial market data from WebSocket streams...");
            let mut price_data_ready = false;
            // Используем значение из config, если оно там есть, или разумное значение по умолчанию
            let max_attempts = self.config.ws_auto_chunk_target_count.max(100).min(300); // Как в логах (100) или ваше значение

            for attempt in 0..max_attempts { // Используем max_attempts
                if self.state.spot_market_data.best_bid_price.is_some() &&
                   self.state.spot_market_data.best_ask_price.is_some() &&
                   self.state.futures_market_data.best_bid_price.is_some() &&
                   self.state.futures_market_data.best_ask_price.is_some()
                {
                    price_data_ready = true;
                    info!(operation_id = self.operation_id, "Initial SPOT and FUTURES market data received.");
                    self.state.status = HedgerWsStatus::SettingLeverage;
                    break;
                }
                // Логгируем с корректным max_attempts
                warn!(operation_id = self.operation_id, attempt = attempt + 1, "Waiting for initial market data (attempt {}/{})...", attempt + 1, max_attempts);

                tokio::select! {
                    biased;
                    maybe_private_result = self.private_ws_receiver.recv() => {
                        match maybe_private_result {
                            Some(Ok(message)) => {
                                if let Err(e) = self.handle_ws_message_with_category(message, "private").await {
                                    warn!(operation_id = self.operation_id, "Error handling PRIVATE message during market data wait: {}", e);
                                }
                            }
                            Some(Err(e)) => {
                                warn!(operation_id = self.operation_id, "Error from private_ws_receiver: {}. Channel might be broken.", e);
                            }
                            None => {
                                let err_msg = "Private WS channel closed during initial market data wait.";
                                error!(operation_id = self.operation_id, "{}", err_msg);
                                self.state.status = HedgerWsStatus::Failed(err_msg.to_string());
                                update_final_db_status(self).await;
                                return Err(anyhow!(err_msg));
                            }
                        }
                    },
                    maybe_public_spot_result = self.public_spot_receiver.recv() => {
                        match maybe_public_spot_result {
                            Some(Ok(message)) => {
                               if let Err(e) = self.handle_ws_message_with_category(message, "spot").await {
                                   warn!(operation_id = self.operation_id, "Error handling PUBLIC SPOT message during market data wait: {}", e);
                               }
                            }
                            Some(Err(e)) => {
                                warn!(operation_id = self.operation_id, "Error from public_spot_receiver: {}. Channel might be broken.", e);
                            }
                            None => {
                                let err_msg = "Public SPOT WS channel closed during initial market data wait.";
                                error!(operation_id = self.operation_id, "{}", err_msg);
                                self.state.status = HedgerWsStatus::Failed(err_msg.to_string());
                                update_final_db_status(self).await;
                                return Err(anyhow!(err_msg));
                            }
                        }
                    },
                    maybe_public_linear_result = self.public_linear_receiver.recv() => {
                        match maybe_public_linear_result {
                            Some(Ok(message)) => {
                               if let Err(e) = self.handle_ws_message_with_category(message, "linear").await {
                                   warn!(operation_id = self.operation_id, "Error handling PUBLIC LINEAR message during market data wait: {}", e);
                               }
                            }
                            Some(Err(e)) => {
                                warn!(operation_id = self.operation_id, "Error from public_linear_receiver: {}. Channel might be broken.", e);
                            }
                            None => {
                                let err_msg = "Public LINEAR WS channel closed during initial market data wait.";
                                error!(operation_id = self.operation_id, "{}", err_msg);
                                self.state.status = HedgerWsStatus::Failed(err_msg.to_string());
                                update_final_db_status(self).await;
                                return Err(anyhow!(err_msg));
                            }
                        }
                    },
                    _ = sleep(Duration::from_millis(100)) => {},
                }
            }

            if !price_data_ready {
                let error_msg = "Failed to get initial market data after sufficient attempts.";
                error!(operation_id = self.operation_id, "{}", error_msg);
                self.state.status = HedgerWsStatus::Failed(error_msg.to_string());
                update_final_db_status(self).await;
                return Err(anyhow!(error_msg));
            }
        }

        // --- Этап 1: Установка Плеча ---
        if matches!(self.state.status, HedgerWsStatus::SettingLeverage) {
            info!(operation_id = self.operation_id, "Attempting to set leverage...");
            let total_sum_decimal = self.state.initial_user_sum;
            let current_futures_price_estimate = self.state.futures_market_data.best_ask_price
                .or(self.state.futures_market_data.best_bid_price)
                .or_else(|| {
                    warn!(operation_id = self.operation_id, "Futures market data missing for leverage calc, trying spot price as fallback.");
                    self.state.spot_market_data.best_ask_price.or(self.state.spot_market_data.best_bid_price)
                })
                .unwrap_or_else(|| {
                    error!(operation_id = self.operation_id, "Critical: No market data (futures or spot) for futures price estimate in leverage calc. Using ONE as fallback.");
                    Decimal::ONE
                });

            let estimated_total_futures_value = self.state.initial_target_futures_qty.abs() * current_futures_price_estimate;
            let available_collateral = total_sum_decimal - self.state.initial_target_spot_value;

            if available_collateral <= Decimal::ZERO {
                let error_message = format!("Estimated available collateral ({}) is non-positive for leverage setting. Sum: {}, SpotValue: {}", available_collateral, total_sum_decimal, self.state.initial_target_spot_value);
                error!(operation_id = self.operation_id, "{}", error_message);
                self.state.status = HedgerWsStatus::Failed(error_message.clone());
                update_final_db_status(self).await;
                return Err(anyhow!(error_message));
            }

            let required_leverage_decimal = if available_collateral.is_zero() {
                if estimated_total_futures_value.abs() < Decimal::from_f64(0.000001).unwrap_or_default() { Decimal::ONE }
                else { Decimal::new(i64::MAX, 0) }
            } else {
                (estimated_total_futures_value / available_collateral).abs()
            };

            let required_leverage = required_leverage_decimal.to_f64().unwrap_or(f64::MAX);
            debug!(operation_id = self.operation_id, required_leverage, "Calculated required leverage for WS hedge");

            if required_leverage < 1.0 || required_leverage.is_nan() || required_leverage.is_infinite() {
                let error_message = format!("Calculated required leverage ({:.2}x) is invalid or less than 1.0. Est.FutVal: {}, Collateral: {}", required_leverage, estimated_total_futures_value, available_collateral);
                error!(operation_id = self.operation_id, "{}", error_message);
                self.state.status = HedgerWsStatus::Failed(error_message.clone());
                update_final_db_status(self).await;
                return Err(anyhow!(error_message));
            }
            if required_leverage > self.config.max_allowed_leverage {
                let error_message = format!("Required leverage {:.2}x exceeds max allowed {:.2}x.", required_leverage, self.config.max_allowed_leverage);
                error!(operation_id = self.operation_id, "{}", error_message);
                self.state.status = HedgerWsStatus::Failed(error_message.clone());
                update_final_db_status(self).await;
                return Err(anyhow!(error_message));
            }

            let leverage_to_set = (required_leverage * 100.0).round() / 100.0;
            info!(operation_id = self.operation_id, leverage_to_set, symbol = %self.state.symbol_futures, "Setting leverage via REST...");
            match self.exchange_rest.set_leverage(&self.state.symbol_futures, leverage_to_set).await {
                Ok(_) => {
                    info!(operation_id = self.operation_id, "Leverage set successfully.");
                    sleep(Duration::from_millis(500)).await;
                    self.state.status = HedgerWsStatus::StartingChunk(1);
                }
                Err(e) => {
                    let error_message = format!("Failed to set leverage: {}", e);
                    error!(operation_id = self.operation_id, "{}", error_message);
                    self.state.status = HedgerWsStatus::Failed(error_message.clone());
                    update_final_db_status(self).await;
                    return Err(anyhow!(error_message));
                }
            }
        }

        // --- Этап 2: Запуск первого чанка ---
        if matches!(self.state.status, HedgerWsStatus::StartingChunk(1)) {
            if !(self.state.spot_market_data.best_bid_price.is_some() &&
                 self.state.futures_market_data.best_bid_price.is_some()) {
                 let error_msg = "Market data (spot or futures) became unavailable before starting first chunk.";
                 error!(operation_id = self.operation_id, "{}", error_msg);
                 self.state.status = HedgerWsStatus::Failed(error_msg.to_string());
                 update_final_db_status(self).await;
                 return Err(anyhow!(error_msg));
            }
            match start_next_chunk(self).await {
                Ok(skipped) => {
                    if skipped { info!(operation_id = self.operation_id, chunk=1, "Initial chunk skipped."); }
                    else { info!(operation_id = self.operation_id, chunk=1, "Initial chunk started."); }
                }
                Err(error) => {
                    error!(operation_id = self.operation_id, %error, "Failed to start initial chunk");
                    self.state.status = HedgerWsStatus::Failed(format!("Failed start chunk 1: {}", error));
                    update_final_db_status(self).await;
                    return Err(error);
                }
            }
        } else if !matches!(self.state.status, HedgerWsStatus::RunningChunk(_) | HedgerWsStatus::PlacingSpotOrder(_) | HedgerWsStatus::PlacingFuturesOrder(_) | HedgerWsStatus::WaitingImbalance{..} | HedgerWsStatus::CancellingOrder{..} | HedgerWsStatus::WaitingCancelConfirmation{..}) {
             warn!(operation_id = self.operation_id, status = ?self.state.status, "Task run: Unexpected status before main loop.");
        }

        // --- ОСНОВНОЙ ЦИКЛ ---
        loop {
            tokio::select! {
                biased;
                maybe_private_result = self.private_ws_receiver.recv() => {
                    match maybe_private_result {
                        Some(Ok(message)) => {
                            debug!(operation_id = self.operation_id, "Received from PRIVATE WS: {:?}", message_type_for_log(&message));
                            if let Err(error) = self.handle_ws_message_with_category(message, "private").await {
                                error!(operation_id = self.operation_id, %error, "Error handling PRIVATE WebSocket message");
                                self.state.status = HedgerWsStatus::Failed(format!("PRIVATE WS Handling Error: {}", error));
                                update_final_db_status(self).await;
                                return Err(error);
                            }
                        }
                        Some(Err(error)) => {
                            error!(operation_id = self.operation_id, %error, "Error received from PRIVATE WebSocket channel. Stopping task.");
                            self.state.status = HedgerWsStatus::Failed(format!("PRIVATE WS channel error: {}", error));
                            update_final_db_status(self).await;
                            return Err(error);
                        }
                        None => {
                            info!(operation_id = self.operation_id, "PRIVATE WebSocket channel closed.");
                            if self.public_spot_receiver.is_closed() && self.public_linear_receiver.is_closed() { break; }
                        }
                    }
                },
                maybe_public_spot_result = self.public_spot_receiver.recv() => {
                    match maybe_public_spot_result {
                        Some(Ok(message)) => {
                            debug!(operation_id = self.operation_id, "Received from PUBLIC SPOT WS: {:?}", message_type_for_log(&message));
                            if let Err(error) = self.handle_ws_message_with_category(message, "spot").await {
                                error!(operation_id = self.operation_id, %error, "Error handling PUBLIC SPOT WebSocket message");
                                self.state.status = HedgerWsStatus::Failed(format!("PUBLIC SPOT WS Handling Error: {}", error));
                                update_final_db_status(self).await;
                                return Err(error);
                            }
                        }
                         Some(Err(error)) => {
                            error!(operation_id = self.operation_id, %error, "Error received from PUBLIC SPOT WebSocket channel. Stopping task.");
                            self.state.status = HedgerWsStatus::Failed(format!("PUBLIC SPOT WS channel error: {}", error));
                            update_final_db_status(self).await;
                            return Err(error);
                        }
                        None => {
                            info!(operation_id = self.operation_id, "PUBLIC SPOT WebSocket channel closed.");
                            if self.private_ws_receiver.is_closed() && self.public_linear_receiver.is_closed() { break; }
                        }
                    }
                },
                maybe_public_linear_result = self.public_linear_receiver.recv() => {
                    match maybe_public_linear_result {
                        Some(Ok(message)) => {
                            debug!(operation_id = self.operation_id, "Received from PUBLIC LINEAR WS: {:?}", message_type_for_log(&message));
                            if let Err(error) = self.handle_ws_message_with_category(message, "linear").await {
                                error!(operation_id = self.operation_id, %error, "Error handling PUBLIC LINEAR WebSocket message");
                                self.state.status = HedgerWsStatus::Failed(format!("PUBLIC LINEAR WS Handling Error: {}", error));
                                update_final_db_status(self).await;
                                return Err(error);
                            }
                        }
                        Some(Err(error)) => {
                            error!(operation_id = self.operation_id, %error, "Error received from PUBLIC LINEAR WebSocket channel. Stopping task.");
                            self.state.status = HedgerWsStatus::Failed(format!("PUBLIC LINEAR WS channel error: {}", error));
                            update_final_db_status(self).await;
                            return Err(error);
                        }
                        None => {
                            info!(operation_id = self.operation_id, "PUBLIC LINEAR WebSocket channel closed.");
                             if self.private_ws_receiver.is_closed() && self.public_spot_receiver.is_closed() { break; }
                        }
                    }
                },
                _ = sleep(Duration::from_millis(100)) => {} // Невелика затримка, якщо немає повідомлень
            }

            // Перевірка, чи всі канали закриті, для виходу з циклу
            if self.private_ws_receiver.is_closed() &&
               self.public_spot_receiver.is_closed() &&
               self.public_linear_receiver.is_closed() {
                 if !matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                     warn!(operation_id = self.operation_id, status = ?self.state.status, "All WebSocket channels closed, task not in final state. Setting to Failed.");
                     self.state.status = HedgerWsStatus::Failed("All WebSocket channels closed while task active.".to_string());
                     update_final_db_status(self).await;
                 }
                 info!(operation_id = self.operation_id, "Exiting run loop as all WS channels are closed.");
                 break; // Вихід з основного циклу loop
            }

            // Логіка обробки стану чанків, реконсиляції і т.д.
            let mut should_start_next_chunk = false;
            let mut should_reconcile = false;
            let mut next_chunk_to_process: Option<u32> = None;

            if matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) && check_chunk_completion(self) {
                let current_chunk_completed = self.state.current_chunk_index.saturating_sub(1);
                debug!(operation_id=self.operation_id, chunk_index=current_chunk_completed, "Chunk completed processing.");

                if current_chunk_completed < self.state.total_chunks {
                    if check_value_imbalance(self) {
                        info!(operation_id=self.operation_id, chunk_index=current_chunk_completed, "Significant value imbalance detected. Waiting.");
                        let leading_leg = if self.state.cumulative_spot_filled_value > self.state.cumulative_futures_filled_value.abs() { Leg::Spot } else { Leg::Futures };
                        self.state.status = HedgerWsStatus::WaitingImbalance { chunk_index: current_chunk_completed, leading_leg };
                    } else {
                        let next_chunk_idx_to_start = self.state.current_chunk_index;
                        self.state.status = HedgerWsStatus::StartingChunk(next_chunk_idx_to_start);
                        should_start_next_chunk = true;
                        next_chunk_to_process = Some(next_chunk_idx_to_start);
                    }
                } else {
                    info!(operation_id = self.operation_id, "All chunks processed, moving to Reconciling state.");
                    self.state.status = HedgerWsStatus::Reconciling;
                    should_reconcile = true;
                }
            }
            else if matches!(self.state.status, HedgerWsStatus::WaitingImbalance { .. }) {
                if !check_value_imbalance(self) { // Якщо дисбаланс зник
                    info!(operation_id=self.operation_id, "Value imbalance resolved. Proceeding.");
                    let next_chunk_idx_to_start = self.state.current_chunk_index; // Поточний індекс чанку, на якому зупинились
                    self.state.status = HedgerWsStatus::StartingChunk(next_chunk_idx_to_start);
                    should_start_next_chunk = true;
                    next_chunk_to_process = Some(next_chunk_idx_to_start);
                }
            }
            // Якщо статус StartingChunk, то потрібно запустити чанк
            else if let HedgerWsStatus::StartingChunk(idx) = self.state.status {
                should_start_next_chunk = true;
                next_chunk_to_process = Some(idx);
            }


            if should_reconcile {
                if let Err(error) = reconcile(self).await {
                    error!(operation_id = self.operation_id, %error, "Reconciliation failed");
                    self.state.status = HedgerWsStatus::Failed(format!("Reconciliation failed: {}", error));
                    update_final_db_status(self).await;
                    return Err(error); // Повертаємо помилку, щоб зупинити задачу
                }
                // Якщо reconcile завершився успішно, статус буде Completed, і цикл завершиться на наступній ітерації
            } else if should_start_next_chunk {
                if let Some(chunk_idx) = next_chunk_to_process {
                    // Додаткова перевірка, чи стан все ще StartingChunk перед викликом
                    if self.state.status == HedgerWsStatus::StartingChunk(chunk_idx) {
                        match start_next_chunk(self).await {
                            Ok(skipped) => {
                                if skipped { info!(operation_id=self.operation_id, chunk=chunk_idx, "Chunk was skipped by start_next_chunk."); }
                                else { info!(operation_id=self.operation_id, chunk=chunk_idx, "Chunk started successfully by start_next_chunk."); }
                            }
                            Err(error) => {
                                error!(operation_id = self.operation_id, chunk = chunk_idx, %error, "Failed to start chunk via start_next_chunk");
                                self.state.status = HedgerWsStatus::Failed(format!("Failed to start chunk {}: {}", chunk_idx, error));
                                update_final_db_status(self).await;
                                return Err(error); // Повертаємо помилку
                            }
                        }
                    } else {
                         warn!(operation_id=self.operation_id, expected_status=?HedgerWsStatus::StartingChunk(chunk_idx), actual_status=?self.state.status, "Decided to start chunk, but state changed. Retrying on next loop iteration.");
                    }
                } else {
                    // Цей випадок не повинен відбуватися, якщо логіка вище правильна
                    warn!(operation_id=self.operation_id, status=?self.state.status, "Inconsistent state: should_start_next_chunk is true, but next_chunk_to_process is None.");
                }
            }


            // Перевірка на завершення, скасування або помилку для виходу з циклу
            if matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting run loop due to final status.");
                break; // Вихід з основного циклу loop
            }
        } // кінець основного циклу loop

        // Фінальна перевірка статусу після виходу з циклу
        if self.state.status == HedgerWsStatus::Completed {
            Ok(())
        } else {
            // Якщо вийшли з циклу не через Completed (наприклад, через закриття всіх каналів),
            // але статус не Failed/Cancelled, це може бути непередбачена ситуація.
            // Однак, update_final_db_status вже мав бути викликаний, якщо статус Failed.
            Err(anyhow!("Hedger task run loop exited with non-completed status: {:?}", self.state.status))
        }
    }
}

// Вспоміжна функція для логування типу повідомлення
fn message_type_for_log(message: &WebSocketMessage) -> String {
    match message {
        WebSocketMessage::OrderUpdate(_) => "OrderUpdate".to_string(),
        WebSocketMessage::Authenticated(_) => "Authenticated".to_string(),
        WebSocketMessage::OrderBookL2 { symbol, .. } => format!("OrderBookL2({})", symbol),
        WebSocketMessage::PublicTrade { symbol, .. } => format!("PublicTrade({})", symbol),
        WebSocketMessage::Pong => "Pong".to_string(),
        WebSocketMessage::SubscriptionResponse { .. } => "SubscriptionResponse".to_string(),
        WebSocketMessage::Error(_) => "Error".to_string(),
        WebSocketMessage::Connected => "Connected".to_string(),
        WebSocketMessage::Disconnected => "Disconnected".to_string(),
    }
}