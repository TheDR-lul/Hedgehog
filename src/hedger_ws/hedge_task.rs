// src/hedger_ws/hedge_task.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*; // Import Decimal and related traits
use rust_decimal_macros::dec; // For decimal literals
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use std::time::Duration; // For sleep after setting leverage
use tokio::time::sleep;
use std::collections::HashMap; // Added for state HashMaps

use crate::config::{Config, HedgeStrategy, WsLimitOrderPlacementStrategy}; // Import relevant config types
use crate::exchange::{Exchange, bybit::SPOT_CATEGORY, bybit::LINEAR_CATEGORY}; // Exchange trait and constants
use crate::exchange::types::{WebSocketMessage, SpotInstrumentInfo, LinearInstrumentInfo, FeeRate, DetailedOrderStatus, OrderSide, OrderStatusText, OrderbookLevel}; // WS messages and instrument info
use crate::hedger::HedgeProgressCallback;
use crate::models::HedgeRequest;
use super::state::{HedgerWsState, HedgerWsStatus, OperationType, MarketUpdate, ChunkOrderState, Leg}; // Import state structs
use crate::storage; // For DB updates

// Структура для управления задачей хеджирования
pub struct HedgerWsHedgeTask {
    operation_id: i64,
    config: Arc<Config>,
    database: Arc<storage::Db>, // --- ДОБАВЛЕНО: Доступ к БД ---
    state: HedgerWsState, // Машина состояний
    ws_receiver: mpsc::Receiver<Result<WebSocketMessage>>, // Канал для получения сообщений WS
    exchange_rest: Arc<dyn Exchange>, // Трейт для выполнения REST запросов (ордера, плечо)
    progress_callback: HedgeProgressCallback, // Колбэк для отправки прогресса
}

impl HedgerWsHedgeTask {
    #[allow(clippy::too_many_arguments)] // new function tends to have many arguments
    pub async fn new(
        operation_id: i64,
        request: HedgeRequest,
        config: Arc<Config>,
        database: Arc<storage::Db>, // --- ДОБАВЛЕНО ---
        exchange_rest: Arc<dyn Exchange>,
        progress_callback: HedgeProgressCallback,
        ws_receiver: mpsc::Receiver<Result<WebSocketMessage>>,
    ) -> Result<Self> {
        info!(operation_id, "Initializing HedgerWsHedgeTask...");

        // --- 1. Получение информации об инструментах и начальные расчеты ---
        let base_symbol = request.symbol.to_uppercase();
        let spot_symbol_name = format!("{}{}", base_symbol, config.quote_currency);
        let futures_symbol_name = format!("{}{}", base_symbol, config.quote_currency);

        debug!(operation_id, %spot_symbol_name, %futures_symbol_name, "Fetching instrument info...");
        let ((spot_info_res, linear_info_res), fee_rate_res, mmr_res, spot_price_res) = tokio::join!(
             tokio::join( // Запрашиваем инфо по инструментам параллельно
                 exchange_rest.get_spot_instrument_info(&base_symbol),
                 exchange_rest.get_linear_instrument_info(&base_symbol)
            ),
            exchange_rest.get_fee_rate(&spot_symbol_name, SPOT_CATEGORY),
            exchange_rest.get_mmr(&futures_symbol_name),
            exchange_rest.get_spot_price(&base_symbol)
        );

        let spot_info = spot_info_res.context("Failed to get SPOT instrument info")?;
        let linear_info = linear_info_res.context("Failed to get LINEAR instrument info")?;
        let fee_rate = fee_rate_res.context("Failed to get SPOT fee rate")?;
        let maintenance_margin_rate = mmr_res.context("Failed to get Futures MMR")?;
        let current_spot_price_f64 = spot_price_res.context("Failed to get current SPOT price")?;

        if current_spot_price_f64 <= 0.0 { return Err(anyhow!("Initial spot price is non-positive")); }
        let current_spot_price = Decimal::try_from(current_spot_price_f64)?;

        let total_sum_decimal = Decimal::try_from(request.sum)?;
        let volatility_decimal = Decimal::try_from(request.volatility)?;

        let denominator = (Decimal::ONE + volatility_decimal) * (Decimal::ONE + Decimal::try_from(maintenance_margin_rate)?);
        if denominator == Decimal::ZERO { return Err(anyhow!("Denominator for initial spot value calculation is zero")); }
        let initial_target_spot_value = total_sum_decimal / denominator;
        debug!(operation_id, %initial_target_spot_value, "Calculated initial target spot value");

        let ideal_gross_spot_quantity = initial_target_spot_value / current_spot_price;
        let fut_decimals = Self::get_decimals_from_step(linear_info.lot_size_filter.qty_step.as_deref())?;
        let initial_target_futures_quantity = ideal_gross_spot_quantity.trunc_with_scale(fut_decimals);
        debug!(operation_id, %initial_target_futures_quantity, "Calculated initial target futures quantity (estimate)");

        // --- 2. Автоматический Расчет Чанков ---
         debug!(operation_id, status=?HedgerWsStatus::CalculatingChunks);
        let target_chunk_count = config.ws_auto_chunk_target_count;

        let min_spot_quantity = Decimal::from_str(&spot_info.lot_size_filter.min_order_qty)?;
        let min_futures_quantity = Decimal::from_str(&linear_info.lot_size_filter.min_order_qty)?;
        let spot_quantity_step = Self::get_step_decimal(spot_info.lot_size_filter.base_precision.as_deref())?;
        let futures_quantity_step = Self::get_step_decimal(linear_info.lot_size_filter.qty_step.as_deref())?;
        let min_spot_notional = spot_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());
        let min_futures_notional = linear_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());

        // Предполагаем, что начальная цена фьюча близка к споту для расчета notional
        let current_futures_price_estimate = current_spot_price;

        let (final_chunk_count, chunk_spot_quantity, chunk_futures_quantity) =
            crate::hedger_ws::common::calculate_auto_chunk_parameters( // Используем common
                initial_target_spot_value,
                initial_target_futures_quantity,
                current_spot_price,
                current_futures_price_estimate, // Передаем оценку
                target_chunk_count,
                min_spot_quantity,
                min_futures_quantity,
                spot_quantity_step,
                futures_quantity_step,
                min_spot_notional,
                min_futures_notional,
            )?;
        info!(operation_id, final_chunk_count, %chunk_spot_quantity, %chunk_futures_quantity, "Chunk parameters calculated");

        // --- 3. Расчет и Установка Плеча ---
        debug!(operation_id, status=?HedgerWsStatus::SettingLeverage);
        let estimated_total_futures_value = initial_target_futures_quantity * current_spot_price;
        let estimated_total_spot_value = initial_target_spot_value;
        let available_collateral = total_sum_decimal - estimated_total_spot_value;

        if available_collateral <= Decimal::ZERO { return Err(anyhow!("Estimated available collateral is non-positive ({})", available_collateral)); }
        let required_leverage_decimal = estimated_total_futures_value / available_collateral;
        let required_leverage = required_leverage_decimal.to_f64().unwrap_or(0.0);
        debug!(operation_id, required_leverage, "Calculated required leverage");

        if required_leverage < 1.0 { return Err(anyhow!("Calculated required leverage ({:.2}) is less than 1.0", required_leverage)); }
        if required_leverage > config.max_allowed_leverage { return Err(anyhow!("Required leverage {:.2}x exceeds max allowed {:.2}x", required_leverage, config.max_allowed_leverage)); }

        let leverage_to_set = (required_leverage * 100.0).round() / 100.0;
        info!(operation_id, leverage_to_set, %futures_symbol_name, "Setting leverage via REST...");
        exchange_rest.set_leverage(&futures_symbol_name, leverage_to_set).await.context("Failed to set leverage via REST API")?;
        info!(operation_id, "Leverage set successfully.");
        sleep(Duration::from_millis(500)).await;

        // --- 4. Создание начального состояния ---
        let mut state = HedgerWsState::new_hedge(
            operation_id,
            spot_symbol_name.clone(),
            futures_symbol_name.clone(),
            initial_target_spot_value,
            initial_target_futures_quantity
        );
        // Сохраняем лимиты и параметры чанков
        state.spot_tick_size = Decimal::from_str(&spot_info.price_filter.tick_size)?;
        state.spot_quantity_step = spot_quantity_step;
        state.min_spot_quantity = min_spot_quantity;
        state.min_spot_notional = min_spot_notional;
        state.futures_tick_size = Decimal::from_str(&linear_info.price_filter.tick_size)?;
        state.futures_quantity_step = futures_quantity_step;
        state.min_futures_quantity = min_futures_quantity;
        state.min_futures_notional = min_futures_notional;
        state.total_chunks = final_chunk_count;
        state.chunk_base_quantity_spot = chunk_spot_quantity;
        state.chunk_base_quantity_futures = chunk_futures_quantity;
        state.status = HedgerWsStatus::StartingChunk(1); // Готовы начать первый чанк

        info!(operation_id, "HedgerWsHedgeTask initialized successfully. Ready to run.");

        Ok(Self {
            operation_id,
            config,
            database, // --- ДОБАВЛЕНО ---
            state,
            ws_receiver,
            exchange_rest,
            progress_callback,
        })
    }

    // Основной цикл обработки сообщений и управления состоянием
    pub async fn run(&mut self) -> Result<()> {
        info!(operation_id = self.operation_id, "Starting HedgerWsHedgeTask run loop...");

        // --- Запуск первого чанка ---
        if matches!(self.state.status, HedgerWsStatus::StartingChunk(1)) {
             if let Err(error) = self.start_next_chunk().await {
                 error!(operation_id = self.operation_id, %error, "Failed to start initial chunk");
                 self.state.status = HedgerWsStatus::Failed(format!("Failed start chunk 1: {}", error));
                 self.update_final_db_status().await; // Попытка обновить БД
                 return Err(error);
             }
        } else if !matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) {
            // Если стартуем не с начала и не в RunningChunk, это ошибка
            warn!(operation_id = self.operation_id, status = ?self.state.status, "Task run started with unexpected status.");
            let error_message = format!("Task started in invalid state: {:?}", self.state.status);
            self.state.status = HedgerWsStatus::Failed(error_message.clone());
            self.update_final_db_status().await;
            return Err(anyhow!(error_message));
        }

        // --- Цикл обработки сообщений WS ---
        loop {
             tokio::select! {
                 // Ожидаем сообщение из канала
                 maybe_result = self.ws_receiver.recv() => {
                      match maybe_result {
                         Some(Ok(message)) => {
                             // Обрабатываем сообщение
                             if let Err(error) = self.handle_websocket_message(message).await {
                                 error!(operation_id = self.operation_id, %error, "Error handling WebSocket message");
                                 // Решаем, фатальна ли ошибка
                                 // TODO: Более умная обработка ошибок - не все должны быть фатальны
                                  self.state.status = HedgerWsStatus::Failed(format!("WS Handling Error: {}", error));
                                  self.update_final_db_status().await;
                                  return Err(error);
                             }
                         }
                         Some(Err(error)) => {
                             error!(operation_id = self.operation_id, %error, "Error received from WebSocket channel. Stopping task.");
                             self.state.status = HedgerWsStatus::Failed(format!("WS channel error: {}", error));
                             self.update_final_db_status().await;
                             return Err(error);
                         }
                         None => {
                             info!(operation_id = self.operation_id, "WebSocket channel closed. Stopping task.");
                             if !matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                                  self.state.status = HedgerWsStatus::Failed("WebSocket channel closed unexpectedly".to_string());
                                  self.update_final_db_status().await;
                                  return Err(anyhow!("WebSocket channel closed unexpectedly"));
                             }
                             break; // Выход из цикла, если канал закрылся и статус финальный
                         }
                     }
                 }
                 // Можно добавить ветку для обработки сигналов отмены от пользователя
                 // _ = self.cancel_signal.recv() => { ... self.state.status = HedgerWsStatus::Cancelling; ... }

                 // Пауза, чтобы не перегружать CPU, если сообщений нет
                 _ = sleep(Duration::from_millis(100)) => {}

             } // конец select!


             // --- Логика после обработки сообщения или таймаута ---

             // Проверяем, не нужно ли переставить ордер из-за устаревания цены
             // (Проверка выполняется внутри handle_market_data_update -> check_stale_orders)

             // Проверка завершения чанка (только если мы не в процессе отмены/ожидания)
             if matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) && self.check_chunk_completion() {
                  let current_chunk = match self.state.status {
                       HedgerWsStatus::RunningChunk(index) => index,
                       _ => self.state.current_chunk_index -1, // Последний завершенный
                  };
                 debug!(operation_id=self.operation_id, chunk_index=current_chunk, "Chunk completed processing.");


                 if current_chunk < self.state.total_chunks {
                     let next_chunk_index = current_chunk + 1;
                     // Переходим к следующему чанку только если нет дисбаланса
                      if self.check_value_imbalance() {
                           info!(operation_id=self.operation_id, chunk_index=current_chunk, "Significant value imbalance detected. Waiting for lagging leg.");
                           let leading = if self.state.cumulative_spot_filled_value > self.state.cumulative_futures_filled_value.abs() { Leg::Spot } else { Leg::Futures };
                           self.state.status = HedgerWsStatus::WaitingImbalance { chunk_index: current_chunk, leading_leg: leading };
                      } else {
                           // Дисбаланса нет, запускаем следующий чанк
                           self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                           if let Err(error) = self.start_next_chunk().await {
                               error!(operation_id = self.operation_id, chunk = next_chunk_index, %error, "Failed to start next chunk");
                               self.state.status = HedgerWsStatus::Failed(format!("Failed start chunk {}: {}", next_chunk_index, error));
                               self.update_final_db_status().await;
                               return Err(error);
                           }
                      }
                 } else {
                     // Все чанки обработаны, переход к Reconciling
                     info!(operation_id = self.operation_id, "All chunks processed, moving to Reconciling state.");
                     self.state.status = HedgerWsStatus::Reconciling;
                     if let Err(error) = self.reconcile().await {
                         error!(operation_id = self.operation_id, %error, "Reconciliation failed");
                         self.state.status = HedgerWsStatus::Failed(format!("Reconciliation failed: {}", error));
                         self.update_final_db_status().await;
                         return Err(error);
                     }
                     // Успешное reconcile -> статус Completed установится внутри
                     info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting run loop after successful reconciliation.");
                     break; // Выход из цикла
                 }
             } else if matches!(self.state.status, HedgerWsStatus::WaitingImbalance { .. }) {
                  // Если ждем баланса, проверяем, не уменьшился ли он
                  if !self.check_value_imbalance() {
                       info!(operation_id=self.operation_id, "Value imbalance resolved. Proceeding to next chunk.");
                       // Запускаем следующий чанк
                        let next_chunk_index = self.state.current_chunk_index; // current_chunk_index еще не был увеличен
                        self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                         if let Err(error) = self.start_next_chunk().await {
                             error!(operation_id = self.operation_id, chunk = next_chunk_index, %error, "Failed to start chunk after imbalance resolved");
                             self.state.status = HedgerWsStatus::Failed(format!("Failed start chunk {}: {}", next_chunk_index, error));
                             self.update_final_db_status().await;
                             return Err(error);
                         }
                  }
             }


             // Проверка статуса для внешнего выхода
             if matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                 info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting run loop due to final status.");
                 break;
             }
        } // конец loop

        // Финальная проверка статуса
        if self.state.status == HedgerWsStatus::Completed {
             Ok(())
        } else {
             // Если вышли из цикла по другой причине (например, канал закрылся до Completed)
             let final_error_message = format!("Hedger task run loop exited with non-completed status: {:?}", self.state.status);
             // Убедимся, что статус Failed перед возвратом ошибки
              if !matches!(self.state.status, HedgerWsStatus::Failed(_)) {
                   self.state.status = HedgerWsStatus::Failed(final_error_message.clone());
                   self.update_final_db_status().await;
              }
             Err(anyhow!(final_error_message))
        }
    }

    // Обработка входящего сообщения WebSocket
     async fn handle_websocket_message(&mut self, message: WebSocketMessage) -> Result<()> {
         match message {
             WebSocketMessage::OrderUpdate(details) => {
                 // Проверяем, не ждем ли мы подтверждения отмены именно этого ордера
                 if let HedgerWsStatus::WaitingCancelConfirmation { cancelled_order_id, cancelled_leg, .. } = &self.state.status {
                      if details.order_id == *cancelled_order_id {
                          // Получили статус ордера, который пытались отменить
                           // Сначала обновим финальные цифры по нему
                          self.handle_order_update(details).await?;
                          // Затем запускаем перевыставление
                          self.handle_cancel_confirmation(cancelled_order_id, *cancelled_leg).await?;
                          return Ok(()); // Статус изменился внутри handle_cancel_confirmation
                      }
                 }
                 // Если не ждем отмены или это другой ордер - просто обновляем состояние
                 self.handle_order_update(details).await?;
             }
             WebSocketMessage::OrderBookL2 { symbol, bids, asks, is_snapshot } => {
                 self.handle_order_book_update(symbol.clone(), bids.clone(), asks.clone(), is_snapshot).await?;
                 self.check_stale_orders().await?; // Проверяем устаревание после обновления стакана
             }
             WebSocketMessage::PublicTrade { symbol, price, qty, side, timestamp } => {
                  self.handle_public_trade_update(symbol.clone(), price, qty, side, timestamp).await?;
                  // Можно не проверять устаревание после каждой сделки, чтобы не спамить проверками
             }
             WebSocketMessage::Error(error_message) => {
                  warn!(operation_id = self.operation_id, %error_message, "Received error message from WebSocket stream");
             }
             WebSocketMessage::Disconnected => {
                  error!(operation_id = self.operation_id, "WebSocket disconnected event received.");
                  return Err(anyhow!("WebSocket disconnected")); // Возвращаем ошибку для остановки run loop
             }
             WebSocketMessage::Pong => { /* Pong обрабатывается в read_loop */ }
             WebSocketMessage::Authenticated(_) | WebSocketMessage::SubscriptionResponse { .. } | WebSocketMessage::Connected => {
                  // Игнорируем системные сообщения в этом обработчике
             }
         }
         Ok(())
     }

     // --- Логика перестановки ордеров ---

    // Проверяет активные ордера на устаревание цены
    async fn check_stale_orders(&mut self) -> Result<()> {
        // Не проверяем, если мы не в активном состоянии чанка
         if !matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) { return Ok(()); }

        if let Some(stale_ratio) = self.config.ws_stale_price_ratio {
            if stale_ratio <= 0.0 { return Ok(()); } // Проверка отключена

            let stale_ratio_decimal = Decimal::try_from(stale_ratio).context("Invalid stale price ratio")?;

            // Проверка спот ордера
            let spot_order_opt = self.state.active_spot_order.clone(); // Клонируем, чтобы избежать borrow checker проблем
             if let Some(spot_order) = spot_order_opt {
                 if matches!(spot_order.status, OrderStatusText::New | OrderStatusText::PartiallyFilled) {
                     if let Some(best_ask) = self.state.spot_market_data.best_ask_price { // Для Buy ордера сравниваем с Ask
                         if spot_order.limit_price < best_ask * (Decimal::ONE - stale_ratio_decimal) {
                             warn!(operation_id = self.operation_id, order_id = %spot_order.order_id,
                                   limit_price = %spot_order.limit_price, best_ask = %best_ask,
                                   "Spot order price is stale! Triggering replacement.");
                             // Инициируем замену, но не ждем ее завершения здесь
                              self.initiate_order_replacement(Leg::Spot, "StalePrice".to_string()).await?;
                             return Ok(()); // Выходим, одна замена за раз
                         }
                     }
                 }
            }

             // Проверка фьючерс ордера (только если спот не заменяется)
             let futures_order_opt = self.state.active_futures_order.clone();
             if let Some(futures_order) = futures_order_opt {
                  if matches!(futures_order.status, OrderStatusText::New | OrderStatusText::PartiallyFilled) {
                      if let Some(best_bid) = self.state.futures_market_data.best_bid_price { // Для Sell ордера сравниваем с Bid
                          if futures_order.limit_price > best_bid * (Decimal::ONE + stale_ratio_decimal) {
                              warn!(operation_id = self.operation_id, order_id = %futures_order.order_id,
                                    limit_price = %futures_order.limit_price, best_bid = %best_bid,
                                    "Futures order price is stale! Triggering replacement.");
                               self.initiate_order_replacement(Leg::Futures, "StalePrice".to_string()).await?;
                               return Ok(()); // Выходим
                          }
                      }
                  }
             }
        }
        Ok(())
    }


     // Инициирует процесс замены ордера (отправляет cancel)
     async fn initiate_order_replacement(&mut self, leg_to_cancel: Leg, reason: String) -> Result<()> {
         let (order_to_cancel_opt, symbol, is_spot) = match leg_to_cancel {
             Leg::Spot => (self.state.active_spot_order.clone(), &self.state.symbol_spot, true),
             Leg::Futures => (self.state.active_futures_order.clone(), &self.state.symbol_futures, false),
         };

         if let Some(order_to_cancel) = order_to_cancel_opt {
             // Проверяем текущий статус, чтобы не отменять уже отменяющийся ордер
              if matches!(self.state.status, HedgerWsStatus::CancellingOrder{..} | HedgerWsStatus::WaitingCancelConfirmation{..}) {
                   warn!(operation_id=self.operation_id, order_id=%order_to_cancel.order_id, "Replacement already in progress. Skipping.");
                   return Ok(());
              }

             info!(operation_id = self.operation_id, order_id = %order_to_cancel.order_id, ?leg_to_cancel, %reason, "Initiating order replacement: sending cancel request...");

             let current_chunk = match self.state.status {
                 HedgerWsStatus::RunningChunk(idx) => idx,
                 _ => self.state.current_chunk_index // Берем текущий индекс
             };
              self.state.status = HedgerWsStatus::CancellingOrder {
                  chunk_index: current_chunk,
                  leg_to_cancel,
                  order_id_to_cancel: order_to_cancel.order_id.clone(),
                  reason,
              };

              // Отправляем запрос на отмену через REST
              let cancel_result = if is_spot {
                   self.exchange_rest.cancel_spot_order(symbol, &order_to_cancel.order_id).await
              } else {
                   self.exchange_rest.cancel_futures_order(symbol, &order_to_cancel.order_id).await
              };

              if let Err(e) = cancel_result {
                   error!(operation_id = self.operation_id, order_id = %order_to_cancel.order_id, %e, "Failed to send cancel request for order replacement");
                   // Возвращаемся в RunningChunk, т.к. отмена не удалась
                   self.state.status = HedgerWsStatus::RunningChunk(current_chunk);
                   return Err(e).context("Failed to send cancel request");
              } else {
                   info!(operation_id = self.operation_id, order_id = %order_to_cancel.order_id, "Cancel request sent. Waiting for WebSocket confirmation...");
                   // Переходим в состояние ожидания подтверждения отмены
                   self.state.status = HedgerWsStatus::WaitingCancelConfirmation {
                       chunk_index: current_chunk,
                       cancelled_leg: leg_to_cancel,
                       cancelled_order_id: order_to_cancel.order_id.clone(),
                   };
              }

         } else {
             warn!(operation_id = self.operation_id, ?leg_to_cancel, "Attempted to replace order, but no active order found for leg.");
         }
         Ok(())
     }

     // Обработка подтверждения отмены и размещение нового ордера
     async fn handle_cancel_confirmation(&mut self, cancelled_order_id: &str, leg: Leg) -> Result<()> {
         // Статус уже должен был обновиться через handle_order_update, здесь мы просто перевыставляем
          info!(operation_id = self.operation_id, %cancelled_order_id, ?leg, "Handling cancel confirmation: placing replacement order if needed...");

          // Определяем оставшееся количество
          let remaining_quantity: Decimal;
          // Важно: берем из кумулятивного состояния, а не из отмененного ордера
          match leg {
               Leg::Spot => {
                   let total_target = self.state.initial_target_spot_value / self.get_current_price(Leg::Spot).unwrap_or(Decimal::ONE); // Приблиз. общий объем
                   remaining_quantity = (total_target - self.state.cumulative_spot_filled_quantity).max(Decimal::ZERO);
                   self.state.active_spot_order = None; // Убираем отмененный
               }
               Leg::Futures => {
                    // Цель фьюча динамическая, берем рассчитанную
                     let current_futures_target = self.state.target_total_futures_value / self.get_current_price(Leg::Futures).unwrap_or(Decimal::ONE);
                     remaining_quantity = (current_futures_target - self.state.cumulative_futures_filled_quantity).max(Decimal::ZERO);
                    self.state.active_futures_order = None; // Убираем отмененный
               }
          };

          let min_quantity = match leg {
               Leg::Spot => self.state.min_spot_quantity,
               Leg::Futures => self.state.min_futures_quantity,
          };
          let step = match leg {
               Leg::Spot => self.state.spot_quantity_step,
               Leg::Futures => self.state.futures_quantity_step,
          };

           // Округляем остаток ВНИЗ по шагу, чтобы не превысить цель
          let remaining_quantity_rounded = round_down_step(remaining_quantity, step)?;


           if remaining_quantity_rounded < min_quantity && remaining_quantity_rounded.abs() > dec!(1e-12) {
                 warn!(operation_id=self.operation_id, %remaining_quantity_rounded, %min_quantity, ?leg, "Remaining quantity after cancel is dust. Not placing replacement order.");
           }
           else if remaining_quantity_rounded > dec!(1e-12) { // Если осталось что исполнять (больше пыли)
               info!(operation_id = self.operation_id, %remaining_quantity_rounded, ?leg, "Placing replacement order...");
               let new_limit_price = self.calculate_limit_price_for_leg(leg)?;

                let (symbol, side, qty_precision, price_precision) = match leg {
                    Leg::Spot => (&self.state.symbol_spot, OrderSide::Buy, self.state.spot_quantity_step.scale(), self.state.spot_tick_size.scale()),
                    Leg::Futures => (&self.state.symbol_futures, OrderSide::Sell, self.state.futures_quantity_step.scale(), self.state.futures_tick_size.scale()),
                };

                // Финальное округление перед отправкой
                let quantity_f64 = remaining_quantity_rounded.round_dp(qty_precision).to_f64().unwrap_or(0.0);
                let price_f64 = new_limit_price.round_dp(price_precision).to_f64().unwrap_or(0.0);

                 let place_result = if leg == Leg::Spot {
                      self.exchange_rest.place_limit_order(symbol, side, quantity_f64, price_f64).await
                 } else {
                      self.exchange_rest.place_futures_limit_order(symbol, side, quantity_f64, price_f64).await
                 };

                 match place_result {
                      Ok(new_order) => {
                          info!(operation_id=self.operation_id, new_order_id=%new_order.id, ?leg, "Replacement order placed successfully.");
                          let new_order_state = ChunkOrderState::new(
                               new_order.id,
                               symbol.to_string(),
                               side,
                               new_limit_price, // Сохраняем цену, по которой выставили
                               remaining_quantity_rounded // Цель - округленный остаток
                          );
                          match leg {
                               Leg::Spot => self.state.active_spot_order = Some(new_order_state),
                               Leg::Futures => self.state.active_futures_order = Some(new_order_state),
                          }
                      }
                      Err(e) => {
                           error!(operation_id = self.operation_id, %e, ?leg, "Failed to place replacement order!");
                           self.state.status = HedgerWsStatus::Failed(format!("Failed place replacement {:?} order: {}", leg, e));
                           self.update_final_db_status().await;
                           return Err(e).context("Failed to place replacement order");
                      }
                 }
           } else {
                 info!(operation_id=self.operation_id, ?leg, "No remaining quantity after cancel confirmation. Not placing replacement.");
           }

            // Возвращаемся в статус RunningChunk после успешной перестановки или если переставлять нечего
             let current_chunk = match self.state.status {
                 HedgerWsStatus::WaitingCancelConfirmation { chunk_index, .. } => chunk_index,
                 _ => self.state.current_chunk_index -1 // Берем индекс из состояния ожидания
             };
            self.state.status = HedgerWsStatus::RunningChunk(current_chunk);

          Ok(())
     }

      // Обработка обновления рыночных данных (стакан, сделки)
      async fn handle_market_data_update(&mut self, _symbol: String, _bids: Vec<OrderbookLevel>, _asks: Vec<OrderbookLevel>, _is_snapshot: bool) -> Result<()> {
          // TODO: Обновить соответствующий MarketUpdate в self.state
          // Например:
          // let market_data_ref = if symbol == self.state.symbol_spot { ... }
          // market_data_ref.best_bid_price = bids.first().map(|l| l.price);
          // ... и т.д.
          // tracing::trace!(operation_id=self.operation_id, %symbol, "Handling market data update");
          Ok(())
      }
      async fn handle_public_trade_update(&mut self, _symbol: String, _price: f64, _qty: f64, _side: OrderSide, _timestamp: i64) -> Result<()> {
           // TODO: Обновить last_trade_price или использовать для определения рыночной цены
           // tracing::trace!(operation_id=self.operation_id, %symbol, price, "Handling public trade update");
           Ok(())
      }

     // --- Остальные вспомогательные методы как в предыдущем шаге ---
     // (handle_order_update, check_chunk_completion, calculate_limit_price_for_leg,
     //  get_decimals_from_step, get_step_decimal, calculate_auto_chunks, round_up_step,
     //  send_progress_update, update_final_db_status)

     // Определение лимитной цены для выставления ордера
     fn calculate_limit_price_for_leg(&self, leg: Leg) -> Result<Decimal> {
         let (market_data, side, tick_size) = match leg {
             Leg::Spot => (&self.state.spot_market_data, OrderSide::Buy, self.state.spot_tick_size),
             Leg::Futures => (&self.state.futures_market_data, OrderSide::Sell, self.state.futures_tick_size),
         };

         let reference_price = match side {
             OrderSide::Buy => market_data.best_ask_price.ok_or_else(|| anyhow!("No best ask price available for {:?}", leg)),
             OrderSide::Sell => market_data.best_bid_price.ok_or_else(|| anyhow!("No best bid price available for {:?}", leg)),
         }?;

          match self.config.ws_limit_order_placement_strategy {
              WsLimitOrderPlacementStrategy::BestAskBid => Ok(reference_price),
              WsLimitOrderPlacementStrategy::OneTickInside => {
                   match side {
                       OrderSide::Buy => Ok((reference_price - tick_size).max(tick_size)), // Не ниже шага цены
                       OrderSide::Sell => Ok(reference_price + tick_size),
                   }
              }
         }
     }
     // Округление вниз с шагом
     fn round_down_step(value: Decimal, step: Decimal) -> Result<Decimal> {
          if step <= Decimal::ZERO {
             if value == Decimal::ZERO { return Ok(Decimal::ZERO); } // Ноль можно не округлять
             warn!("Rounding step is zero or negative: {}", step);
             return Ok(value.normalize());
          }
          if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
          let positive_step = step.abs();
          let precision = positive_step.scale() + 3;
          let value_scaled = value.round_dp(precision);
          let step_scaled = positive_step.round_dp(precision);
          Ok(((value_scaled / step_scaled).floor() * step_scaled).normalize())
      }
     // Определение текущей цены для расчетов (упрощенно)
      fn get_current_price(&self, leg: Leg) -> Option<Decimal> {
           let market_data = match leg {
                Leg::Spot => &self.state.spot_market_data,
                Leg::Futures => &self.state.futures_market_data,
           };
           // Берем среднее между бидом и аском, если они есть
            match (market_data.best_bid_price, market_data.best_ask_price) {
                (Some(bid), Some(ask)) => Some((bid + ask) / dec!(2.0)),
                (Some(bid), None) => Some(bid),
                (None, Some(ask)) => Some(ask),
                (None, None) => None,
            }
      }
     // handle_order_update, check_chunk_completion, get_decimals_from_step, get_step_decimal,
      // calculate_auto_chunks, round_up_step, send_progress_update, update_final_db_status
      // Копируем их из предыдущей версии кода, т.к. они в основном остаются теми же
      // (нужно только передать Db в update_final_db_status)
     fn get_decimals_from_step(step: Option<&str>) -> Result<u32> {
        let step_str = step.ok_or_else(|| anyhow!("Missing qtyStep/basePrecision"))?;
        Ok(step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32)
    }

     fn get_step_decimal(step: Option<&str>) -> Result<Decimal> {
         let step_str = step.ok_or_else(|| anyhow!("Missing qtyStep/basePrecision"))?;
         Decimal::from_str(step_str).context("Failed to parse step string to Decimal")
     }

    // Обработка обновления ордера из WebSocket
     async fn handle_order_update(&mut self, details: DetailedOrderStatus) -> Result<()> {
         info!(operation_id = self.operation_id, order_id = %details.order_id, status = ?details.status_text, filled_qty = details.filled_qty, "Handling order update");

         let mut target_order_state: Option<&mut ChunkOrderState> = None;
         let is_spot_order = self.state.active_spot_order.as_ref().map_or(false, |order| order.order_id == details.order_id);
         let is_futures_order = self.state.active_futures_order.as_ref().map_or(false, |order| order.order_id == details.order_id);

         // Находим ордер в состоянии
         if is_spot_order {
             target_order_state = self.state.active_spot_order.as_mut();
         } else if is_futures_order {
              target_order_state = self.state.active_futures_order.as_mut();
         } else {
             warn!(operation_id = self.operation_id, order_id = %details.order_id, "Received update for an unknown or inactive order. Ignoring.");
             return Ok(());
         }

         // Обновляем состояние найденного ордера
         if let Some(order_state) = target_order_state {
             let old_filled_qty = order_state.filled_quantity;
             order_state.update_from_details(&details);
             let new_filled_qty = order_state.filled_quantity;
             // Используем Decimal для разницы - ВАЖНО!
             let quantity_diff = new_filled_qty - old_filled_qty;

             // Обновляем кумулятивные значения только если что-то реально исполнилось
              let tolerance = dec!(1e-12); // Небольшой допуск для Decimal
             if quantity_diff.abs() > tolerance {
                   let price_for_diff = if order_state.average_price > Decimal::ZERO {
                       order_state.average_price
                   } else if let Some(last_price_f64) = details.last_filled_price {
                       Decimal::try_from(last_price_f64).context("Failed converting last_filled_price")?
                   } else {
                       warn!(order_id=%details.order_id, "Cannot determine price for value calculation in order update");
                        // Пытаемся взять цену из market data как fallback
                        self.get_current_price(if is_spot_order { Leg::Spot } else { Leg::Futures })
                            .ok_or_else(|| anyhow!("Cannot determine price for value calculation for order {}", details.order_id))?
                   };

                  let value_diff = quantity_diff * price_for_diff;

                 if is_spot_order {
                     self.state.cumulative_spot_filled_quantity += quantity_diff;
                     self.state.cumulative_spot_filled_value += value_diff;
                     // !!! Пересчитываем цель для фьючерса !!!
                     self.state.target_total_futures_value = self.state.cumulative_spot_filled_value;
                     debug!(operation_id = self.operation_id,
                            new_spot_value = %self.state.cumulative_spot_filled_value,
                            new_target_futures_value = %self.state.target_total_futures_value,
                            "Recalculated target futures value based on spot fill.");
                     // TODO: Здесь может быть логика немедленной корректировки активного фьюч ордера, если нужно
                 } else if is_futures_order {
                      self.state.cumulative_futures_filled_quantity += quantity_diff;
                      self.state.cumulative_futures_filled_value += value_diff.abs(); // Используем abs() для фьюча
                 }
                 // Отправляем прогресс
                 self.send_progress_update().await?;
             }

             // Если ордер завершен (Filled, Cancelled, Rejected)
             if matches!(order_state.status, OrderStatusText::Filled | OrderStatusText::Cancelled | OrderStatusText::PartiallyFilledCanceled | OrderStatusText::Rejected) {
                  info!(operation_id = self.operation_id, order_id = %order_state.order_id, status = ?order_state.status, "Order finished.");
                  // Убираем ордер из активных
                  if is_spot_order { self.state.active_spot_order = None; }
                  if is_futures_order { self.state.active_futures_order = None; }
             }
         }
         Ok(())
     }

      // Проверка, завершен ли текущий чанк (оба активных ордера отсутствуют)
      fn check_chunk_completion(&self) -> bool {
          self.state.active_spot_order.is_none() && self.state.active_futures_order.is_none()
      }

       // Отправка прогресса через колбэк
       async fn send_progress_update(&mut self) -> Result<()> {
            use crate::hedger::HedgeProgressUpdate; // Импортируем структуру для колбэка

            // TODO: Определить текущий stage и другие параметры для HedgeProgressUpdate
            // Например, общий процент выполнения можно считать как:
             let progress_percent = if self.state.initial_target_spot_value > Decimal::ZERO {
                  (self.state.cumulative_spot_filled_value / self.state.initial_target_spot_value * dec!(100.0))
                  .round_dp(1).to_f64().unwrap_or(0.0)
             } else { 0.0 };

             debug!(operation_id=self.operation_id, progress = progress_percent, "Sending progress update (placeholder)...");

            // Примерный вызов (нужно заполнить реальными данными)
            /*
            let update = HedgeProgressUpdate {
                 stage: if self.state.active_spot_order.is_some() { crate::hedger::HedgeStage::Spot } else { crate::hedger::HedgeStage::Futures },
                 current_spot_price: self.get_current_price(Leg::Spot).map(|d|d.to_f64().unwrap_or(0.0)).unwrap_or(0.0),
                 new_limit_price: 0.0, // Цена текущего активного ордера?
                 is_replacement: false, // Сложно определить здесь
                 filled_qty: 0.0, // Последнее исполнение?
                 target_qty: 0.0, // Цель текущего ордера?
                 cumulative_filled_qty: self.state.cumulative_spot_filled_quantity.to_f64().unwrap_or(0.0), // Или фьюча?
                 total_target_qty: self.state.initial_target_spot_value.to_f64().unwrap_or(0.0), // Или фьюча?
            };
             (self.progress_callback)(update).await?;
             */

            Ok(())
       }

        // Обновление финального статуса в БД
        async fn update_final_db_status(&self) {
             let status_str = match &self.state.status {
                 HedgerWsStatus::Completed => "Completed",
                 HedgerWsStatus::Cancelled => "Cancelled", // TODO: Обработать отмену пользователем
                 HedgerWsStatus::Failed(_) => "Failed",
                 _ => {
                      warn!(operation_id = self.operation_id, status=?self.state.status, "Attempted to update DB with non-final status.");
                      return;
                 } // Не финальный статус
             };
             let error_message = match &self.state.status {
                  HedgerWsStatus::Failed(msg) => Some(msg.as_str()),
                  _ => None,
             };

             // TODO: Получить ID последнего спотового и фьючерсного ордера из state (если храним историю или берем из активных перед очисткой)
             let last_spot_order_id : Option<&str> = None;
             let last_futures_order_id : Option<&str> = None;

             let spot_qty_f64 = self.state.cumulative_spot_filled_quantity.to_f64().unwrap_or(0.0);
             let fut_qty_f64 = self.state.cumulative_futures_filled_quantity.to_f64().unwrap_or(0.0);

             if let Err(e) = crate::storage::update_hedge_final_status(
                  &self.database, // Используем self.database
                  self.operation_id,
                  status_str,
                  last_spot_order_id, // Передаем ID последнего спот ордера
                  spot_qty_f64,
                  last_futures_order_id, // Передаем ID последнего фьюч ордера
                  fut_qty_f64,
                  error_message,
             ).await {
                  error!(operation_id=self.operation_id, "Failed to update final DB status: {}", e);
             } else {
                  info!(operation_id=self.operation_id, status=status_str, "Final DB status updated.");
             }
        }


} // --- Конец impl HedgerWsHedgeTask ---