// src/hedger.rs

use anyhow::{anyhow, Result};
use tracing::{debug, error, info, warn};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use futures::future::BoxFuture;
// --- ДОБАВЛЕНО: Для отмены ---
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
// --- Конец добавления ---

// --- Используемые типажи и структуры ---
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
        // ... (код без изменений) ...
        let HedgeRequest { sum, symbol, volatility } = req;
        debug!(
            "Calculating hedge params for {} with sum={}, volatility={}",
            symbol, sum, volatility
        );

        // --- Опционально: Получаем и логируем комиссию ---
        match self.exchange.get_fee_rate(symbol, SPOT_CATEGORY).await {
            Ok(fee) => info!(symbol, category=SPOT_CATEGORY, maker_fee=fee.maker, taker_fee=fee.taker, "Current spot fee rate"),
            Err(e) => warn!("Could not get spot fee rate for {}: {}", symbol, e),
        }
        match self.exchange.get_fee_rate(symbol, LINEAR_CATEGORY).await {
             Ok(fee) => info!(symbol, category=LINEAR_CATEGORY, maker_fee=fee.maker, taker_fee=fee.taker, "Current futures fee rate"),
             Err(e) => warn!("Could not get futures fee rate for {}: {}", symbol, e),
        }
        // --- Конец опциональной части ---


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


    // --- ИЗМЕНЕНО: Добавляем параметры для отмены ---
    pub async fn run_hedge(
        &self,
        params: HedgeParams,
        mut progress_callback: HedgeProgressCallback,
        current_order_id_storage: Arc<TokioMutex<Option<String>>>, // <-- Добавлено
        total_filled_qty_storage: Arc<TokioMutex<f64>>, // <-- Добавлено
    ) -> Result<(f64, f64)>
    // --- Конец изменений ---
    {
        let HedgeParams {
            spot_order_qty: initial_spot_qty,
            fut_order_qty: initial_fut_qty,
            current_spot_price: mut current_spot_price,
            initial_limit_price: mut limit_price,
            symbol,
        } = params;

        let mut total_filled_spot_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty;

        info!(
            "Running hedge for {} with initial_spot_qty={}, initial_fut_qty={}, initial_limit_price={}",
            symbol, initial_spot_qty, initial_fut_qty, limit_price
        );

        // --- Функция для обновления ID текущего ордера ---
        let update_current_order_id = |id: Option<String>| {
            let storage = current_order_id_storage.clone();
            async move {
                *storage.lock().await = id;
            }
        };

        // --- Функция для обновления общего исполненного кол-ва ---
        // Возвращает новое общее кол-во
        let update_total_filled_qty = |filled_now: f64| {
            let storage = total_filled_qty_storage.clone();
            async move {
                let mut guard = storage.lock().await;
                *guard += filled_now;
                *guard // Возвращаем новое значение
            }
        };

        info!("Placing initial limit buy at {} for qty {}", limit_price, current_order_target_qty);
        // --- Проверка на отмену перед place_limit_order ---
        tokio::task::yield_now().await;
        let mut spot_order = match self
            .exchange
            .place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price)
            .await {
                Ok(o) => o,
                Err(e) => {
                    error!("Failed to place initial order: {}", e);
                    return Err(e); // Выходим, если первый ордер не удалось поставить
                }
            };
        info!("Placed initial spot limit order: id={}", spot_order.id);
        update_current_order_id(Some(spot_order.id.clone())).await; // Сохраняем ID

        let mut start = Instant::now();
        let mut last_update_sent = Instant::now();
        let update_interval = Duration::from_secs(5);

        loop {
            // --- Проверка на отмену перед sleep ---
             sleep(Duration::from_secs(1)).await;
            let now = Instant::now();

            // --- Проверка на отмену перед get_order_status ---
            tokio::task::yield_now().await;
            let status_result = self.exchange.get_order_status(&symbol, &spot_order.id).await;

            let status: OrderStatus = match status_result {
                Ok(s) => s,
                Err(e) => {
                    if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                         warn!("Order {} not found, assuming it filled for target qty {}. Continuing...", spot_order.id, current_order_target_qty);
                         // --- Обновляем общее исполненное кол-во ---
                         total_filled_spot_qty = update_total_filled_qty(current_order_target_qty).await;
                         update_current_order_id(None).await; // Ордера больше нет
                         // --- Конец обновления ---
                         if total_filled_spot_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                             info!("Total filled spot quantity {} meets target {}. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                             break;
                         } else {
                             warn!("Order filled assumption, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                             start = now - self.max_wait - Duration::from_secs(1);
                             continue;
                         }
                    } else {
                        warn!("Failed to get order status for {}: {}. Retrying...", spot_order.id, e);
                        // Если ошибка получения статуса, возможно, задача отменена
                        // Даем шанс yield_now сработать перед следующей итерацией
                        tokio::task::yield_now().await;
                        continue;
                    }
                }
            };

            // --- Проверяем исполнение ТЕКУЩЕГО ордера ---
            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                info!("Current spot order {} filled for qty {}.", spot_order.id, status.filled_qty);
                // --- Обновляем общее исполненное кол-во ---
                total_filled_spot_qty = update_total_filled_qty(status.filled_qty).await;
                update_current_order_id(None).await; // Ордер исполнился
                // --- Конец обновления ---
                if total_filled_spot_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                    info!("Total filled spot quantity {} meets target {}. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                    break;
                } else {
                    warn!("Current order filled, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                    start = now - self.max_wait - Duration::from_secs(1);
                    // Переходим к следующей итерации, где сработает замена на остаток
                }
            }

            let elapsed_since_start = now.duration_since(start);
            let elapsed_since_update = now.duration_since(last_update_sent);

            let mut is_replacement = false;
            let mut price_for_update = current_spot_price;

            // --- Логика перестановки ордера ---
            if elapsed_since_start > self.max_wait {
                is_replacement = true;
                warn!(
                    "Spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                    spot_order.id, status.filled_qty, current_order_target_qty, self.max_wait
                );

                // --- Обновляем общее исполненное кол-во ---
                total_filled_spot_qty = update_total_filled_qty(status.filled_qty).await;
                // --- Конец обновления ---
                let remaining_total_qty = initial_spot_qty - total_filled_spot_qty;

                // --- Проверка на отмену перед cancel_order ---
                tokio::task::yield_now().await;
                // Отменяем старый ордер
                if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                    if !e.to_string().contains("110025") && !e.to_string().contains("10001") && !e.to_string().contains("170106") {
                        error!("Unexpected error cancelling order {}: {}", spot_order.id, e);
                        // Если отмена не удалась из-за отмены задачи, выходим
                        update_current_order_id(None).await;
                        return Err(anyhow!("Hedge task cancelled during order replacement (cancel step): {}", e));
                    }
                    // Игнорируем ошибки "уже неактивен"
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }
                update_current_order_id(None).await; // Старого ордера больше нет

                if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                    info!("Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                    break;
                }

                // --- Проверка на отмену перед get_spot_price ---
                tokio::task::yield_now().await;
                current_spot_price = match self.exchange.get_spot_price(&symbol).await {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Failed to get spot price during replacement: {}. Assuming cancellation.", e);
                        return Err(anyhow!("Hedge task cancelled during replacement (get price step): {}", e));
                    }
                };
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 - self.slippage);
                price_for_update = current_spot_price;
                current_order_target_qty = remaining_total_qty;

                info!("New spot price: {}, placing new limit buy at {} for remaining qty {}", current_spot_price, limit_price, current_order_target_qty);
                // --- Проверка на отмену перед place_limit_order ---
                tokio::task::yield_now().await;
                spot_order = match self.exchange.place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price).await {
                    Ok(o) => o,
                    Err(e) => {
                         warn!("Failed to place replacement order: {}. Assuming cancellation.", e);
                         return Err(anyhow!("Hedge task cancelled during replacement (place order step): {}", e));
                    }
                };
                info!("Placed replacement spot limit order: id={}", spot_order.id);
                update_current_order_id(Some(spot_order.id.clone())).await; // Сохраняем новый ID
                start = now;
            }

            // --- Логика обновления статуса в Telegram ---
            if is_replacement || elapsed_since_update > update_interval {
                 if !is_replacement {
                     // --- Проверка на отмену перед get_spot_price (для обновления) ---
                     tokio::task::yield_now().await;
                     match self.exchange.get_spot_price(&symbol).await {
                         Ok(p) if p > 0.0 => price_for_update = p,
                         Ok(_) => { /* Используем старую цену */ }
                         Err(e) => {
                             warn!("Failed to get spot price for update: {}. Assuming cancellation.", e);
                             // Не выходим, просто не обновляем цену
                         }
                     }
                 }

                let update = HedgeProgressUpdate {
                    current_spot_price: price_for_update,
                    new_limit_price: limit_price,
                    is_replacement,
                };

                // --- Проверка на отмену перед вызовом колбэка ---
                tokio::task::yield_now().await;
                if let Err(e) = progress_callback(update).await {
                    if !e.to_string().contains("message is not modified") {
                        warn!("Progress callback failed: {}. Continuing hedge...", e);
                        // Если колбэк упал из-за отмены, мы это не узнаем легко.
                        // Но yield_now выше должен был помочь.
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
        // --- Проверка на отмену перед place_market_order ---
        tokio::task::yield_now().await;
        let fut_order = match self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Sell, initial_fut_qty)
            .await {
                Ok(o) => o,
                Err(e) => {
                    warn!("Failed to place futures order: {}. Spot was already bought!", e);
                    // Спот уже куплен, но фьюч не продался. Ситуация плохая.
                    // Возвращаем ошибку, но total_filled_spot_qty будет > 0
                    return Err(anyhow!("Failed to place futures order after spot fill: {}", e));
                }
            };
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Hedge completed successfully for {}. Total spot filled: {}", symbol, total_filled_spot_qty);
        Ok((total_filled_spot_qty, initial_fut_qty))
    }


    pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
        // TODO: Добавить поддержку отмены для unhedge по аналогии с run_hedge
        // Потребуется передавать Arc<Mutex<...>> для ID и кол-ва
    ) -> Result<(f64, f64)> {
        let UnhedgeRequest { quantity: initial_spot_qty, symbol } = req;
        let initial_fut_qty = initial_spot_qty;

         info!(
            "Starting unhedge for {} with initial_quantity={}",
            symbol, initial_spot_qty
        );

        let mut total_filled_spot_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty;

        if initial_spot_qty <= 0.0 {
             error!("Initial quantity is non-positive: {}. Aborting unhedge.", initial_spot_qty);
             return Err(anyhow::anyhow!("Initial quantity is non-positive"));
        }

        let mut current_spot_price = self.exchange.get_spot_price(&symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }

        let mut limit_price = current_spot_price * (1.0 + self.slippage);
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
                         total_filled_spot_qty += current_order_target_qty;
                         if total_filled_spot_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                             info!("Total filled spot quantity {} meets target {}. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                             break;
                         } else {
                             warn!("Order filled assumption, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                             start = now - self.max_wait - Duration::from_secs(1);
                             continue;
                         }
                    } else {
                        warn!("Failed to get order status for {}: {}. Retrying...", spot_order.id, e);
                        continue;
                    }
                }
            };

            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                 info!("Current spot order {} filled for qty {}.", spot_order.id, status.filled_qty);
                 total_filled_spot_qty += status.filled_qty;
                 if total_filled_spot_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                     info!("Total filled spot quantity {} meets target {}. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                     break;
                 } else {
                     warn!("Current order filled, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                     start = now - self.max_wait - Duration::from_secs(1);
                 }
            }

            if start.elapsed() > self.max_wait {
                 warn!(
                    "Spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                    spot_order.id, status.filled_qty, current_order_target_qty, self.max_wait
                );

                 total_filled_spot_qty += status.filled_qty;
                 let remaining_total_qty = initial_spot_qty - total_filled_spot_qty;

                 if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                    if !e.to_string().contains("110025") && !e.to_string().contains("10001") && !e.to_string().contains("170106") {
                        error!("Unexpected error cancelling order {}: {}", spot_order.id, e);
                    }
                     sleep(Duration::from_millis(500)).await;
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                 if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                    info!("Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                    break;
                 }

                current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 + self.slippage);
                current_order_target_qty = remaining_total_qty;

                info!("New spot price: {}, placing new limit sell at {} for remaining qty {}", current_spot_price, limit_price, current_order_target_qty);
                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
                    .await?;
                 info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = Instant::now();
            }
        } // Конец цикла loop

        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "Placing market buy order on futures ({}) for initial quantity {}",
            futures_symbol, initial_fut_qty
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Buy, initial_fut_qty)
            .await?;
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Unhedge completed successfully for {}. Total spot filled: {}", symbol, total_filled_spot_qty);
        Ok((total_filled_spot_qty, initial_fut_qty))
    }
}