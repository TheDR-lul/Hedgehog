// src/hedger_ws/unhedge_task.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use std::time::Duration;
use tokio::time::sleep;

use crate::config::{Config, WsLimitOrderPlacementStrategy};
use crate::exchange::{Exchange, bybit::SPOT_CATEGORY, bybit::LINEAR_CATEGORY}; // LINEAR_CATEGORY нужен для инфо
use crate::exchange::types::{WebSocketMessage, DetailedOrderStatus, OrderSide, OrderStatusText, OrderbookLevel};
use crate::hedger::HedgeProgressCallback;
use crate::storage::{self, HedgeOperation}; // Входные параметры - запись из БД
use super::state::{HedgerWsState, HedgerWsStatus, OperationType, MarketUpdate, ChunkOrderState, Leg};
use super::common::calculate_auto_chunk_parameters; // Используем общую функцию расчета чанков

// Структура для управления задачей расхеджирования
pub struct HedgerWsUnhedgeTask {
    operation_id: i64, // ID исходной операции хеджирования
    config: Arc<Config>,
    database: Arc<storage::Db>,
    state: HedgerWsState,
    ws_receiver: mpsc::Receiver<Result<WebSocketMessage>>,
    exchange_rest: Arc<dyn Exchange>,
    progress_callback: HedgeProgressCallback,
    original_spot_target: Decimal,  // Сохраняем изначальную цель по споту
    original_futures_target: Decimal, // Сохраняем изначальную цель по фьючу
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
        let operation_id = original_operation.id;
        info!(operation_id, "Initializing HedgerWsUnhedgeTask...");

        // --- 1. Получение целей и информации об инструментах ---
        let base_symbol = original_operation.base_symbol.to_uppercase();
        let spot_symbol_name = format!("{}{}", base_symbol, config.quote_currency);
        let futures_symbol_name = format!("{}{}", base_symbol, config.quote_currency);

        // Цели из оригинальной операции
        let target_spot_sell_quantity = Decimal::try_from(original_operation.spot_filled_qty)
            .context("Failed to convert original spot_filled_qty to Decimal")?;
        let target_futures_buy_quantity = Decimal::try_from(original_operation.target_futures_qty)
            .context("Failed to convert original target_futures_qty to Decimal")?;

        if target_spot_sell_quantity <= Decimal::ZERO || target_futures_buy_quantity <= Decimal::ZERO {
             return Err(anyhow!("Original operation quantities are zero or negative. Cannot unhedge. Spot: {}, Futures: {}", target_spot_sell_quantity, target_futures_buy_quantity));
        }

        debug!(operation_id, %spot_symbol_name, %futures_symbol_name, %target_spot_sell_quantity, %target_futures_buy_quantity, "Fetching instrument info and balance...");

        // Параллельные запросы
        let ((spot_info_res, linear_info_res), spot_balance_res, spot_price_res, futures_price_res) = tokio::join!(
             tokio::join!(
                 exchange_rest.get_spot_instrument_info(&base_symbol),
                 exchange_rest.get_linear_instrument_info(&base_symbol)
             ),
             exchange_rest.get_balance(&base_symbol), // Баланс спота
             exchange_rest.get_spot_price(&base_symbol), // Цена спота
             exchange_rest.get_market_price(&futures_symbol_name, false) // Цена фьючерса
        );

        let spot_info = spot_info_res.context("Failed to get SPOT instrument info")?;
        let linear_info = linear_info_res.context("Failed to get LINEAR instrument info")?;
        let spot_balance = spot_balance_res.context("Failed to get SPOT balance")?;
        let current_spot_price_f64 = spot_price_res.context("Failed to get current SPOT price")?;
        let current_futures_price_f64 = futures_price_res.context("Failed to get current FUTURES price")?;

        if current_spot_price_f64 <= 0.0 || current_futures_price_f64 <= 0.0 {
             return Err(anyhow!("Spot or Futures price is non-positive"));
        }
        let current_spot_price = Decimal::try_from(current_spot_price_f64)?;
        let current_futures_price = Decimal::try_from(current_futures_price_f64)?;


        // --- 2. Проверка баланса и корректировка цели спота ---
        let available_spot_balance = Decimal::try_from(spot_balance.free)?;
        let mut actual_spot_sell_target_quantity = target_spot_sell_quantity;

        if available_spot_balance < target_spot_sell_quantity {
            warn!(operation_id,
                  target = %target_spot_sell_quantity,
                  available = %available_spot_balance,
                  "Available spot balance is less than target sell quantity. Adjusting target.");
            actual_spot_sell_target_quantity = available_spot_balance;
        }
        // Если доступный баланс вообще нулевой или меньше минимального ордера - ошибка
        let min_spot_quantity = Decimal::from_str(&spot_info.lot_size_filter.min_order_qty)?;
        if actual_spot_sell_target_quantity < min_spot_quantity {
             return Err(anyhow!("Available spot balance ({}) is less than minimum order quantity ({}) for {}. Cannot unhedge.", actual_spot_sell_target_quantity, min_spot_quantity, spot_symbol_name));
        }

        // Оценочная стоимость спота (для расчетов чанков)
        let initial_target_spot_value = actual_spot_sell_target_quantity * current_spot_price;


        // --- 3. Автоматический Расчет Чанков ---
        debug!(operation_id, status=?HedgerWsStatus::CalculatingChunks);
        let target_chunk_count = config.ws_auto_chunk_target_count;

        let min_futures_quantity = Decimal::from_str(&linear_info.lot_size_filter.min_order_qty)?;
        let spot_quantity_step = Decimal::from_str(spot_info.lot_size_filter.base_precision.as_deref().unwrap_or("1"))?; // Используем base_precision как шаг
        let futures_quantity_step = Decimal::from_str(linear_info.lot_size_filter.qty_step.as_deref().unwrap_or("1"))?;
        let min_spot_notional = spot_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());
        let min_futures_notional = linear_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());

        // Расчет чанков
        let (final_chunk_count, chunk_spot_quantity, chunk_futures_quantity) =
            calculate_auto_chunk_parameters(
                initial_target_spot_value,          // Используем скорректированную стоимость
                target_futures_buy_quantity,        // Фиксированная цель фьюча
                current_spot_price,
                current_futures_price,
                target_chunk_count,
                min_spot_quantity,
                min_futures_quantity,
                spot_quantity_step,
                futures_quantity_step,
                min_spot_notional,
                min_futures_notional,
            )?;
        info!(operation_id, final_chunk_count, %chunk_spot_quantity, %chunk_futures_quantity, "Unhedge chunk parameters calculated");

        // --- 4. Создание начального состояния ---
        let spot_tick_size = Decimal::from_str(&spot_info.price_filter.tick_size)?;
        let futures_tick_size = Decimal::from_str(&linear_info.price_filter.tick_size)?;

        let mut state = HedgerWsState::new_unhedge(
            operation_id,
            spot_symbol_name.clone(),
            futures_symbol_name.clone(),
            actual_spot_sell_target_quantity, // Скорректированная цель спота
            target_futures_buy_quantity       // Фиксированная цель фьюча
        );
        // Сохраняем лимиты и параметры чанков
        state.spot_tick_size = spot_tick_size;
        state.spot_quantity_step = spot_quantity_step;
        state.min_spot_quantity = min_spot_quantity;
        state.min_spot_notional = min_spot_notional;
        state.futures_tick_size = futures_tick_size;
        state.futures_quantity_step = futures_quantity_step;
        state.min_futures_quantity = min_futures_quantity;
        state.min_futures_notional = min_futures_notional;
        state.total_chunks = final_chunk_count;
        state.chunk_base_quantity_spot = chunk_spot_quantity;
        state.chunk_base_quantity_futures = chunk_futures_quantity;
        state.initial_target_spot_value = initial_target_spot_value; // Обновляем оценочную стоимость
        state.status = HedgerWsStatus::StartingChunk(1);

        info!(operation_id, "HedgerWsUnhedgeTask initialized successfully. Ready to run.");

        Ok(Self {
            operation_id,
            config,
            database,
            state,
            ws_receiver,
            exchange_rest,
            progress_callback,
            original_spot_target: target_spot_sell_quantity, // Сохраняем изначальную цель
            original_futures_target: target_futures_buy_quantity,
        })
    }

    // Основной цикл обработки сообщений и управления состоянием
    pub async fn run(&mut self) -> Result<()> {
        info!(operation_id = self.operation_id, "Starting HedgerWsUnhedgeTask run loop...");

        // --- Запуск первого чанка ---
        if matches!(self.state.status, HedgerWsStatus::StartingChunk(1)) {
             if let Err(error) = self.start_next_chunk().await {
                 error!(operation_id = self.operation_id, %error, "Failed to start initial unhedge chunk");
                 self.state.status = HedgerWsStatus::Failed(format!("Failed start unhedge chunk 1: {}", error));
                 self.update_final_db_status().await; // Используем общий метод для записи статуса
                 return Err(error);
             }
        } else if !matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) {
            warn!(operation_id = self.operation_id, status = ?self.state.status, "Unhedge task run started with unexpected status.");
            let error_message = format!("Unhedge task started in invalid state: {:?}", self.state.status);
            self.state.status = HedgerWsStatus::Failed(error_message.clone());
            self.update_final_db_status().await;
            return Err(anyhow!(error_message));
        }

        // --- Цикл обработки сообщений WS (Аналогичен hedge_task.run) ---
         loop {
              tokio::select! {
                  maybe_result = self.ws_receiver.recv() => {
                       match maybe_result {
                          Some(Ok(message)) => {
                              if let Err(error) = self.handle_websocket_message(message).await {
                                  error!(operation_id = self.operation_id, %error, "Error handling WebSocket message during unhedge");
                                   self.state.status = HedgerWsStatus::Failed(format!("WS Handling Error (unhedge): {}", error));
                                   self.update_final_db_status().await;
                                   return Err(error);
                              }
                          }
                          Some(Err(error)) => {
                              error!(operation_id = self.operation_id, %error, "Error received from WebSocket channel (unhedge). Stopping task.");
                              self.state.status = HedgerWsStatus::Failed(format!("WS channel error (unhedge): {}", error));
                              self.update_final_db_status().await;
                              return Err(error);
                          }
                          None => {
                              info!(operation_id = self.operation_id, "WebSocket channel closed (unhedge). Stopping task.");
                               if !matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                                   self.state.status = HedgerWsStatus::Failed("WebSocket channel closed unexpectedly (unhedge)".to_string());
                                   self.update_final_db_status().await;
                                   return Err(anyhow!("WebSocket channel closed unexpectedly (unhedge)"));
                               }
                              break;
                          }
                      }
                  }
                  _ = sleep(Duration::from_millis(100)) => {}
              } // конец select!

              // --- Логика после обработки сообщения или паузы ---
              if matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) && self.check_chunk_completion() {
                  let current_chunk = match self.state.status {
                      HedgerWsStatus::RunningChunk(index) => index,
                      _ => self.state.current_chunk_index -1,
                  };
                  debug!(operation_id=self.operation_id, chunk_index=current_chunk, "Unhedge chunk completed processing.");

                  if current_chunk < self.state.total_chunks {
                      let next_chunk_index = current_chunk + 1;
                       // Проверка дисбаланса для unhedge менее критична, но можно оставить
                       if self.check_value_imbalance() {
                            info!(operation_id=self.operation_id, chunk_index=current_chunk, "Unhedge value imbalance detected. Waiting...");
                            let leading = if self.state.cumulative_spot_filled_value > self.state.cumulative_futures_filled_value.abs() { Leg::Spot } else { Leg::Futures };
                            self.state.status = HedgerWsStatus::WaitingImbalance { chunk_index: current_chunk, leading_leg: leading };
                       } else {
                            self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                            if let Err(error) = self.start_next_chunk().await {
                                error!(operation_id = self.operation_id, chunk = next_chunk_index, %error, "Failed to start next unhedge chunk");
                                self.state.status = HedgerWsStatus::Failed(format!("Failed start unhedge chunk {}: {}", next_chunk_index, error));
                                self.update_final_db_status().await;
                                return Err(error);
                            }
                       }
                  } else {
                      info!(operation_id = self.operation_id, "All unhedge chunks processed, moving to Reconciling state.");
                      self.state.status = HedgerWsStatus::Reconciling;
                      if let Err(error) = self.reconcile().await {
                          error!(operation_id = self.operation_id, %error, "Unhedge reconciliation failed");
                          self.state.status = HedgerWsStatus::Failed(format!("Unhedge reconciliation failed: {}", error));
                          self.update_final_db_status().await;
                          return Err(error);
                      }
                      info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting unhedge run loop after successful reconciliation.");
                      break;
                  }
              } else if matches!(self.state.status, HedgerWsStatus::WaitingImbalance { .. }) {
                   if !self.check_value_imbalance() {
                        info!(operation_id=self.operation_id, "Unhedge value imbalance resolved. Proceeding.");
                         let next_chunk_index = self.state.current_chunk_index;
                         self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                          if let Err(error) = self.start_next_chunk().await {
                              error!(operation_id = self.operation_id, chunk = next_chunk_index, %error, "Failed start unhedge chunk after imbalance resolved");
                              self.state.status = HedgerWsStatus::Failed(format!("Failed start unhedge chunk {}: {}", next_chunk_index, error));
                              self.update_final_db_status().await;
                              return Err(error);
                          }
                   }
              }

              // Проверка статуса для выхода
              if matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                  info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting unhedge run loop due to final status.");
                  break;
              }
         } // конец loop

        if self.state.status == HedgerWsStatus::Completed { Ok(()) }
        else { Err(anyhow!("Unhedge task run loop exited with non-completed status: {:?}", self.state.status)) }
    }


    // Функция для начала обработки следующего чанка (Unhedge)
    async fn start_next_chunk(&mut self) -> Result<()> {
        let chunk_index = match self.state.status {
             HedgerWsStatus::StartingChunk(index) => index,
             HedgerWsStatus::RunningChunk(index) => { warn!(/*...*/); index },
             _ => return Err(anyhow!("start_next_chunk (unhedge) called in unexpected state: {:?}", self.state.status)),
        };
        info!(operation_id = self.operation_id, chunk_index, "Starting unhedge chunk placement...");
        self.state.active_spot_order = None;
        self.state.active_futures_order = None;

        // --- Расчет объемов для текущего чанка (Unhedge) ---
         let spot_quantity_chunk: Decimal;
         let futures_quantity_chunk: Decimal;
         let is_last_chunk = chunk_index == self.state.total_chunks;

         if is_last_chunk {
             // Последний чанк - берем остаток
             spot_quantity_chunk = (self.state.initial_target_spot_value / self.get_current_price(Leg::Spot).unwrap_or(Decimal::ONE) - self.state.cumulative_spot_filled_quantity).max(Decimal::ZERO);
             futures_quantity_chunk = (self.state.initial_target_futures_qty - self.state.cumulative_futures_filled_quantity).max(Decimal::ZERO);
             info!(operation_id=self.operation_id, chunk_index, "Calculating quantities for last unhedge chunk.");
         } else {
             spot_quantity_chunk = self.state.chunk_base_quantity_spot;
             futures_quantity_chunk = self.state.chunk_base_quantity_futures;
         }
         debug!(operation_id=self.operation_id, chunk_index, %spot_quantity_chunk, %futures_quantity_chunk, "Calculated unhedge chunk quantities.");

         let spot_quantity_rounded = self.round_down_step(spot_quantity_chunk, self.state.spot_quantity_step)?;
         let futures_quantity_rounded = self.round_down_step(futures_quantity_chunk, self.state.futures_quantity_step)?;

        // Проверка на минимум
         let tolerance = dec!(1e-12);
         let place_spot = spot_quantity_rounded >= self.state.min_spot_quantity || spot_quantity_rounded < tolerance;
         let place_futures = futures_quantity_rounded >= self.state.min_futures_quantity || futures_quantity_rounded < tolerance;

        if !place_spot && !place_futures { /* ... обработка пропуска чанка ... */ return Ok(()); }
        if !place_spot { warn!(/*...*/); }
        if !place_futures { warn!(/*...*/); }

        // --- Определение лимитных цен (Unhedge) ---
         let spot_limit_price = if place_spot { Some(self.calculate_limit_price_for_leg(Leg::Spot)?) } else { None };       // Sell Spot -> цена на основе Bid
         let futures_limit_price = if place_futures { Some(self.calculate_limit_price_for_leg(Leg::Futures)?) } else { None }; // Buy Futures -> цена на основе Ask

        // --- Выставление ордеров через REST API (Unhedge) ---
        let mut placed_spot_order: Option<ChunkOrderState> = None;
        let mut placed_futures_order: Option<ChunkOrderState> = None;

        // Выставляем фьючерс первым (покупка), чтобы обеспечить хедж быстрее? Или спот? Пока спот.
        if let (true, Some(limit_price)) = (place_spot, spot_limit_price) {
            self.state.status = HedgerWsStatus::PlacingSpotOrder(chunk_index);
            let qty_f64 = spot_quantity_rounded.round_dp(self.state.spot_quantity_step.scale()).to_f64().unwrap_or(0.0);
            let price_f64 = limit_price.round_dp(self.state.spot_tick_size.scale()).to_f64().unwrap_or(0.0);
            if qty_f64 <= 0.0 || price_f64 <= 0.0 { /* ... ошибка ... */ return Err(anyhow!("Invalid spot params")); }

            let spot_order_result = self.exchange_rest.place_limit_order(
                 &self.state.symbol_spot, OrderSide::Sell, qty_f64, price_f64 // SELL SPOT
            ).await;
             match spot_order_result { Ok(o) => { placed_spot_order = Some(ChunkOrderState::new(o.id, self.state.symbol_spot.clone(), OrderSide::Sell, limit_price, spot_quantity_rounded)); }, Err(e) => { /* ... ошибка + выход ... */ return Err(e); } };
        }

         if let (true, Some(limit_price)) = (place_futures, futures_limit_price) {
             self.state.status = HedgerWsStatus::PlacingFuturesOrder(chunk_index);
              let qty_f64 = futures_quantity_rounded.round_dp(self.state.futures_quantity_step.scale()).to_f64().unwrap_or(0.0);
              let price_f64 = limit_price.round_dp(self.state.futures_tick_size.scale()).to_f64().unwrap_or(0.0);
              if qty_f64 <= 0.0 || price_f64 <= 0.0 { /* ... ошибка + откат спота ... */ return Err(anyhow!("Invalid futures params")); }

              let futures_order_result = self.exchange_rest.place_futures_limit_order(
                   &self.state.symbol_futures, OrderSide::Buy, qty_f64, price_f64 // BUY FUTURES
              ).await;
               match futures_order_result { Ok(o) => { placed_futures_order = Some(ChunkOrderState::new(o.id, self.state.symbol_futures.clone(), OrderSide::Buy, limit_price, futures_quantity_rounded)); }, Err(e) => { /* ... ошибка + откат спота ... */ return Err(e); } };
         }

        // Обновляем состояние
        self.state.active_spot_order = placed_spot_order;
        self.state.active_futures_order = placed_futures_order;
        if self.state.active_spot_order.is_some() || self.state.active_futures_order.is_some() { self.state.current_chunk_index += 1; }
        self.state.status = HedgerWsStatus::RunningChunk(chunk_index);
        info!(/* ... */);

        Ok(())
    }

     // Функция для финальной стадии (Unhedge)
     async fn reconcile(&mut self) -> Result<()> {
         info!(operation_id = self.operation_id, "Starting final unhedge reconciliation...");
         // TODO: Рассчитать финальный дисбаланс объемов (qty) относительно целей
         //       spot: actual_spot_sell_target_quantity vs cumulative_spot_filled_quantity
         //       futures: target_futures_buy_quantity vs cumulative_futures_filled_quantity
         // TODO: Выставить 1-2 рыночных ордера через REST для закрытия дисбаланса

         // --- ВАЖНО: Помечаем исходную операцию как расхеджированную ---
          match storage::mark_hedge_as_unhedged(&self.database, self.operation_id).await {
               Ok(_) => info!(operation_id=self.operation_id, "Marked original hedge operation as unhedged."),
               Err(e) => error!(operation_id=self.operation_id, %e, "Failed to mark original hedge operation as unhedged in DB!"), // Не фатально для самой операции, но плохо для учета
          }

         self.state.status = HedgerWsStatus::Completed;
         self.update_final_db_status().await; // Обновляем статус в БД
         info!(operation_id = self.operation_id, "Unhedge reconciliation complete.");
         Ok(())
     }

     // --- Обработка WS сообщений (используем методы из hedge_task, т.к. логика обработки одинакова) ---
     async fn handle_websocket_message(&mut self, message: WebSocketMessage) -> Result<()> {
         // Вызываем реализацию из hedge_task (можно вынести в common или трейт)
          HedgerWsHedgeTask::handle_websocket_message(self, message).await
     }
     async fn handle_order_update(&mut self, details: DetailedOrderStatus) -> Result<()> {
         // Вызываем реализацию из hedge_task
         // Важно: target_total_futures_value НЕ пересчитывается при Unhedge!
          info!(operation_id = self.operation_id, order_id = %details.order_id, status = ?details.status_text, filled_qty = details.filled_qty, "Handling UNHEDGE order update");
          let mut target_order_state: Option<&mut ChunkOrderState> = None;
          let is_spot_order = self.state.active_spot_order.as_ref().map_or(false, |order| order.order_id == details.order_id);
          let is_futures_order = self.state.active_futures_order.as_ref().map_or(false, |order| order.order_id == details.order_id);
          if is_spot_order { target_order_state = self.state.active_spot_order.as_mut(); } else if is_futures_order { target_order_state = self.state.active_futures_order.as_mut(); } else { /*...*/ return Ok(()); }

          if let Some(order_state) = target_order_state {
              let old_filled_qty = order_state.filled_quantity; order_state.update_from_details(&details); let new_filled_qty = order_state.filled_quantity; let quantity_diff = new_filled_qty - old_filled_qty;
              let tolerance = dec!(1e-12);
              if quantity_diff.abs() > tolerance {
                    let price_for_diff = /* ... как в hedge_task ... */ self.get_current_price(if is_spot_order { Leg::Spot } else { Leg::Futures }).ok_or_else(|| anyhow!("Cannot determine price"))?;
                   let value_diff = quantity_diff * price_for_diff;
                  if is_spot_order { self.state.cumulative_spot_filled_quantity += quantity_diff; self.state.cumulative_spot_filled_value += value_diff.abs(); /* НЕ пересчитываем target_total_futures_value */ }
                  else if is_futures_order { self.state.cumulative_futures_filled_quantity += quantity_diff; self.state.cumulative_futures_filled_value += value_diff.abs(); }
                  self.send_progress_update().await?;
              }
              if matches!(/* ... order finished ... */) { /* ... убираем из активных ... */ }
          } Ok(())
     }
      async fn handle_market_data_update(&mut self, symbol: String, bids: Vec<OrderbookLevel>, asks: Vec<OrderbookLevel>, is_snapshot: bool) -> Result<()> {
           // Вызываем реализацию из hedge_task
           HedgerWsHedgeTask::handle_market_data_update(self, symbol, bids, asks, is_snapshot).await
      }
      async fn handle_public_trade_update(&mut self, symbol: String, price: f64, qty: f64, side: OrderSide, timestamp: i64) -> Result<()> {
            // Вызываем реализацию из hedge_task
           HedgerWsHedgeTask::handle_public_trade_update(self, symbol, price, qty, side, timestamp).await
      }
      async fn check_stale_orders(&mut self) -> Result<()> {
           // Вызываем реализацию из hedge_task (логика проверки та же, стороны учтены в calculate_limit_price_for_leg)
           HedgerWsHedgeTask::check_stale_orders(self).await
      }
      async fn initiate_order_replacement(&mut self, leg_to_cancel: Leg, reason: String) -> Result<()> {
            // Вызываем реализацию из hedge_task
           HedgerWsHedgeTask::initiate_order_replacement(self, leg_to_cancel, reason).await
      }
      async fn handle_cancel_confirmation(&mut self, cancelled_order_id: &str, leg: Leg) -> Result<()> {
           // Вызываем реализацию из hedge_task (логика перевыставления та же)
           HedgerWsHedgeTask::handle_cancel_confirmation(self, cancelled_order_id, leg).await
      }
       fn calculate_limit_price_for_leg(&self, leg: Leg) -> Result<Decimal> {
            // Вызываем реализацию из hedge_task (она уже учитывает сторону ордера Buy/Sell)
            HedgerWsHedgeTask::calculate_limit_price_for_leg(self, leg)
       }
       fn round_down_step(&self, value: Decimal, step: Decimal) -> Result<Decimal> {
             // Вызываем реализацию из hedge_task
             HedgerWsHedgeTask::round_down_step(self, value, step)
       }
       fn get_current_price(&self, leg: Leg) -> Option<Decimal> {
             // Вызываем реализацию из hedge_task
             HedgerWsHedgeTask::get_current_price(self, leg)
       }
        fn check_chunk_completion(&self) -> bool { HedgerWsHedgeTask::check_chunk_completion(self) }
        async fn send_progress_update(&mut self) -> Result<()> { Ok(()) } // Placeholder
        async fn update_final_db_status(&self) { HedgerWsHedgeTask::update_final_db_status(self).await }
         fn check_value_imbalance(&self) -> bool { HedgerWsHedgeTask::check_value_imbalance(self) }
         // Вспомогательные функции для работы с Decimal, если они не в common
          fn get_decimals_from_step(step: Option<&str>) -> Result<u32> { HedgerWsHedgeTask::get_decimals_from_step(step) }
          fn get_step_decimal(step: Option<&str>) -> Result<Decimal> { HedgerWsHedgeTask::get_step_decimal(step) }


} // --- Конец impl HedgerWsUnhedgeTask ---
