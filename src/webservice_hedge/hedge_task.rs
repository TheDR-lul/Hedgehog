// src/webservice_hedge/hedge_task.rs

use anyhow::{anyhow, Context, Result};
// ИЗМЕНЕНО: Добавлены импорты для Decimal
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
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
use crate::models::HedgeRequest; // HedgeRequest используется в конструкторе
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
    pub state: HedgerWsState, // Теперь это полностью инициализированное состояние
    pub private_ws_receiver: Receiver<Result<WebSocketMessage>>, 
    pub public_ws_receiver: Receiver<Result<WebSocketMessage>>,  
    pub exchange_rest: Arc<dyn Exchange>,
    pub progress_callback: HedgeProgressCallback,
    // Можно добавить поле для хранения HedgeRequest, если его данные нужны в других методах,
    // но для расчета плеча мы теперь используем self.state.initial_user_sum.
    // pub original_request: HedgeRequest, 
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
            request.clone(), // Клонируем, так как request может быть еще нужен
            config.clone(),
            exchange_rest.clone(),
            database.clone(),
        ).await {
            Ok(s) => s,
            Err(e) => {
                error!(operation_id, "Failed to create initial HedgerWsState: {}", e);
                let error_msg_for_db = format!("Failed state init: {}", e);
                // Попытка обновить статус в БД, если операция уже была создана в spawners
                // (insert_hedge_operation вызывается до HedgerWsHedgeTask::new)
                let _ = storage::update_hedge_final_status(
                    database.as_ref(), 
                    operation_id, 
                    "Failed", 
                    None, 
                    0.0, 
                    Some(&error_msg_for_db)
                ).await.map_err(|db_err| warn!("Failed to update DB status for failed state init: {}", db_err));
                return Err(e.context("Failed to create initial HedgerWsState for HedgerWsHedgeTask"));
            }
        };

        initial_state.status = HedgerWsStatus::SettingLeverage;

        Ok(Self {
            operation_id,
            config,
            database,
            state: initial_state,
            private_ws_receiver,
            public_ws_receiver,
            exchange_rest,
            progress_callback,
            // original_request: request, // Если нужно сохранить весь запрос
        })
    }


    pub async fn run(&mut self) -> Result<()> {
        info!(operation_id = self.operation_id, "Starting HedgerWsHedgeTask run loop (dual stream)...");

        if matches!(self.state.status, HedgerWsStatus::SettingLeverage) {
            info!(operation_id = self.operation_id, "Attempting to set leverage...");
            
            // Используем self.state.initial_user_sum для расчета плеча
            let total_sum_decimal = self.state.initial_user_sum; 

            // Оценка текущей цены фьючерса для расчета плеча
            // Пытаемся получить из данных стакана, если они уже есть, иначе используем оценку из state
            let current_futures_price_estimate = self.state.futures_market_data.best_ask_price // Цена, по которой мы бы продавали фьюч (для оценки стоимости)
                .or(self.state.futures_market_data.best_bid_price) // Или другая сторона, если ask нет
                .or(self.state.spot_market_data.best_ask_price) // Fallback на спот, если фьюч данных нет
                .unwrap_or_else(|| {
                    warn!(operation_id = self.operation_id, "No market data for futures price estimate in leverage calc, using 1.0 as fallback.");
                    Decimal::ONE // Крайний случай
                });

            // initial_target_futures_qty это количество, initial_target_spot_value это стоимость
            let estimated_total_futures_value = self.state.initial_target_futures_qty.abs() * current_futures_price_estimate;
            let available_collateral = total_sum_decimal - self.state.initial_target_spot_value;

            if available_collateral <= Decimal::ZERO {
                let error_message = format!("Estimated available collateral ({}) is non-positive for leverage setting.", available_collateral);
                error!(operation_id = self.operation_id, "{}", error_message);
                self.state.status = HedgerWsStatus::Failed(error_message.clone());
                update_final_db_status(self).await;
                return Err(anyhow!(error_message));
            }

            let required_leverage_decimal = estimated_total_futures_value / available_collateral;
            // ИЗМЕНЕНО: Используем to_f64() из ToPrimitive
            let required_leverage = required_leverage_decimal.to_f64().unwrap_or(0.0);
            debug!(operation_id = self.operation_id, required_leverage, "Calculated required leverage for WS hedge");

            if required_leverage < 1.0 || required_leverage.is_nan() || required_leverage.is_infinite() {
                let error_message = format!("Calculated required leverage ({:.2}x) is invalid or less than 1.0.", required_leverage);
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
        
        if matches!(self.state.status, HedgerWsStatus::StartingChunk(1)) {
            let mut price_data_ready = false;
            for attempt in 0..15 { 
                if self.state.spot_market_data.best_bid_price.is_some() && 
                   self.state.spot_market_data.best_ask_price.is_some() &&
                   self.state.futures_market_data.best_bid_price.is_some() && 
                   self.state.futures_market_data.best_ask_price.is_some()
                {
                    price_data_ready = true;
                    info!(operation_id = self.operation_id, "Market data for initial chunk is ready before start_next_chunk.");
                    break;
                }
                warn!(operation_id = self.operation_id, attempt = attempt + 1, "Waiting for initial market data before starting chunk 1 in HedgerWsHedgeTask::run...");
                
                // Даем шанс обработать входящие WS сообщения для обновления MarketUpdate
                tokio::select! {
                    biased; // Приоритет обработке сообщений
                    maybe_private_result = self.private_ws_receiver.recv() => {
                        if let Some(Ok(message)) = maybe_private_result {
                            let _ = handle_websocket_message(self, message).await; // Ошибки здесь будут обработаны в основном цикле
                        } else if maybe_private_result.is_none() {
                            warn!(operation_id = self.operation_id, "Private WS channel closed during market data wait.");
                            // Можно добавить более строгую обработку, если это критично
                        }
                    },
                    maybe_public_result = self.public_ws_receiver.recv() => {
                        if let Some(Ok(message)) = maybe_public_result {
                           let _ = handle_websocket_message(self, message).await;
                        } else if maybe_public_result.is_none() {
                            warn!(operation_id = self.operation_id, "Public WS channel closed during market data wait.");
                        }
                    },
                    _ = sleep(Duration::from_millis(200)) => {}, // Таймаут для этой итерации ожидания
                }
            }

            if !price_data_ready {
                let error_msg = "Failed to get initial market data in HedgerWsHedgeTask::run after several attempts.";
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
            warn!(operation_id = self.operation_id, status = ?self.state.status, "Task run started with unexpected status after leverage setting/chunk1 attempt.");
            let error_message = format!("Task started in invalid state after setup: {:?}", self.state.status);
            self.state.status = HedgerWsStatus::Failed(error_message.clone());
            update_final_db_status(self).await;
            return Err(anyhow!(error_message));
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
                                return Err(anyhow!("PRIVATE WebSocket channel closed unexpectedly"));
                            }
                            if self.public_ws_receiver.is_closed() { info!("Both WS channels closed."); break; }
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
                                return Err(anyhow!("PUBLIC WebSocket channel closed unexpectedly"));
                            }
                            if self.private_ws_receiver.is_closed() { info!("Both WS channels closed."); break; }
                        }
                    }
                }
                
                _ = sleep(Duration::from_millis(100)) => { 
                    // No messages from WS, proceed with state logic
                }
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

            if self.private_ws_receiver.is_closed() && self.public_ws_receiver.is_closed() {
                if !matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                     warn!(operation_id = self.operation_id, status = ?self.state.status, "Both WebSocket channels closed, but task not in a final state. Setting to Failed.");
                     self.state.status = HedgerWsStatus::Failed("Both WebSocket channels closed unexpectedly.".to_string());
                     update_final_db_status(self).await;
                }
                info!(operation_id = self.operation_id, "Both WebSocket channels confirmed closed. Exiting run loop.");
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