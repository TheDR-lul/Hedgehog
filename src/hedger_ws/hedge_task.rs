// src/hedger_ws/hedge_task.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};
// --- ДОБАВЛЕНО: недостающие импорты ---
use std::time::{Duration, SystemTime, UNIX_EPOCH};
// --- КОНЕЦ ДОБАВЛЕНИЙ ---
use tokio::time::sleep;
// Убрали HashMap, т.к. он не используется напрямую здесь
// use std::collections::HashMap;

use crate::config::{Config, WsLimitOrderPlacementStrategy}; // Убрали HedgeStrategy
use crate::exchange::{Exchange, bybit::SPOT_CATEGORY, bybit::LINEAR_CATEGORY}; // --- ДОБАВЛЕНО: LINEAR_CATEGORY ---
use crate::exchange::types::{WebSocketMessage, DetailedOrderStatus, OrderSide, OrderStatusText, OrderbookLevel}; // Убрали неиспользуемые InstrumentInfo, FeeRate
use crate::hedger::HedgeProgressCallback;
use crate::models::HedgeRequest;
use super::state::{HedgerWsState, HedgerWsStatus, OperationType, MarketUpdate, ChunkOrderState, Leg};
use crate::storage; // Для доступа к Db
// --- ДОБАВЛЕНО: Импорт из common ---
use crate::hedger_ws::common::{calculate_auto_chunk_parameters, round_up_step};
// --- КОНЕЦ ДОБАВЛЕНИЙ ---

// Структура для управления задачей хеджирования
pub struct HedgerWsHedgeTask {
    operation_id: i64,
    config: Arc<Config>,
    database: Arc<storage::Db>, // Доступ к БД
    state: HedgerWsState,
    ws_receiver: mpsc::Receiver<Result<WebSocketMessage>>,
    exchange_rest: Arc<dyn Exchange>,
    progress_callback: HedgeProgressCallback,
}

impl HedgerWsHedgeTask {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        operation_id: i64,
        request: HedgeRequest,
        config: Arc<Config>,
        database: Arc<storage::Db>, // Добавили Db
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

        // Параллельные запросы с tokio::join!
        let ((spot_info_res, linear_info_res), fee_rate_res, mmr_res, spot_price_res) = tokio::join!(
             tokio::join!(
                 exchange_rest.get_spot_instrument_info(&base_symbol),
                 exchange_rest.get_linear_instrument_info(&base_symbol)
            ),
            exchange_rest.get_fee_rate(&spot_symbol_name, SPOT_CATEGORY), // Используем spot_symbol_name
            exchange_rest.get_mmr(&futures_symbol_name), // Используем futures_symbol_name
            exchange_rest.get_spot_price(&base_symbol)
        );

        let spot_info = spot_info_res.context("Failed to get SPOT instrument info")?;
        let linear_info = linear_info_res.context("Failed to get LINEAR instrument info")?;
        let _fee_rate = fee_rate_res.context("Failed to get SPOT fee rate")?; // Пока не используется, но получаем
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
        let current_futures_price_estimate = current_spot_price; // Используем спот как оценку

        // Вызываем функцию из common
        let (final_chunk_count, chunk_spot_quantity, chunk_futures_quantity) =
            calculate_auto_chunk_parameters( // Используем импортированную функцию
                initial_target_spot_value, initial_target_futures_quantity,
                current_spot_price, current_futures_price_estimate,
                target_chunk_count, min_spot_quantity, min_futures_quantity,
                spot_quantity_step, futures_quantity_step,
                min_spot_notional, min_futures_notional,
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
        sleep(Duration::from_millis(500)).await; // Небольшая пауза после установки плеча

        // --- 4. Создание начального состояния ---
        let spot_tick_size = Decimal::from_str(&spot_info.price_filter.tick_size)?;
        let futures_tick_size = Decimal::from_str(&linear_info.price_filter.tick_size)?;

        let mut state = HedgerWsState::new_hedge(
            operation_id, spot_symbol_name.clone(), futures_symbol_name.clone(),
            initial_target_spot_value, initial_target_futures_quantity
        );
        // Сохраняем точные лимиты и параметры чанков
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
        state.status = HedgerWsStatus::StartingChunk(1); // Готов к запуску первого чанка

        info!(operation_id, "HedgerWsHedgeTask initialized successfully. Ready to run.");

        Ok(Self { operation_id, config, database, state, ws_receiver, exchange_rest, progress_callback })
    }

    // Основной цикл обработки сообщений и управления состоянием
    pub async fn run(&mut self) -> Result<()> {
        info!(operation_id = self.operation_id, "Starting HedgerWsHedgeTask run loop...");

        // --- Запуск первого чанка ---
        if matches!(self.state.status, HedgerWsStatus::StartingChunk(1)) {
             if let Err(error) = self.start_next_chunk().await {
                 error!(operation_id = self.operation_id, %error, "Failed to start initial chunk");
                 self.state.status = HedgerWsStatus::Failed(format!("Failed start chunk 1: {}", error));
                 self.update_final_db_status().await;
                 return Err(error);
             }
        } else if !matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) {
            warn!(operation_id = self.operation_id, status = ?self.state.status, "Task run started with unexpected status.");
            let error_message = format!("Task started in invalid state: {:?}", self.state.status);
            self.state.status = HedgerWsStatus::Failed(error_message.clone());
            self.update_final_db_status().await;
            return Err(anyhow!(error_message));
        }

        // --- Цикл обработки сообщений WS ---
        loop {
             tokio::select! {
                 // Чтение сообщения из канала WebSocket
                 maybe_result = self.ws_receiver.recv() => {
                      match maybe_result {
                         Some(Ok(message)) => {
                             // Обработка полученного сообщения
                             if let Err(error) = self.handle_websocket_message(message).await {
                                 error!(operation_id = self.operation_id, %error, "Error handling WebSocket message");
                                  self.state.status = HedgerWsStatus::Failed(format!("WS Handling Error: {}", error));
                                  self.update_final_db_status().await;
                                  return Err(error); // Выходим из цикла при ошибке обработки
                             }
                         }
                         Some(Err(error)) => {
                             // Ошибка при чтении из канала
                             error!(operation_id = self.operation_id, %error, "Error received from WebSocket channel. Stopping task.");
                             self.state.status = HedgerWsStatus::Failed(format!("WS channel error: {}", error));
                             self.update_final_db_status().await;
                             return Err(error); // Выходим из цикла
                         }
                         None => {
                             // Канал закрыт
                             info!(operation_id = self.operation_id, "WebSocket channel closed. Stopping task.");
                              // Проверяем, не завершилась ли задача штатно перед закрытием канала
                              if !matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                                  self.state.status = HedgerWsStatus::Failed("WebSocket channel closed unexpectedly".to_string());
                                  self.update_final_db_status().await;
                                  return Err(anyhow!("WebSocket channel closed unexpectedly")); // Возвращаем ошибку
                              }
                             break; // Выходим из цикла, если задача уже завершилась
                         }
                     }
                 }
                 // Небольшая пауза, чтобы не загружать CPU на 100%
                 _ = sleep(Duration::from_millis(100)) => {}
             } // конец select!

             // --- Логика после обработки сообщения или паузы ---

             // Проверка завершения текущего чанка
             if matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) && self.check_chunk_completion() {
                 let current_chunk = match self.state.status {
                     HedgerWsStatus::RunningChunk(index) => index,
                     _ => self.state.current_chunk_index.saturating_sub(1), // Последний завершенный
                 };
                 debug!(operation_id=self.operation_id, chunk_index=current_chunk, "Chunk completed processing.");

                 // Если это был не последний чанк
                 if current_chunk < self.state.total_chunks {
                     let next_chunk_index = current_chunk + 1;
                      // Проверяем дисбаланс стоимостей перед запуском следующего чанка
                      if self.check_value_imbalance() {
                           info!(operation_id=self.operation_id, chunk_index=current_chunk, "Significant value imbalance detected. Waiting for lagging leg.");
                           let leading = if self.state.cumulative_spot_filled_value > self.state.cumulative_futures_filled_value.abs() { Leg::Spot } else { Leg::Futures };
                           // Устанавливаем статус ожидания
                           self.state.status = HedgerWsStatus::WaitingImbalance { chunk_index: current_chunk, leading_leg: leading };
                      } else {
                           // Дисбаланса нет, запускаем следующий чанк
                           self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                           if let Err(error) = self.start_next_chunk().await {
                               error!(operation_id = self.operation_id, chunk = next_chunk_index, %error, "Failed to start next chunk");
                               self.state.status = HedgerWsStatus::Failed(format!("Failed start chunk {}: {}", next_chunk_index, error));
                               self.update_final_db_status().await;
                               return Err(error); // Выходим из цикла
                           }
                      }
                 } else {
                     // Все чанки обработаны, переходим к финальной сверке
                     info!(operation_id = self.operation_id, "All chunks processed, moving to Reconciling state.");
                     self.state.status = HedgerWsStatus::Reconciling;
                     if let Err(error) = self.reconcile().await {
                         error!(operation_id = self.operation_id, %error, "Reconciliation failed");
                         self.state.status = HedgerWsStatus::Failed(format!("Reconciliation failed: {}", error));
                         self.update_final_db_status().await;
                         return Err(error); // Выходим из цикла
                     }
                     // Реконсиляция завершена (успешно или нет - обработано внутри reconcile)
                     info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting run loop after reconciliation attempt.");
                     break; // Выходим из цикла после реконсиляции
                 }
             } else if matches!(self.state.status, HedgerWsStatus::WaitingImbalance { .. }) {
                  // Если были в статусе ожидания дисбаланса, проверяем снова
                  if !self.check_value_imbalance() {
                       info!(operation_id=self.operation_id, "Value imbalance resolved. Proceeding to next chunk.");
                        // Индекс следующего чанка берем из состояния
                        let next_chunk_index = self.state.current_chunk_index; // current_chunk_index уже инкрементирован
                        self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                         if let Err(error) = self.start_next_chunk().await {
                             error!(operation_id = self.operation_id, chunk = next_chunk_index, %error, "Failed to start chunk after imbalance resolved");
                             self.state.status = HedgerWsStatus::Failed(format!("Failed start chunk {}: {}", next_chunk_index, error));
                             self.update_final_db_status().await;
                             return Err(error); // Выходим из цикла
                         }
                  }
             }

             // Проверка статуса для выхода из цикла (завершение, отмена, ошибка)
             if matches!(self.state.status, HedgerWsStatus::Completed | HedgerWsStatus::Cancelled | HedgerWsStatus::Failed(_)) {
                 info!(operation_id = self.operation_id, status = ?self.state.status, "Exiting run loop due to final status.");
                 break; // Выходим из цикла
             }
        } // конец loop

        // Возвращаем результат в зависимости от финального статуса
        if self.state.status == HedgerWsStatus::Completed { Ok(()) }
        else { Err(anyhow!("Hedger task run loop exited with non-completed status: {:?}", self.state.status)) }
    }

    // Обработка входящего сообщения WebSocket
    async fn handle_websocket_message(&mut self, message: WebSocketMessage) -> Result<()> {
        match message {
            WebSocketMessage::OrderUpdate(details) => {
                // Проверяем, не ждем ли мы подтверждения отмены этого ордера
                if let HedgerWsStatus::WaitingCancelConfirmation { ref cancelled_order_id, cancelled_leg, .. } = self.state.status {
                     if details.order_id == *cancelled_order_id {
                         info!(operation_id=self.operation_id, order_id=%details.order_id, "Received update for the order being cancelled.");
                         // Обрабатываем обновление статуса (может быть Cancelled, PartiallyFilledCanceled, Filled и т.д.)
                         self.handle_order_update(details).await?;
                         // Запускаем обработку подтверждения отмены, если ордер действительно больше не активен
                         if matches!(self.state.status, HedgerWsStatus::WaitingCancelConfirmation { .. }) // Проверяем, что мы все еще ждем
                             && (self.state.active_spot_order.is_none() && cancelled_leg == Leg::Spot
                                 || self.state.active_futures_order.is_none() && cancelled_leg == Leg::Futures)
                         {
                              self.handle_cancel_confirmation(cancelled_order_id, cancelled_leg).await?;
                         }
                         return Ok(());
                     }
                }
                // Обычная обработка обновления ордера
                self.handle_order_update(details).await?;
            }
            WebSocketMessage::OrderBookL2 { symbol, bids, asks, is_snapshot } => {
                // Обработка обновления стакана
                self.handle_order_book_update(symbol, bids, asks, is_snapshot).await?;
                // Проверка "устаревших" ордеров после обновления рыночных данных
                self.check_stale_orders().await?;
            }
            WebSocketMessage::PublicTrade { symbol, price, qty, side, timestamp } => {
                 // Обработка публичных сделок (если нужно для стратегии)
                 self.handle_public_trade_update(symbol, price, qty, side, timestamp).await?;
            }
            WebSocketMessage::Error(error_message) => {
                // Логируем ошибку из WS потока
                warn!(operation_id = self.operation_id, %error_message, "Received error message from WebSocket stream");
            }
            WebSocketMessage::Disconnected => {
                 // Обрабатываем событие дисконнекта
                 error!(operation_id = self.operation_id, "WebSocket disconnected event received.");
                 return Err(anyhow!("WebSocket disconnected")); // Возвращаем ошибку, чтобы остановить run()
            }
            WebSocketMessage::Pong => { debug!(operation_id = self.operation_id, "Pong received"); } // Просто логируем Pong
            WebSocketMessage::Authenticated(success) => { info!(operation_id = self.operation_id, success, "WS Authenticated status received"); }
            WebSocketMessage::SubscriptionResponse { success, ref topic } => { info!(operation_id = self.operation_id, success, %topic, "WS Subscription response received"); }
            WebSocketMessage::Connected => { info!(operation_id = self.operation_id, "WS Connected event received (likely redundant)"); }
            // Игнорируем остальные типы сообщений
            // _ => { trace!(operation_id = self.operation_id, "Ignoring system message type in run loop"); }
        }
        Ok(())
    }

    // --- Логика перестановки ордеров ---

    // Проверка активных ордеров на "устаревание" цены
    async fn check_stale_orders(&mut self) -> Result<()> {
        // Проверяем только если мы в активной фазе выполнения чанка
        if !matches!(self.state.status, HedgerWsStatus::RunningChunk(_)) { return Ok(()); }

        // Получаем порог устаревания из конфига
        // !!! Убедитесь, что ws_stale_price_ratio добавлен в config.rs !!!
        if let Some(stale_ratio_f64) = self.config.ws_stale_price_ratio {
            if stale_ratio_f64 <= 0.0 { return Ok(()); } // Проверка отключена
            let stale_ratio_decimal = Decimal::try_from(stale_ratio_f64)?;

            let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;

            // Проверка ордера на СПОТ (Buy)
            let spot_order_opt = self.state.active_spot_order.clone(); // Клонируем, чтобы избежать borrow checker проблем
            if let Some(spot_order) = spot_order_opt {
                // Проверяем только активные лимитные ордера
                if matches!(spot_order.status, OrderStatusText::New | OrderStatusText::PartiallyFilled) {
                    // Получаем лучшие рыночные цены из состояния
                    if let (Some(best_ask), Some(last_update_time)) = (self.state.spot_market_data.best_ask_price, self.state.spot_market_data.last_update_time_ms) {
                        // Проверяем, что рыночные данные не слишком старые (например, < 5 секунд)
                         if now_ms - last_update_time < 5000 {
                            // Цена нашего ордера (Buy) должна быть НЕ ЗНАЧИТЕЛЬНО НИЖЕ лучшей цены Ask
                            if spot_order.limit_price < best_ask * (Decimal::ONE - stale_ratio_decimal) {
                                warn!(operation_id = self.operation_id, order_id = %spot_order.order_id, limit_price = %spot_order.limit_price, best_ask = %best_ask, "Spot order price is stale! Triggering replacement.");
                                // Запускаем процесс замены ордера
                                self.initiate_order_replacement(Leg::Spot, "StalePrice".to_string()).await?;
                                return Ok(()); // Выходим после инициации замены
                            }
                        } else { trace!(operation_id = self.operation_id, "Spot market data is too old for stale check ({:.1}s)", (now_ms - last_update_time) as f64 / 1000.0); }
                    }
                }
            }

            // Проверка ордера на ФЬЮЧЕРС (Sell)
            let futures_order_opt = self.state.active_futures_order.clone();
            if let Some(futures_order) = futures_order_opt {
                if matches!(futures_order.status, OrderStatusText::New | OrderStatusText::PartiallyFilled) {
                    if let (Some(best_bid), Some(last_update_time)) = (self.state.futures_market_data.best_bid_price, self.state.futures_market_data.last_update_time_ms) {
                         if now_ms - last_update_time < 5000 {
                             // Цена нашего ордера (Sell) должна быть НЕ ЗНАЧИТЕЛЬНО ВЫШЕ лучшей цены Bid
                             if futures_order.limit_price > best_bid * (Decimal::ONE + stale_ratio_decimal) {
                                 warn!(operation_id = self.operation_id, order_id = %futures_order.order_id, limit_price = %futures_order.limit_price, best_bid = %best_bid, "Futures order price is stale! Triggering replacement.");
                                 self.initiate_order_replacement(Leg::Futures, "StalePrice".to_string()).await?;
                                 return Ok(());
                             }
                         } else { trace!(operation_id = self.operation_id, "Futures market data is too old for stale check ({:.1}s)", (now_ms - last_update_time) as f64 / 1000.0); }
                    }
                }
            }
        }
        Ok(())
    }

    // Инициирование замены ордера (отправка команды cancel)
    async fn initiate_order_replacement(&mut self, leg_to_cancel: Leg, reason: String) -> Result<()> {
        let (order_to_cancel_opt, symbol, is_spot) = match leg_to_cancel {
            Leg::Spot => (self.state.active_spot_order.as_ref(), &self.state.symbol_spot, true),
            Leg::Futures => (self.state.active_futures_order.as_ref(), &self.state.symbol_futures, false),
        };

        if let Some(order_to_cancel) = order_to_cancel_opt {
             // Проверяем, не идет ли уже процесс отмены/ожидания
             if matches!(self.state.status, HedgerWsStatus::CancellingOrder{..} | HedgerWsStatus::WaitingCancelConfirmation{..}) {
                  warn!(operation_id=self.operation_id, order_id=%order_to_cancel.order_id, current_status=?self.state.status, "Replacement already in progress. Skipping.");
                  return Ok(());
             }
            info!(operation_id = self.operation_id, order_id = %order_to_cancel.order_id, ?leg_to_cancel, %reason, "Initiating order replacement: sending cancel request...");
            let current_chunk = match self.state.status { HedgerWsStatus::RunningChunk(idx) => idx, _ => self.state.current_chunk_index }; // Определяем текущий чанк
             let order_id_to_cancel_str = order_to_cancel.order_id.clone(); // Клонируем ID для передачи в статус

             // Устанавливаем статус "Отменяем ордер"
             self.state.status = HedgerWsStatus::CancellingOrder { chunk_index: current_chunk, leg_to_cancel, order_id_to_cancel: order_id_to_cancel_str.clone(), reason };

             // Отправляем запрос на отмену через REST
             let cancel_result = if is_spot { self.exchange_rest.cancel_spot_order(symbol, &order_id_to_cancel_str).await }
                               else { self.exchange_rest.cancel_futures_order(symbol, &order_id_to_cancel_str).await };

             if let Err(e) = cancel_result {
                  error!(operation_id = self.operation_id, order_id = %order_id_to_cancel_str, %e, "Failed to send cancel request for order replacement");
                  // Возвращаем статус обратно в RunningChunk, если отмена не удалась
                  self.state.status = HedgerWsStatus::RunningChunk(current_chunk);
                  return Err(e).context("Failed to send cancel request");
             } else {
                  info!(operation_id = self.operation_id, order_id = %order_id_to_cancel_str, "Cancel request sent. Waiting for WebSocket confirmation...");
                  // Устанавливаем статус "Ожидаем подтверждения отмены"
                  self.state.status = HedgerWsStatus::WaitingCancelConfirmation { chunk_index: current_chunk, cancelled_leg: leg_to_cancel, cancelled_order_id: order_id_to_cancel_str };
             }
        } else {
            warn!(operation_id = self.operation_id, ?leg_to_cancel, "Attempted to replace order, but no active order found for leg.");
        }
        Ok(())
    }

    // Обработка подтверждения отмены и выставление нового ордера
    async fn handle_cancel_confirmation(&mut self, cancelled_order_id: &str, leg: Leg) -> Result<()> {
         info!(operation_id = self.operation_id, %cancelled_order_id, ?leg, "Handling cancel confirmation: placing replacement order if needed...");

         // Определяем общий целевой объем, исполненное кол-во, мин. кол-во и шаг для данной ноги
         let (total_target_qty_approx, filled_qty, min_quantity, step, is_spot) = match leg {
              Leg::Spot => {
                  // Приблизительная общая цель по объему спота на основе последней известной цены
                  let price = self.get_current_price(Leg::Spot).unwrap_or(Decimal::ONE); // Fallback price
                  let target_approx = if price > Decimal::ZERO { self.state.initial_target_spot_value / price } else { Decimal::MAX }; // Используем initial_target_spot_value
                  (target_approx, self.state.cumulative_spot_filled_quantity, self.state.min_spot_quantity, self.state.spot_quantity_step, true)
              }
              Leg::Futures => (self.state.initial_target_futures_qty, self.state.cumulative_futures_filled_quantity, self.state.min_futures_quantity, self.state.futures_quantity_step, false),
         };

          // Рассчитываем остаток, который нужно исполнить
          let remaining_quantity = (total_target_qty_approx - filled_qty).max(Decimal::ZERO);
          // Округляем остаток ВНИЗ до шага инструмента
          let remaining_quantity_rounded = self.round_down_step(remaining_quantity, step)?;

          // Сбрасываем активный ордер для этой ноги в состоянии
          match leg {
               Leg::Spot => self.state.active_spot_order = None,
               Leg::Futures => self.state.active_futures_order = None,
          }
          info!(operation_id = self.operation_id, %cancelled_order_id, ?leg, "Cleared active order state after cancel confirmation.");

          // Проверяем, не является ли остаток "пылью" (меньше минимального ордера)
          let tolerance = dec!(1e-12);
          if remaining_quantity_rounded < min_quantity && remaining_quantity_rounded.abs() > tolerance {
                warn!(operation_id=self.operation_id, %remaining_quantity_rounded, %min_quantity, ?leg, "Remaining quantity after cancel is dust. Not placing replacement order.");
                // Ордер уже сброшен, просто выходим
          }
          // Проверяем, есть ли вообще остаток для исполнения
          else if remaining_quantity_rounded > tolerance {
              info!(operation_id = self.operation_id, %remaining_quantity_rounded, ?leg, "Placing replacement order...");
              // Рассчитываем новую лимитную цену
              let new_limit_price = self.calculate_limit_price_for_leg(leg)?;

               // Определяем параметры для выставления ордера
               let (symbol, side, qty_precision, price_precision) = match leg {
                   Leg::Spot => (&self.state.symbol_spot, OrderSide::Buy, step.scale(), self.state.spot_tick_size.scale()),
                   Leg::Futures => (&self.state.symbol_futures, OrderSide::Sell, step.scale(), self.state.futures_tick_size.scale()),
               };

               // Конвертируем в f64 с нужным округлением
               let quantity_f64 = remaining_quantity_rounded.round_dp(qty_precision).to_f64().unwrap_or(0.0);
               let price_f64 = new_limit_price.round_dp(price_precision).to_f64().unwrap_or(0.0);

               // Проверка на валидность перед отправкой
               if quantity_f64 <= 0.0 || price_f64 <= 0.0 {
                    error!(operation_id=self.operation_id, %quantity_f64, %price_f64, ?leg, "Invalid parameters for replacement order!");
                     // Возвращаем статус в RunningChunk, т.к. замена не удалась
                     let current_chunk = match self.state.status { HedgerWsStatus::WaitingCancelConfirmation { chunk_index, .. } => chunk_index, _ => self.state.current_chunk_index };
                     self.state.status = HedgerWsStatus::RunningChunk(current_chunk);
                     return Err(anyhow!("Invalid parameters for replacement order"));
               }


                // Выставляем новый ордер через REST
                let place_result = if is_spot { self.exchange_rest.place_limit_order(symbol, side, quantity_f64, price_f64).await }
                                  else { self.exchange_rest.place_futures_limit_order(symbol, side, quantity_f64, price_f64).await };

                match place_result {
                     Ok(new_order) => {
                         info!(operation_id=self.operation_id, new_order_id=%new_order.id, ?leg, "Replacement order placed successfully.");
                         // Создаем новое состояние для ордера
                         let new_order_state = ChunkOrderState::new(new_order.id, symbol.to_string(), side, new_limit_price, remaining_quantity_rounded);
                         // Сохраняем его как активный ордер для данной ноги
                         match leg {
                              Leg::Spot => self.state.active_spot_order = Some(new_order_state),
                              Leg::Futures => self.state.active_futures_order = Some(new_order_state)
                         }
                     }
                     Err(e) => {
                          error!(operation_id = self.operation_id, %e, ?leg, "Failed to place replacement order!");
                          // Ошибка выставления - переводим задачу в Failed
                          self.state.status = HedgerWsStatus::Failed(format!("Failed place replacement {:?} order: {}", leg, e));
                          self.update_final_db_status().await;
                          return Err(e).context("Failed to place replacement order");
                     }
                }
          } else {
              info!(operation_id=self.operation_id, ?leg, "No remaining quantity after cancel confirmation. Not placing replacement.");
              // Ордер уже сброшен, выходим
          }

           // Возвращаем статус в RunningChunk после успешной обработки отмены (даже если новый ордер не выставлен)
           let current_chunk = match self.state.status { HedgerWsStatus::WaitingCancelConfirmation { chunk_index, .. } => chunk_index, _ => self.state.current_chunk_index };
           self.state.status = HedgerWsStatus::RunningChunk(current_chunk);
         Ok(())
    }

    // --- Реализация методов из предыдущего шага ---

    // Функция для начала обработки следующего чанка (Hedge)
    async fn start_next_chunk(&mut self) -> Result<()> {
        let chunk_index = match self.state.status {
             HedgerWsStatus::StartingChunk(index) => index,
             HedgerWsStatus::RunningChunk(index) => {
                warn!(operation_id = self.operation_id, chunk_index=index, "start_next_chunk called while chunk already running.");
                return Ok(());
            }
            _ => return Err(anyhow!("start_next_chunk (hedge) called in unexpected state: {:?}", self.state.status)),
        };
        info!(operation_id = self.operation_id, chunk_index, "Starting hedge chunk placement...");
        self.state.active_spot_order = None;
        self.state.active_futures_order = None;

        let spot_quantity_chunk: Decimal;
        let futures_quantity_chunk: Decimal;
        let is_last_chunk = chunk_index == self.state.total_chunks;

        let current_spot_price_estimate = self.get_current_price(Leg::Spot)
            .ok_or_else(|| anyhow!("Cannot get current spot price estimate for chunk calculation"))?;

        let current_total_spot_target_qty = if current_spot_price_estimate > Decimal::ZERO {
            self.state.initial_target_spot_value / current_spot_price_estimate
        } else {
            warn!(operation_id = self.operation_id, "Current spot price estimate is zero, cannot calculate spot target quantity.");
            return Err(anyhow!("Current spot price estimate is zero"));
        };

        if is_last_chunk {
            spot_quantity_chunk = (current_total_spot_target_qty - self.state.cumulative_spot_filled_quantity).max(Decimal::ZERO);
            futures_quantity_chunk = (self.state.initial_target_futures_qty - self.state.cumulative_futures_filled_quantity).max(Decimal::ZERO);
            info!(operation_id=self.operation_id, chunk_index, "Calculating quantities for last hedge chunk.");
        } else {
            spot_quantity_chunk = self.state.chunk_base_quantity_spot;
            futures_quantity_chunk = self.state.chunk_base_quantity_futures;
        }
        debug!(operation_id=self.operation_id, chunk_index, %spot_quantity_chunk, %futures_quantity_chunk, "Calculated hedge chunk quantities.");

        let spot_quantity_rounded = self.round_down_step(spot_quantity_chunk, self.state.spot_quantity_step)?;
        let futures_quantity_rounded = self.round_down_step(futures_quantity_chunk, self.state.futures_quantity_step)?;

        let tolerance = dec!(1e-12);
        let place_spot = spot_quantity_rounded >= self.state.min_spot_quantity || spot_quantity_rounded < tolerance;
        let place_futures = futures_quantity_rounded >= self.state.min_futures_quantity || futures_quantity_rounded < tolerance;

        let spot_notional_ok = self.state.min_spot_notional.map_or(true, |min_val| {
             (spot_quantity_rounded * current_spot_price_estimate) >= min_val || spot_quantity_rounded < tolerance
        });
        let futures_price_estimate = self.get_current_price(Leg::Futures).unwrap_or(current_spot_price_estimate);
        let futures_notional_ok = self.state.min_futures_notional.map_or(true, |min_val| {
             (futures_quantity_rounded * futures_price_estimate) >= min_val || futures_quantity_rounded < tolerance
        });

        if (!place_spot || !spot_notional_ok) && (!place_futures || !futures_notional_ok) {
            warn!(operation_id=self.operation_id, chunk_index, %spot_quantity_rounded, %futures_quantity_rounded,
                  place_spot, spot_notional_ok, place_futures, futures_notional_ok,
                  "Both legs quantities for chunk are below minimums or notionals. Skipping chunk.");
             if is_last_chunk {
                info!(operation_id=self.operation_id, chunk_index, "Skipping last chunk due to minimums, moving to reconcile.");
                self.state.status = HedgerWsStatus::Reconciling;
                self.reconcile().await?;
             } else {
                 let next_chunk_index = chunk_index + 1;
                 self.state.current_chunk_index = next_chunk_index;
                 self.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
                 self.start_next_chunk().await?;
             }
             return Ok(());
        }
        if !place_spot || !spot_notional_ok { warn!(operation_id=self.operation_id, chunk_index, %spot_quantity_rounded, "Spot leg quantity too small or below notional. Skipping spot placement."); }
        if !place_futures || !futures_notional_ok { warn!(operation_id=self.operation_id, chunk_index, %futures_quantity_rounded, "Futures leg quantity too small or below notional. Skipping futures placement."); }


        let spot_limit_price = if place_spot && spot_notional_ok { Some(self.calculate_limit_price_for_leg(Leg::Spot)?) } else { None };
        let futures_limit_price = if place_futures && futures_notional_ok { Some(self.calculate_limit_price_for_leg(Leg::Futures)?) } else { None };

        let mut placed_spot_order: Option<ChunkOrderState> = None;
        let mut placed_futures_order: Option<ChunkOrderState> = None;
        let mut spot_place_error: Option<anyhow::Error> = None;
        let mut futures_place_error: Option<anyhow::Error> = None;

        if let (true, true, Some(limit_price)) = (place_spot, spot_notional_ok, spot_limit_price) {
            self.state.status = HedgerWsStatus::PlacingSpotOrder(chunk_index);
            let qty_f64 = spot_quantity_rounded.round_dp(self.state.spot_quantity_step.scale()).to_f64().unwrap_or(0.0);
            let price_f64 = limit_price.round_dp(self.state.spot_tick_size.scale()).to_f64().unwrap_or(0.0);

            if qty_f64 > 0.0 && price_f64 > 0.0 {
                info!(operation_id=self.operation_id, chunk_index, %qty_f64, %price_f64, "Placing SPOT BUY order via REST");
                let spot_order_result = self.exchange_rest.place_limit_order(
                    &self.state.symbol_spot, OrderSide::Buy, qty_f64, price_f64
                ).await;

                match spot_order_result {
                    Ok(order) => {
                        info!(operation_id=self.operation_id, chunk_index, order_id=%order.id, "SPOT BUY order placed.");
                        placed_spot_order = Some(ChunkOrderState::new(order.id, self.state.symbol_spot.clone(), OrderSide::Buy, limit_price, spot_quantity_rounded));
                    }
                    Err(e) => {
                        error!(operation_id=self.operation_id, chunk_index, %e, "Failed to place SPOT BUY order!");
                        spot_place_error = Some(e.context("Failed to place SPOT BUY order"));
                    }
                };
            } else {
                spot_place_error = Some(anyhow!("Invalid SPOT order parameters (qty={}, price={})", qty_f64, price_f64));
            }
        }

        if let (true, true, Some(limit_price)) = (place_futures, futures_notional_ok, futures_limit_price) {
             if spot_place_error.is_none() {
                self.state.status = HedgerWsStatus::PlacingFuturesOrder(chunk_index);
                let qty_f64 = futures_quantity_rounded.round_dp(self.state.futures_quantity_step.scale()).to_f64().unwrap_or(0.0);
                let price_f64 = limit_price.round_dp(self.state.futures_tick_size.scale()).to_f64().unwrap_or(0.0);

                if qty_f64 > 0.0 && price_f64 > 0.0 {
                    info!(operation_id=self.operation_id, chunk_index, %qty_f64, %price_f64, "Placing FUTURES SELL order via REST");
                    let futures_order_result = self.exchange_rest.place_futures_limit_order(
                        &self.state.symbol_futures, OrderSide::Sell, qty_f64, price_f64
                    ).await;

                    match futures_order_result {
                        Ok(order) => {
                             info!(operation_id=self.operation_id, chunk_index, order_id=%order.id, "FUTURES SELL order placed.");
                             placed_futures_order = Some(ChunkOrderState::new(order.id, self.state.symbol_futures.clone(), OrderSide::Sell, limit_price, futures_quantity_rounded));
                        }
                        Err(e) => {
                            error!(operation_id=self.operation_id, chunk_index, %e, "Failed to place FUTURES SELL order!");
                            futures_place_error = Some(e.context("Failed to place FUTURES SELL order"));
                            if let Some(ref spot_order_state) = placed_spot_order {
                                warn!(operation_id=self.operation_id, chunk_index, order_id=%spot_order_state.order_id, "Attempting to cancel SPOT order due to FUTURES placement failure.");
                                if let Err(cancel_err) = self.exchange_rest.cancel_spot_order(&self.state.symbol_spot, &spot_order_state.order_id).await {
                                     error!(operation_id=self.operation_id, chunk_index, order_id=%spot_order_state.order_id, %cancel_err, "Failed to cancel SPOT order after FUTURES failure!");
                                     futures_place_error = futures_place_error.map(|err| err.context(format!("Also failed to cancel spot: {}", cancel_err)));
                                } else {
                                     placed_spot_order = None;
                                     info!(operation_id=self.operation_id, chunk_index, order_id=%spot_order_state.order_id, "SPOT order cancelled successfully after FUTURES failure.");
                                }
                            }
                        }
                    };
                } else {
                    futures_place_error = Some(anyhow!("Invalid FUTURES order parameters (qty={}, price={})", qty_f64, price_f64));
                    if let Some(ref spot_order_state) = placed_spot_order {
                        warn!(operation_id=self.operation_id, chunk_index, order_id=%spot_order_state.order_id, "Attempting to cancel SPOT order due to FUTURES invalid params.");
                         if let Err(cancel_err) = self.exchange_rest.cancel_spot_order(&self.state.symbol_spot, &spot_order_state.order_id).await { /* ... */ }
                         else { placed_spot_order = None; }
                    }
                }
            } else {
                 warn!(operation_id=self.operation_id, chunk_index, "Skipping FUTURES placement because SPOT placement failed.");
            }
        }

        if let Some(error) = spot_place_error.or(futures_place_error) {
             self.state.status = HedgerWsStatus::Failed(format!("Chunk {} placement error: {}", chunk_index, error));
             self.update_final_db_status().await;
             return Err(error);
        }

        self.state.active_spot_order = placed_spot_order;
        self.state.active_futures_order = placed_futures_order;
        if self.state.active_spot_order.is_some() || self.state.active_futures_order.is_some() {
             self.state.current_chunk_index = chunk_index + 1;
        } else {
             warn!(operation_id=self.operation_id, chunk_index, "No orders were placed for this chunk (both skipped or failed?).");
             if is_last_chunk {
                self.state.status = HedgerWsStatus::Reconciling;
                self.reconcile().await?;
                return Ok(());
             }
             if matches!(self.state.status, HedgerWsStatus::StartingChunk(_)) {
                 self.start_next_chunk().await?;
                 return Ok(());
             }
        }

        self.state.status = HedgerWsStatus::RunningChunk(chunk_index);
        info!(operation_id = self.operation_id, chunk_index, "Hedge chunk placement finished. Status: {:?}. Active Spot: {:?}, Active Futures: {:?}",
            self.state.status,
            self.state.active_spot_order.as_ref().map(|o| &o.order_id),
            self.state.active_futures_order.as_ref().map(|o| &o.order_id)
        );

        self.send_progress_update().await?;
        Ok(())
    }

     // Функция для финальной стадии (Hedge)
     async fn reconcile(&mut self) -> Result<()> {
         info!(operation_id = self.operation_id, "Starting final hedge reconciliation...");
         self.state.status = HedgerWsStatus::Reconciling;

         let final_spot_qty = self.state.cumulative_spot_filled_quantity;
         let final_futures_qty = self.state.cumulative_futures_filled_quantity;
         let final_spot_value = self.state.cumulative_spot_filled_value;

          let target_futures_value = final_spot_value;

          let current_futures_price = self.get_current_price(Leg::Futures)
              .ok_or_else(|| anyhow!("Cannot get current futures price for reconciliation"))?;
          let actual_futures_value = final_futures_qty * current_futures_price;

          let value_imbalance = target_futures_value - actual_futures_value.abs();

          info!(operation_id = self.operation_id,
                %final_spot_value, target_futures_value = %target_futures_value,
                %final_futures_qty, %current_futures_price, actual_futures_value = %actual_futures_value.abs(),
                %value_imbalance, "Calculated final value imbalance for reconciliation.");

          let tolerance_value = self.state.initial_target_spot_value * dec!(0.001);

          if value_imbalance.abs() > tolerance_value {
              let adjustment_qty_decimal = value_imbalance / current_futures_price;
               let adjustment_qty_rounded = self.round_down_step(adjustment_qty_decimal.abs(), self.state.futures_quantity_step)?;
               let adjustment_qty = adjustment_qty_rounded.to_f64().unwrap_or(0.0);

               if adjustment_qty_rounded >= self.state.min_futures_quantity {
                    let side = if value_imbalance > Decimal::ZERO { OrderSide::Sell } else { OrderSide::Buy };
                    info!(operation_id=self.operation_id, ?side, adjustment_qty, %current_futures_price, "Placing FUTURES market order for reconciliation...");

                    match self.exchange_rest.place_futures_market_order(&self.state.symbol_futures, side, adjustment_qty).await {
                         Ok(order) => {
                              info!(operation_id=self.operation_id, order_id=%order.id, ?side, adjustment_qty, "Reconciliation market order placed successfully.");
                              sleep(Duration::from_secs(2)).await;
                         }
                         Err(e) => {
                              error!(operation_id=self.operation_id, %e, ?side, adjustment_qty, "Failed to place FUTURES market order for reconciliation!");
                         }
                    }
               } else {
                    warn!(operation_id=self.operation_id, %adjustment_qty_decimal, %adjustment_qty_rounded, min_qty=%self.state.min_futures_quantity,
                          "Required futures adjustment quantity is below minimum. Skipping reconciliation order.");
               }
          } else {
                info!(operation_id = self.operation_id, "Value imbalance within tolerance. No reconciliation needed.");
          }

         self.state.status = HedgerWsStatus::Completed;
         self.update_final_db_status().await;
         info!(operation_id = self.operation_id, "Hedge reconciliation complete. Final Status: Completed.");
         Ok(())
     }

    // --- Остальные методы (не изменены по сравнению с предыдущим ответом) ---
    // Обработка обновления ордера
    async fn handle_order_update(&mut self, details: DetailedOrderStatus) -> Result<()> {
        info!(operation_id = self.operation_id, order_id = %details.order_id, status = ?details.status_text, filled_qty = details.filled_qty, "Handling order update");

        let mut target_order_state: Option<&mut ChunkOrderState> = None;
        let leg_option: Option<Leg>; // Определяем ногу

        if let Some(spot_order) = self.state.active_spot_order.as_mut() {
            if spot_order.order_id == details.order_id {
                target_order_state = Some(spot_order);
                leg_option = Some(Leg::Spot);
            } else { leg_option = None; } // Не этот ордер, проверяем следующий
        } else { leg_option = None; }

        if target_order_state.is_none() {
            if let Some(futures_order) = self.state.active_futures_order.as_mut() {
                 if futures_order.order_id == details.order_id {
                    target_order_state = Some(futures_order);
                    leg_option = Some(Leg::Futures);
                 }
            }
        }

        let leg = match leg_option {
            Some(l) => l,
            None => {
                 warn!(operation_id = self.operation_id, order_id = %details.order_id, "Received update for an unknown or inactive order.");
                 return Ok(());
            }
        };

        if let Some(order_state) = target_order_state {
            let old_filled_qty = order_state.filled_quantity;
            let old_status = order_state.status.clone();
            order_state.update_from_details(&details); // Обновляем состояние ордера
            let new_filled_qty = order_state.filled_quantity;
            let quantity_diff = new_filled_qty - old_filled_qty;

            let tolerance = dec!(1e-12);
            if quantity_diff.abs() > tolerance {
                // Рассчитываем изменение стоимости
                let price_for_diff = if order_state.average_price > Decimal::ZERO {
                    order_state.average_price // Используем среднюю цену ордера, если она есть
                } else if let Some(last_price_f64) = details.last_filled_price {
                     // Используем цену последнего исполнения, если есть
                     Decimal::try_from(last_price_f64)?
                } else {
                     // В крайнем случае берем текущую рыночную цену
                    self.get_current_price(leg).ok_or_else(|| anyhow!("Cannot determine price for value calc for order {}", details.order_id))?
                };
                let value_diff = quantity_diff * price_for_diff;

                // Обновляем кумулятивные значения
                match leg {
                    Leg::Spot => {
                        self.state.cumulative_spot_filled_quantity += quantity_diff;
                        self.state.cumulative_spot_filled_value += value_diff;
                        // --- Динамический пересчет цели фьючерса ---
                        self.state.target_total_futures_value = self.state.cumulative_spot_filled_value;
                        debug!(operation_id = self.operation_id, new_target_fut_value = %self.state.target_total_futures_value, "Updated target futures value based on spot fill.");
                    }
                    Leg::Futures => {
                        self.state.cumulative_futures_filled_quantity += quantity_diff;
                        // Стоимость фьючерса всегда положительна в нашем учете (abs)
                        self.state.cumulative_futures_filled_value += value_diff.abs();
                    }
                }
                // Отправляем обновление прогресса
                self.send_progress_update().await?;
            }

            // Проверяем, завершился ли ордер
             if old_status != order_state.status && // Только если статус изменился
                matches!(order_state.status, OrderStatusText::Filled | OrderStatusText::Cancelled | OrderStatusText::PartiallyFilledCanceled | OrderStatusText::Rejected)
             {
                info!(operation_id=self.operation_id, order_id=%order_state.order_id, final_status=?order_state.status, "Order reached final state.");
                // Убираем ордер из активных
                match leg {
                    Leg::Spot => self.state.active_spot_order = None,
                    Leg::Futures => self.state.active_futures_order = None,
                }
            }
        }
        Ok(())
    }

    // Обработка обновления стакана
    async fn handle_order_book_update(&mut self, symbol: String, bids: Vec<OrderbookLevel>, asks: Vec<OrderbookLevel>, _is_snapshot: bool) -> Result<()> {
        // Определяем, для спота или фьючерса пришло обновление
        let market_data_ref = if symbol == self.state.symbol_spot { &mut self.state.spot_market_data }
                              else if symbol == self.state.symbol_futures { &mut self.state.futures_market_data }
                              else { return Ok(()); }; // Игнорируем не наши символы

        // Обновляем лучшие цены и объемы
        market_data_ref.best_bid_price = bids.first().map(|l| l.price);
        market_data_ref.best_bid_quantity = bids.first().map(|l| l.quantity);
        market_data_ref.best_ask_price = asks.first().map(|l| l.price);
        market_data_ref.best_ask_quantity = asks.first().map(|l| l.quantity);
        // Записываем время обновления
        market_data_ref.last_update_time_ms = Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64);
        // trace!(operation_id = self.operation_id, %symbol, ?market_data_ref.best_bid_price, ?market_data_ref.best_ask_price, "Order book updated");
        Ok(())
    }

    // Обработка публичных сделок (пока заглушка)
    async fn handle_public_trade_update(&mut self, symbol: String, price: f64, qty: f64, side: OrderSide, timestamp: i64) -> Result<()> {
        trace!(operation_id = self.operation_id, %symbol, %price, %qty, ?side, %timestamp, "Received public trade (currently ignored)");
        // TODO: Возможно, использовать для более точного определения рыночной цены или ликвидности?
        Ok(())
    }

    // Расчет лимитной цены для ноги на основе стратегии и рыночных данных
    fn calculate_limit_price_for_leg(&self, leg: Leg) -> Result<Decimal> {
        let (market_data, side, tick_size) = match leg {
            Leg::Spot => (&self.state.spot_market_data, OrderSide::Buy, self.state.spot_tick_size),
            Leg::Futures => (&self.state.futures_market_data, OrderSide::Sell, self.state.futures_tick_size),
        };

        // Получаем лучшую цену для противоположной стороны (Buy -> Ask, Sell -> Bid)
        let reference_price = match side {
            OrderSide::Buy => market_data.best_ask_price.ok_or_else(|| anyhow!("No best ask price available for {:?}", leg)),
            OrderSide::Sell => market_data.best_bid_price.ok_or_else(|| anyhow!("No best bid price available for {:?}", leg)),
        }?;

        // Применяем стратегию выставления цены
         match self.config.ws_limit_order_placement_strategy {
            // Цена = лучшая цена противоположной стороны
            WsLimitOrderPlacementStrategy::BestAskBid => Ok(reference_price),
            // Цена на один тик "внутрь" стакана от лучшей цены
            WsLimitOrderPlacementStrategy::OneTickInside => {
                match side {
                    // Buy: цена чуть ниже лучшего Ask
                    OrderSide::Buy => Ok((reference_price - tick_size).max(tick_size)), // Убедимся, что цена не <= 0
                    // Sell: цена чуть выше лучшего Bid
                    OrderSide::Sell => Ok(reference_price + tick_size),
                }
            }
        }
    }

    // Округление ВНИЗ до указанного шага
    fn round_down_step(&self, value: Decimal, step: Decimal) -> Result<Decimal> {
        if step <= Decimal::ZERO {
            if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
            warn!("Rounding step zero or negative: {}. Returning original value.", step);
            return Ok(value.normalize());
        }
        if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
        let positive_step = step.abs();
        // Добавляем небольшой запас точности при делении
        let precision = positive_step.scale() + 3;
        let value_scaled = value.round_dp(precision);
        let step_scaled = positive_step.round_dp(precision);
        // Floor деления, затем умножаем на шаг
        Ok(((value_scaled / step_scaled).floor() * step_scaled).normalize())
    }

    // Получение текущей средней цены (mid-price) для ноги
    fn get_current_price(&self, leg: Leg) -> Option<Decimal> {
        let market_data = match leg {
            Leg::Spot => &self.state.spot_market_data,
            Leg::Futures => &self.state.futures_market_data,
        };
        // Рассчитываем среднюю цену между лучшим бидом и аском
        match (market_data.best_bid_price, market_data.best_ask_price) {
            (Some(bid), Some(ask)) => Some((bid + ask) / dec!(2.0)),
            (Some(bid), None) => Some(bid), // Если есть только бид
            (None, Some(ask)) => Some(ask), // Если есть только аск
            (None, None) => None,          // Если нет ни бида, ни аска
        }
    }

    // Проверка, завершен ли текущий чанк (оба ордера неактивны)
    fn check_chunk_completion(&self) -> bool {
        self.state.active_spot_order.is_none() && self.state.active_futures_order.is_none()
    }

    // Отправка колбэка прогресса (заглушка, требует реализации)
    async fn send_progress_update(&mut self) -> Result<()> {
        // TODO: Сформировать HedgeProgressUpdate и вызвать self.progress_callback
        trace!(operation_id = self.operation_id, "Progress update callback not implemented yet.");
        Ok(())
    }

    // Обновление финального статуса операции в БД
    async fn update_final_db_status(&self) {
        let status_str = match &self.state.status {
            HedgerWsStatus::Completed => "Completed",
            HedgerWsStatus::Cancelled => "Cancelled",
            HedgerWsStatus::Failed(_) => "Failed",
            _ => {
                warn!(operation_id = self.operation_id, status = ?self.state.status, "update_final_db_status called with non-final status.");
                return; // Не обновляем, если статус не финальный
            }
        };
        let error_message = match &self.state.status {
            HedgerWsStatus::Failed(msg) => Some(msg.as_str()),
            _ => None,
        };

        // Получаем финальные данные из состояния
        let spot_qty_f64 = self.state.cumulative_spot_filled_quantity.to_f64().unwrap_or(0.0);
        let fut_qty_f64 = self.state.cumulative_futures_filled_quantity.to_f64().unwrap_or(0.0);

        // Пытаемся получить ID последних активных ордеров (если они были)
        // В ТЗ не указано, нужно ли сохранять ID последнего ордера при ошибке/отмене,
        // поэтому пока передаем None. Если нужно, надо будет хранить last_placed_id.
        let last_futures_order_id: Option<&str> = None; // Заглушка

        // !!! ВАЖНО: Сигнатура update_hedge_final_status в storage/db.rs должна соответствовать!
        //             Текущая версия в db.rs может отличаться. Нужно синхронизировать.
        if let Err(e) = storage::update_hedge_final_status(
            &self.database,
            self.operation_id,
            status_str,
            last_futures_order_id, // Передаем ID фьючерсного ордера (или None)
            fut_qty_f64, // Передаем исполненное кол-во фьючерса
            error_message,
        ).await {
            error!(operation_id = self.operation_id, %e, "Failed to update final status in DB!");
        } else {
            info!(operation_id = self.operation_id, %status_str, "Final status updated in DB.");
        }
    }

    // Вспомогательные функции для получения точности/шага из строк
    fn get_decimals_from_step(step: Option<&str>) -> Result<u32> {
        let step_str = step.ok_or_else(|| anyhow!("Missing qtyStep/basePrecision"))?;
        Ok(step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32)
    }
    fn get_step_decimal(step: Option<&str>) -> Result<Decimal> {
        let step_str = step.ok_or_else(|| anyhow!("Missing qtyStep/basePrecision"))?;
        Decimal::from_str(step_str).context("Failed to parse step string to Decimal")
    }

     // Проверка дисбаланса стоимостей
     fn check_value_imbalance(&self) -> bool {
        if let Some(ratio) = self.config.ws_max_value_imbalance_ratio {
             if ratio <= 0.0 { return false; } // Проверка отключена
             // Используем ОБЩУЮ цель по стоимости спота как базу для сравнения
             let total_value_base = self.state.initial_target_spot_value;
             if total_value_base <= Decimal::ZERO { return false; } // Не можем рассчитать отношение
             // Дисбаланс = разница между стоимостью исполненного спота и модуля стоимости фьюча
             let imbalance = (self.state.cumulative_spot_filled_value - self.state.cumulative_futures_filled_value.abs()).abs();
             let current_ratio = imbalance / total_value_base;
             let threshold = Decimal::try_from(ratio).unwrap_or_default();
             trace!(operation_id=%self.state.operation_id, %imbalance, %current_ratio, %threshold, "Checking value imbalance");
             current_ratio > threshold // Возвращаем true, если дисбаланс превышает порог
        } else {
             false // Проверка отключена, если ratio не задан
        }
     }

} // --- Конец impl HedgerWsHedgeTask ---