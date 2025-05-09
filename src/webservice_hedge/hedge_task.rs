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
    ws_handlers::handle_websocket_message,
};

use crate::webservice_hedge::state::{HedgerWsState, HedgerWsStatus, Leg};

pub struct HedgerWsHedgeTask {
    pub operation_id: i64,
    pub config: Arc<Config>,
    pub database: Arc<storage::Db>,
    pub state: HedgerWsState,
    pub private_ws_receiver: Receiver<Result<WebSocketMessage>>,
    pub public_ws_receiver: Receiver<Result<WebSocketMessage>>,
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
        public_ws_receiver: Receiver<Result<WebSocketMessage>>,
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
                ).await.map_err(|db_err| warn!(operation_id, "Failed to update DB status for failed state init: {}", db_err));
                return Err(anyhow!(error_msg)); // Возвращаем anyhow ошибку
            }
        };

        initial_state.status = HedgerWsStatus::Initializing; // Начальный статус перед ожиданием данных в run

        Ok(Self {
            operation_id,
            config,
            database,
            state: initial_state,
            private_ws_receiver,
            public_ws_receiver,
            exchange_rest,
            progress_callback,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        info!(operation_id = self.operation_id, "Starting HedgerWsHedgeTask run loop (dual stream)...");

        // --- ЭТАП 0: Ожидание начальных рыночных данных ---
        if matches!(self.state.status, HedgerWsStatus::Initializing) {
            info!(operation_id = self.operation_id, "Waiting for initial market data from WebSocket streams...");
            let mut price_data_ready = false;
            // Увеличим таймаут ожидания, например, до 10 секунд (50 * 200ms)
            for attempt in 0..200 { 
                if self.state.spot_market_data.best_bid_price.is_some() && 
                   self.state.spot_market_data.best_ask_price.is_some() &&
                   self.state.futures_market_data.best_bid_price.is_some() && 
                   self.state.futures_market_data.best_ask_price.is_some()
                {
                    price_data_ready = true;
                    info!(operation_id = self.operation_id, "Initial market data received.");
                    self.state.status = HedgerWsStatus::SettingLeverage; 
                    break;
                }
                warn!(operation_id = self.operation_id, attempt = attempt + 1, "Waiting for initial market data (attempt {}/200)...", attempt + 1);
                
                tokio::select! {
                    biased; 
                    maybe_private_result = self.private_ws_receiver.recv() => {
                        if let Some(Ok(message)) = maybe_private_result {
                            if let Err(e) = handle_websocket_message(self, message).await {
                                warn!(operation_id = self.operation_id, "Error handling private message during market data wait: {}", e);
                            }
                        } else if maybe_private_result.is_none() { 
                            let err_msg = "Private WS channel closed during initial market data wait.";
                            error!(operation_id = self.operation_id, "{}", err_msg);
                            self.state.status = HedgerWsStatus::Failed(err_msg.to_string());
                            update_final_db_status(self).await;
                            return Err(anyhow!(err_msg));
                        }
                    },
                    maybe_public_result = self.public_ws_receiver.recv() => {
                        if let Some(Ok(message)) = maybe_public_result {
                           if let Err(e) = handle_websocket_message(self, message).await {
                               warn!(operation_id = self.operation_id, "Error handling public message during market data wait: {}", e);
                           }
                        } else if maybe_public_result.is_none() { 
                            let err_msg = "Public WS channel closed during initial market data wait.";
                            error!(operation_id = self.operation_id, "{}", err_msg);
                            self.state.status = HedgerWsStatus::Failed(err_msg.to_string());
                            update_final_db_status(self).await;
                            return Err(anyhow!(err_msg));
                        }
                    },
                    _ = sleep(Duration::from_millis(200)) => {}, 
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
                .or(self.state.spot_market_data.best_ask_price) 
                .unwrap_or_else(|| {
                    error!(operation_id = self.operation_id, "Critical: No market data for futures price estimate in leverage calc, despite waiting. This should not happen.");
                    // Если данных все еще нет, это серьезная проблема, но попробуем продолжить с 1.0, чтобы не паниковать здесь.
                    // Ошибка, скорее всего, проявится дальше.
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
                else { 
                    warn!(operation_id = self.operation_id, "Attempting to calculate leverage with zero collateral and non-zero futures value. This will result in extremely high leverage.");
                    Decimal::new(i64::MAX, 0) // Очень большое значение, вероятно, вызовет ошибку ниже
                } 
            } else {
                (estimated_total_futures_value / available_collateral).abs() // Плечо всегда положительное
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
                 let error_msg = "Market data became unavailable before starting first chunk (HedgerWsHedgeTask::run).";
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
            // Если мы здесь и статус не один из ожидаемых активных, значит что-то пошло не так на предыдущих этапах.
            warn!(operation_id = self.operation_id, status = ?self.state.status, "Task run: Unexpected status before main loop. This might indicate an issue in state transitions.");
            // Не меняем статус на Failed здесь, если это не Initializing, SettingLeverage или StartingChunk(1),
            // так как это может быть восстановленная задача в уже активном состоянии.
            // Основной цикл должен сам обработать некорректные переходы или ошибки.
        }
        
        loop {
            tokio::select! {
                biased; 
                maybe_private_result = self.private_ws_receiver.recv() => {
                    match maybe_private_result {
                        Some(Ok(message)) => {
                            debug!(operation_id = self.operation_id, "Received from PRIVATE WS: {:?}", message_type_for_log(&message));
                            if let Err(error) = handle_websocket_message(self, message).await { 
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
                            if !matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                                self.state.status = HedgerWsStatus::Failed("PRIVATE WebSocket channel closed unexpectedly".to_string());
                                update_final_db_status(self).await;
                                // Не возвращаем ошибку немедленно, дадим шанс public каналу тоже закрыться или задаче завершиться
                            }
                            if self.public_ws_receiver.is_closed() { 
                                info!(operation_id = self.operation_id, "Both WS channels closed."); 
                                break; // Выход из select! и затем из loop, если оба канала закрыты
                            }
                        }
                    }
                }
                
                maybe_public_result = self.public_ws_receiver.recv() => {
                    match maybe_public_result {
                        Some(Ok(message)) => {
                            debug!(operation_id = self.operation_id, "Received from PUBLIC WS: {:?}", message_type_for_log(&message));
                            if let Err(error) = handle_websocket_message(self, message).await {
                                error!(operation_id = self.operation_id, %error, "Error handling PUBLIC WebSocket message");
                                self.state.status = HedgerWsStatus::Failed(format!("PUBLIC WS Handling Error: {}", error));
                                update_final_db_status(self).await;
                                return Err(error);
                            }
                        }
                        Some(Err(error)) => {
                            error!(operation_id = self.operation_id, %error, "Error received from PUBLIC WebSocket channel. Stopping task.");
                            self.state.status = HedgerWsStatus::Failed(format!("PUBLIC WS channel error: {}", error));
                            update_final_db_status(self).await;
                            return Err(error);
                        }
                        None => { 
                            info!(operation_id = self.operation_id, "PUBLIC WebSocket channel closed.");
                            if !matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                                self.state.status = HedgerWsStatus::Failed("PUBLIC WebSocket channel closed unexpectedly".to_string());
                                update_final_db_status(self).await;
                            }
                            if self.private_ws_receiver.is_closed() { 
                                info!(operation_id = self.operation_id, "Both WS channels closed."); 
                                break; 
                            }
                        }
                    }
                }
                
                _ = sleep(Duration::from_millis(100)) => { 
                    // Пауза для выполнения логики ниже, если нет сообщений
                }
            } 

            // Если оба канала закрылись, и мы вышли из select! по этой причине,
            // то выходим из внешнего цикла loop.
            if self.private_ws_receiver.is_closed() && self.public_ws_receiver.is_closed() {
                 if !matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                     warn!(operation_id = self.operation_id, status = ?self.state.status, "Both WebSocket channels closed, task not in final state. Setting to Failed.");
                     self.state.status = HedgerWsStatus::Failed("Both WebSocket channels closed while task active.".to_string());
                     update_final_db_status(self).await;
                 }
                 info!(operation_id = self.operation_id, "Exiting run loop as both WS channels are closed.");
                 break;
            }


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
                if !check_value_imbalance(self) {
                    info!(operation_id=self.operation_id, "Value imbalance resolved. Proceeding.");
                    let next_chunk_idx_to_start = self.state.current_chunk_index; 
                    self.state.status = HedgerWsStatus::StartingChunk(next_chunk_idx_to_start);
                    should_start_next_chunk = true;
                    next_chunk_to_process = Some(next_chunk_idx_to_start);
                }
            }
            else if let HedgerWsStatus::StartingChunk(idx) = self.state.status {
                should_start_next_chunk = true;
                next_chunk_to_process = Some(idx);
            }
            
            if should_reconcile {
                if let Err(error) = reconcile(self).await {
                    error!(operation_id = self.operation_id, %error, "Reconciliation failed");
                    self.state.status = HedgerWsStatus::Failed(format!("Reconciliation failed: {}", error));
                    update_final_db_status(self).await;
                    return Err(error); 
                }
            } else if should_start_next_chunk {
                if let Some(chunk_idx) = next_chunk_to_process {
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
                                return Err(error); 
                            }
                        }
                    } else {
                        warn!(operation_id=self.operation_id, expected_status=?HedgerWsStatus::StartingChunk(chunk_idx), actual_status=?self.state.status, "Decided to start chunk, but state changed. Retrying on next loop iteration.");
                    }
                } else {
                    warn!(operation_id=self.operation_id, status=?self.state.status, "Inconsistent state: should_start_next_chunk is true, but next_chunk_to_process is None.");
                }
            }

            if matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting run loop due to final status.");
                break; 
            }
        } 

        if self.state.status == HedgerWsStatus::Completed {
            Ok(())
        } else {
            Err(anyhow!("Hedger task run loop exited with non-completed status: {:?}", self.state.status))
        }
    }
} 

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