// src/hedger.rs

use anyhow::{anyhow, Result};
use tracing::{debug, error, info, warn};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use futures::future::BoxFuture;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

use crate::exchange::{Exchange, OrderStatus, FeeRate};
use crate::exchange::bybit::{SPOT_CATEGORY, LINEAR_CATEGORY};
use crate::exchange::types::OrderSide;
use crate::models::{HedgeRequest, UnhedgeRequest};

pub const ORDER_FILL_TOLERANCE: f64 = 1e-8;

#[derive(Clone)]
pub struct Hedger<E> {
    exchange:     E,
    slippage:     f64,
    max_wait:     Duration,
    quote_currency: String,
}

#[derive(Debug)]
pub struct HedgeParams {
    pub spot_order_qty: f64,
    pub fut_order_qty: f64,
    pub current_spot_price: f64,
    pub initial_limit_price: f64,
    pub symbol: String,
}

#[derive(Debug, Clone)]
pub struct HedgeProgressUpdate {
    pub current_spot_price: f64,
    pub new_limit_price: f64,
    pub is_replacement: bool,
}

pub type HedgeProgressCallback = Box<dyn FnMut(HedgeProgressUpdate) -> BoxFuture<'static, Result<()>> + Send + Sync>;


impl<E> Hedger<E>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    pub fn new(exchange: E, slippage: f64, max_wait_secs: u64, quote_currency: String) -> Self {
        Self {
            exchange,
            slippage,
            max_wait: Duration::from_secs(max_wait_secs),
            quote_currency,
        }
    }

    pub async fn calculate_hedge_params(&self, req: &HedgeRequest) -> Result<HedgeParams> {
        let HedgeRequest { sum, symbol, volatility } = req;
        debug!(
            "Calculating hedge params for {} with sum={}, volatility={}",
            symbol, sum, volatility
        );

        match self.exchange.get_fee_rate(symbol, SPOT_CATEGORY).await {
            Ok(fee) => info!(symbol, category=SPOT_CATEGORY, maker_fee=fee.maker, taker_fee=fee.taker, "Current spot fee rate"),
            Err(e) => warn!("Could not get spot fee rate for {}: {}", symbol, e),
        }
        match self.exchange.get_fee_rate(symbol, LINEAR_CATEGORY).await {
             Ok(fee) => info!(symbol, category=LINEAR_CATEGORY, maker_fee=fee.maker, taker_fee=fee.taker, "Current futures fee rate"),
             Err(e) => warn!("Could not get futures fee rate for {}: {}", symbol, e),
        }

        let mmr = self.exchange.get_mmr(symbol).await?;
        let spot_value = sum / ((1.0 + volatility) * (1.0 + mmr));
        let fut_value  = sum - spot_value;
        debug!(
            "Calculated values: spot_value={}, fut_value={}, MMR={}",
            spot_value, fut_value, mmr
        );

        if spot_value <= 0.0 || fut_value <= 0.0 {
             error!("Calculated values are non-positive: spot={}, fut={}", spot_value, fut_value);
             return Err(anyhow::anyhow!("Calculated values are non-positive"));
        }

        let current_spot_price = self.exchange.get_spot_price(symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }
        let spot_order_qty = spot_value / current_spot_price;
        let fut_order_qty = fut_value / current_spot_price;
        let initial_limit_price = current_spot_price * (1.0 - self.slippage);
        debug!(
            "Calculated quantities based on price {}: spot_order_qty={}, fut_order_qty={}",
            current_spot_price, spot_order_qty, fut_order_qty
        );
        debug!("Initial limit price: {}", initial_limit_price);


        if spot_order_qty <= 0.0 || fut_order_qty <= 0.0 {
             error!("Calculated order quantities are non-positive.");
             return Err(anyhow::anyhow!("Calculated order quantities are non-positive"));
        }

        Ok(HedgeParams {
            spot_order_qty,
            fut_order_qty,
            current_spot_price,
            initial_limit_price,
            symbol: symbol.clone(),
        })
    }


    pub async fn run_hedge(
        &self,
        params: HedgeParams,
        mut progress_callback: HedgeProgressCallback,
        current_order_id_storage: Arc<TokioMutex<Option<String>>>,
        total_filled_qty_storage: Arc<TokioMutex<f64>>,
    ) -> Result<(f64, f64)>
    {
        let HedgeParams {
            spot_order_qty: initial_spot_qty,
            fut_order_qty: initial_fut_qty,
            current_spot_price: mut current_spot_price,
            initial_limit_price: mut limit_price,
            symbol,
        } = params;

        // --- ИЗМЕНЕНО: Локальные переменные для отслеживания ---
        let mut cumulative_filled_qty = 0.0; // Общее исполненное количество по всем ордерам
        let mut current_order_target_qty = initial_spot_qty; // Цель для текущего ордера
        let mut qty_filled_in_current_order = 0.0; // Сколько исполнено в ТЕКУЩЕМ ордере (с момента его постановки/перестановки)
        // --- Конец изменений ---

        info!(
            "Running hedge for {} with initial_spot_qty={}, initial_fut_qty={}, initial_limit_price={}",
            symbol, initial_spot_qty, initial_fut_qty, limit_price
        );

        let update_current_order_id = |id: Option<String>| {
            let storage = current_order_id_storage.clone();
            async move {
                *storage.lock().await = id;
            }
        };

        // --- ИЗМЕНЕНО: Убираем update_total_filled_qty, будем обновлять напрямую ---

        info!("Placing initial limit buy at {} for qty {}", limit_price, current_order_target_qty);
        tokio::task::yield_now().await;
        let mut spot_order = match self
            .exchange
            .place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price)
            .await {
                Ok(o) => o,
                Err(e) => {
                    error!("Failed to place initial order: {}", e);
                    return Err(e);
                }
            };
        info!("Placed initial spot limit order: id={}", spot_order.id);
        update_current_order_id(Some(spot_order.id.clone())).await;
        // Инициализируем общее хранилище
        *total_filled_qty_storage.lock().await = 0.0;

        let mut start = Instant::now();
        let mut last_update_sent = Instant::now();
        let update_interval = Duration::from_secs(5);

        loop {
             sleep(Duration::from_secs(1)).await;
            let now = Instant::now();

            tokio::task::yield_now().await;
            let status_result = self.exchange.get_order_status(&symbol, &spot_order.id).await;

            let status: OrderStatus = match status_result {
                Ok(s) => s,
                Err(e) => {
                    if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                         warn!("Order {} not found, assuming it filled for target qty {}. Continuing...", spot_order.id, current_order_target_qty);
                         // --- ИЗМЕНЕНО: Обновляем локальное и общее кол-во ---
                         cumulative_filled_qty += current_order_target_qty; // Добавляем целевое кол-во к общему
                         *total_filled_qty_storage.lock().await = cumulative_filled_qty; // Обновляем общее хранилище
                         // --- Конец изменений ---
                         update_current_order_id(None).await;
                         // --- ИЗМЕНЕНО: Проверяем достижение цели здесь ---
                         if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                             info!("Total filled spot quantity {} meets target {}. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                             break; // Выходим из цикла, если цель достигнута
                         } else {
                             // Если цель не достигнута, инициируем замену
                             warn!("Order filled assumption, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                             start = now - self.max_wait - Duration::from_secs(1); // Состариваем таймер
                             qty_filled_in_current_order = 0.0; // Сбрасываем для нового ордера
                             continue; // Переходим к замене
                         }
                         // --- Конец изменений ---
                    } else {
                        warn!("Failed to get order status for {}: {}. Aborting hedge.", spot_order.id, e);
                        *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                        update_current_order_id(None).await;
                        // --- ИЗМЕНЕНО: Возвращаем ошибку с понятным текстом ---
                        return Err(anyhow!("Hedge failed during status check: {}", e));
                        // --- Конец изменений ---
                    }
                }
            };

            // --- ИЗМЕНЕНО: Обновляем локальное и общее кол-во СРАЗУ после получения статуса ---
            let previously_filled_in_current = qty_filled_in_current_order; // Запоминаем предыдущее значение для этого ордера
            qty_filled_in_current_order = status.filled_qty; // Обновляем исполненное в текущем ордере
            let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current; // Сколько исполнилось с прошлой проверки

            if filled_since_last_check > 0.0 { // Обновляем общее, только если что-то исполнилось
                cumulative_filled_qty += filled_since_last_check; // Добавляем разницу к общему
                *total_filled_qty_storage.lock().await = cumulative_filled_qty; // Обновляем общее хранилище
            }
            // --- Конец изменений ---

            // --- Проверяем исполнение ТЕКУЩЕГО ордера ---
            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                info!("Current spot order {} filled (total cumulative: {}).", spot_order.id, cumulative_filled_qty);
                update_current_order_id(None).await;
                // --- ИЗМЕНЕНО: Просто проверяем, достигнут ли общий лимит ---
                if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                    info!("Total filled spot quantity {} meets target {}. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                    break; // Выходим из цикла, если цель достигнута
                } else {
                    // Если текущий ордер исполнился, но общий объем не достигнут,
                    // это значит, что initial_spot_qty был больше, чем qty этого ордера
                    // (что возможно только если это был заменяющий ордер).
                    // Инициируем следующую замену.
                    warn!("Replacement order filled, but total target {} not reached. Triggering next replacement.", initial_spot_qty);
                    start = now - self.max_wait - Duration::from_secs(1); // Состариваем таймер
                    qty_filled_in_current_order = 0.0; // Сбрасываем для нового ордера
                    // continue не нужен, т.к. проверка max_wait сработает на след. итерации
                }
                // --- Конец изменений ---
            }

            let elapsed_since_start = now.duration_since(start);
            let elapsed_since_update = now.duration_since(last_update_sent);

            let mut is_replacement = false;
            let mut price_for_update = current_spot_price;

            // --- Логика перестановки ордера ---
            // --- ИЗМЕНЕНО: Проверяем И если ордер еще не исполнен ---
            if elapsed_since_start > self.max_wait && status.remaining_qty > ORDER_FILL_TOLERANCE {
            // --- Конец изменений ---
                is_replacement = true;
                warn!(
                    "Spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                    spot_order.id, qty_filled_in_current_order, current_order_target_qty, self.max_wait
                );

                let remaining_total_qty = initial_spot_qty - cumulative_filled_qty;

                tokio::task::yield_now().await;
                // --- ИЗМЕНЕНО: Обрабатываем ошибку отмены более мягко ---
                if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                    // Игнорируем ошибки "Order not found" или "already filled/canceled"
                    if !e.to_string().contains("110025") // Order not found or finished
                       && !e.to_string().contains("10001") // Parameter error (может быть, если ордер уже отменен)
                       && !e.to_string().contains("170106") // Order is already cancelled
                       && !e.to_string().contains("170213") // Order does not exist (Bybit Testnet)
                    {
                        error!("Unexpected error cancelling order {}: {}", spot_order.id, e);
                        *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                        update_current_order_id(None).await;
                        // --- ИЗМЕНЕНО: Возвращаем ошибку с понятным текстом ---
                        return Err(anyhow!("Hedge failed during order replacement (cancel step): {}", e));
                        // --- Конец изменений ---
                    } else {
                        warn!("Order {} cancellation failed (likely already inactive): {}", spot_order.id, e);
                    }
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }
                // --- Конец изменений ---
                update_current_order_id(None).await;

                if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                    info!("Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                    break;
                }

                tokio::task::yield_now().await;
                current_spot_price = match self.exchange.get_spot_price(&symbol).await {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Failed to get spot price during replacement: {}. Aborting hedge.", e);
                        *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                        // --- ИЗМЕНЕНО: Возвращаем ошибку с понятным текстом ---
                        return Err(anyhow!("Hedge failed during replacement (get price step): {}", e));
                        // --- Конец изменений ---
                    }
                };
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 - self.slippage);
                price_for_update = current_spot_price;
                current_order_target_qty = remaining_total_qty;
                qty_filled_in_current_order = 0.0;

                info!("New spot price: {}, placing new limit buy at {} for remaining qty {}", current_spot_price, limit_price, current_order_target_qty);
                tokio::task::yield_now().await;
                spot_order = match self.exchange.place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price).await {
                    Ok(o) => o,
                    Err(e) => {
                         warn!("Failed to place replacement order: {}. Aborting hedge.", e);
                         *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                         // --- ИЗМЕНЕНО: Возвращаем ошибку с понятным текстом ---
                         return Err(anyhow!("Hedge failed during replacement (place order step): {}", e));
                         // --- Конец изменений ---
                    }
                };
                info!("Placed replacement spot limit order: id={}", spot_order.id);
                update_current_order_id(Some(spot_order.id.clone())).await;
                start = now;
            }

            // --- Логика обновления статуса в Telegram ---
            if is_replacement || elapsed_since_update > update_interval {
                 if !is_replacement {
                     tokio::task::yield_now().await;
                     match self.exchange.get_spot_price(&symbol).await {
                         Ok(p) if p > 0.0 => price_for_update = p,
                         Ok(_) => { /* Используем старую цену */ }
                         Err(e) => {
                             warn!("Failed to get spot price for update: {}", e); // Не прерываем хедж из-за этого
                         }
                     }
                 }

                let update = HedgeProgressUpdate {
                    current_spot_price: price_for_update,
                    new_limit_price: limit_price,
                    is_replacement,
                };

                tokio::task::yield_now().await;
                if let Err(e) = progress_callback(update).await {
                    if !e.to_string().contains("message is not modified") {
                        warn!("Progress callback failed: {}. Continuing hedge...", e);
                    }
                }
                last_update_sent = now;
            }
        } // Конец цикла loop

        // --- Размещение фьючерсного ордера ---
        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "Placing market sell order on futures ({}) for initial quantity {}",
            futures_symbol, initial_fut_qty
        );
        tokio::task::yield_now().await;
        // --- ИЗМЕНЕНО: Используем place_futures_market_order ---
        let fut_order = match self
            .exchange
            .place_futures_market_order(&futures_symbol, OrderSide::Sell, initial_fut_qty)
            .await {
        // --- Конец изменений ---
                Ok(o) => o,
                Err(e) => {
                    warn!("Failed to place futures order: {}. Spot was already bought!", e);
                    *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                    return Err(anyhow!("Failed to place futures order after spot fill: {}", e));
                }
            };
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Hedge completed successfully for {}. Total spot filled: {}", symbol, cumulative_filled_qty);
        *total_filled_qty_storage.lock().await = cumulative_filled_qty;
        Ok((cumulative_filled_qty, initial_fut_qty))
    }

    // --- ИЗМЕНЕНО: Обновляем run_unhedge аналогично ---
    pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
        // TODO: Добавить поддержку отмены для unhedge по аналогии с run_hedge
    ) -> Result<(f64, f64)> {
        let UnhedgeRequest { quantity: initial_spot_qty, symbol } = req;
        let initial_fut_qty = initial_spot_qty;

         info!(
            "Starting unhedge for {} with initial_quantity={}",
            symbol, initial_spot_qty
        );

        let mut cumulative_filled_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty;
        let mut qty_filled_in_current_order = 0.0;

        if initial_spot_qty <= 0.0 {
             error!("Initial quantity is non-positive: {}. Aborting unhedge.", initial_spot_qty);
             return Err(anyhow::anyhow!("Initial quantity is non-positive"));
        }

        let mut current_spot_price = self.exchange.get_spot_price(&symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }

        let mut limit_price = current_spot_price * (1.0 + self.slippage); // Sell -> +slippage
        info!("Initial spot price: {}, placing limit sell at {} for qty {}", current_spot_price, limit_price, current_order_target_qty);
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);
        let mut start = Instant::now();

        loop {
            sleep(Duration::from_secs(1)).await;
            let now = Instant::now();

             let status: OrderStatus = match self.exchange.get_order_status(&symbol, &spot_order.id).await {
                Ok(s) => s,
                Err(e) => {
                    if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                         warn!("Order {} not found, assuming it filled for target qty {}. Continuing...", spot_order.id, current_order_target_qty);
                         cumulative_filled_qty += current_order_target_qty;
                         // TODO: Update shared state if unhedge cancellation is implemented
                         if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                             info!("Total filled spot quantity {} meets target {}. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                             break;
                         } else {
                             warn!("Order filled assumption, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                             start = now - self.max_wait - Duration::from_secs(1);
                             qty_filled_in_current_order = 0.0;
                             continue;
                         }
                    } else {
                        warn!("Failed to get order status for {}: {}. Aborting unhedge.", spot_order.id, e);
                        // TODO: Update shared state if unhedge cancellation is implemented
                        return Err(anyhow!("Unhedge failed during status check: {}", e));
                    }
                }
            };

            let previously_filled_in_current = qty_filled_in_current_order;
            qty_filled_in_current_order = status.filled_qty;
            let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current;

            if filled_since_last_check > 0.0 {
                cumulative_filled_qty += filled_since_last_check;
                // TODO: Update shared state if unhedge cancellation is implemented
            }

            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                 info!("Current spot order {} filled (total cumulative: {}).", spot_order.id, cumulative_filled_qty);
                 // TODO: Update shared state if unhedge cancellation is implemented
                 if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                     info!("Total filled spot quantity {} meets target {}. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                     break;
                 } else {
                     warn!("Replacement order filled, but total target {} not reached. Triggering next replacement.", initial_spot_qty);
                     start = now - self.max_wait - Duration::from_secs(1);
                     qty_filled_in_current_order = 0.0;
                 }
            }

            if start.elapsed() > self.max_wait && status.remaining_qty > ORDER_FILL_TOLERANCE {
                 warn!(
                    "Spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                    spot_order.id, qty_filled_in_current_order, current_order_target_qty, self.max_wait
                );

                 let remaining_total_qty = initial_spot_qty - cumulative_filled_qty;

                 if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                    if !e.to_string().contains("110025")
                       && !e.to_string().contains("10001")
                       && !e.to_string().contains("170106")
                       && !e.to_string().contains("170213")
                    {
                        error!("Unexpected error cancelling order {}: {}", spot_order.id, e);
                        // TODO: Update shared state if unhedge cancellation is implemented
                        return Err(anyhow!("Unhedge failed during order replacement (cancel step): {}", e));
                    } else {
                        warn!("Order {} cancellation failed (likely already inactive): {}", spot_order.id, e);
                    }
                     sleep(Duration::from_millis(500)).await; // Небольшая пауза после отмены
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }
                 // TODO: Update shared state if unhedge cancellation is implemented (order ID = None)

                 if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                    info!("Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                    break;
                 }

                current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 + self.slippage); // Sell -> +slippage
                current_order_target_qty = remaining_total_qty;
                qty_filled_in_current_order = 0.0;

                info!("New spot price: {}, placing new limit sell at {} for remaining qty {}", current_spot_price, limit_price, current_order_target_qty);
                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
                    .await?;
                 info!("Placed replacement spot limit order: id={}", spot_order.id);
                 // TODO: Update shared state if unhedge cancellation is implemented (new order ID)
                start = Instant::now();
            }
        } // Конец цикла loop

        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "Placing market buy order on futures ({}) for initial quantity {}",
            futures_symbol, initial_fut_qty
        );
        // --- ИЗМЕНЕНО: Используем place_futures_market_order ---
        let fut_order = self
            .exchange
            .place_futures_market_order(&futures_symbol, OrderSide::Buy, initial_fut_qty)
            .await?;
        // --- Конец изменений ---
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Unhedge completed successfully for {}. Total spot filled: {}", symbol, cumulative_filled_qty);
        // TODO: Update shared state if unhedge cancellation is implemented
        Ok((cumulative_filled_qty, initial_fut_qty))
    }
    // --- Конец изменений ---
}