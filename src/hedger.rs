// src/hedger.rs

use anyhow::{anyhow, Result};
use tracing::{debug, error, info, warn};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use futures::future::BoxFuture;

// --- ИЗМЕНЕНО: Добавляем SPOT_CATEGORY и LINEAR_CATEGORY из bybit (или определяем здесь) ---
use crate::exchange::{Exchange, OrderStatus, FeeRate}; // Добавляем FeeRate
// Если константы не pub в bybit.rs, определяем их здесь:
// const SPOT_CATEGORY: &str = "spot";
// const LINEAR_CATEGORY: &str = "linear";
// Если они pub, используем импорт:
use crate::exchange::bybit::{SPOT_CATEGORY, LINEAR_CATEGORY};
// --- Конец изменений ---
use crate::exchange::types::OrderSide;
use crate::models::{HedgeRequest, UnhedgeRequest};

const ORDER_FILL_TOLERANCE: f64 = 1e-8;

pub struct Hedger<E> {
    exchange:     E,
    slippage:     f64,
    // commission:   f64, // <-- УДАЛЕНО
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
    // --- ИЗМЕНЕНО: Убираем commission ---
    pub fn new(exchange: E, slippage: f64, max_wait_secs: u64, quote_currency: String) -> Self {
        Self {
            exchange,
            slippage,
            max_wait: Duration::from_secs(max_wait_secs),
            quote_currency,
        }
    }
    // --- Конец изменений ---

    pub async fn calculate_hedge_params(&self, req: &HedgeRequest) -> Result<HedgeParams> {
        let HedgeRequest { sum, symbol, volatility } = req;
        debug!(
            "Calculating hedge params for {} with sum={}, volatility={}",
            symbol, sum, volatility
        );

        // --- ИЗМЕНЕНО: Убираем adj_sum ---
        // --- Конец изменений ---

        // --- Опционально: Получаем и логируем комиссию ---
        match self.exchange.get_fee_rate(symbol, SPOT_CATEGORY).await {
            Ok(fee) => info!(symbol, category=SPOT_CATEGORY, maker_fee=fee.maker, taker_fee=fee.taker, "Current spot fee rate"),
            Err(e) => warn!("Could not get spot fee rate for {}: {}", symbol, e),
        }
        // Для фьючерсов используем базовый символ при запросе комиссии
        match self.exchange.get_fee_rate(symbol, LINEAR_CATEGORY).await {
             Ok(fee) => info!(symbol, category=LINEAR_CATEGORY, maker_fee=fee.maker, taker_fee=fee.taker, "Current futures fee rate"),
             Err(e) => warn!("Could not get futures fee rate for {}: {}", symbol, e),
        }
        // --- Конец опциональной части ---


        let mmr = self.exchange.get_mmr(symbol).await?;
        // --- ИЗМЕНЕНО: Используем sum напрямую ---
        let spot_value = sum / ((1.0 + volatility) * (1.0 + mmr));
        let fut_value  = sum - spot_value;
        // --- Конец изменений ---
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
    ) -> Result<(f64, f64)> {

        let HedgeParams {
            spot_order_qty,
            fut_order_qty,
            current_spot_price: mut current_spot_price,
            initial_limit_price: mut limit_price,
            symbol,
        } = params;

        info!(
            "Running hedge for {} with spot_qty={}, fut_qty={}, initial_limit_price={}",
            symbol, spot_order_qty, fut_order_qty, limit_price
        );

        info!("Placing initial limit buy at {} for qty {}", limit_price, spot_order_qty);
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Buy, spot_order_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);

        let mut start = Instant::now();
        let mut last_update_sent = Instant::now();
        let update_interval = Duration::from_secs(5);

        loop {
            sleep(Duration::from_secs(1)).await;
            let now = Instant::now();

            let status: OrderStatus = match self.exchange.get_order_status(&symbol, &spot_order.id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to get order status for {}: {}. Retrying...", spot_order.id, e);
                    continue;
                }
            };

            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                info!("Spot order {} filled.", spot_order.id);
                break;
            }

            let elapsed_since_start = now.duration_since(start);
            let elapsed_since_update = now.duration_since(last_update_sent);

            let mut is_replacement = false;
            let mut price_for_update = current_spot_price;

            if elapsed_since_start > self.max_wait {
                is_replacement = true;
                warn!(
                    "Spot order {} not filled within {:?}. Replacing...",
                    spot_order.id, self.max_wait
                );
                if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                    // Ошибка уже логируется внутри cancel_order, если это "not found"
                    // Логируем только неожиданные ошибки
                    if !e.to_string().contains("110025") && !e.to_string().contains("10001") && !e.to_string().contains("170106") {
                        error!("Unexpected error cancelling order {}: {}", spot_order.id, e);
                    }
                    // Небольшая пауза на всякий случай
                    sleep(Duration::from_millis(500)).await;
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 - self.slippage);
                price_for_update = current_spot_price;
                info!("New spot price: {}, placing new limit buy at {} for qty {}", current_spot_price, limit_price, spot_order_qty);

                spot_order = self.exchange.place_limit_order(&symbol, OrderSide::Buy, spot_order_qty, limit_price).await?;
                info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = now;
            }

            if is_replacement || elapsed_since_update > update_interval {
                if !is_replacement {
                    match self.exchange.get_spot_price(&symbol).await {
                        Ok(p) if p > 0.0 => price_for_update = p,
                        _ => { /* Используем последнюю известную */ }
                    }
                }

                let update = HedgeProgressUpdate {
                    current_spot_price: price_for_update,
                    new_limit_price: limit_price,
                    is_replacement,
                };

                if let Err(e) = progress_callback(update).await {
                    warn!("Progress callback failed: {}. Continuing hedge...", e);
                }
                last_update_sent = now;
            }
        } // Конец цикла

        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "Placing market sell order on futures ({}) for quantity {}",
            futures_symbol, fut_order_qty
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Sell, fut_order_qty) // Передаем полный символ
            .await?;
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Hedge completed successfully for {}", symbol);
        Ok((spot_order_qty, fut_order_qty))
    }


    pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
        // mut progress_callback: Option<HedgeProgressCallback>, // Пока без колбэка
    ) -> Result<(f64, f64)> {
        let UnhedgeRequest { quantity, symbol } = req;
         info!(
            "Starting unhedge for {} with quantity={}",
            symbol, quantity
        );

        let spot_order_qty = quantity;
        let fut_order_qty = quantity;
        info!("Using order quantity: {}", spot_order_qty);

        if spot_order_qty <= 0.0 {
             error!("Order quantity is non-positive: {}. Aborting unhedge.", spot_order_qty);
             return Err(anyhow::anyhow!("Order quantity is non-positive"));
        }

        let mut current_spot_price = self.exchange.get_spot_price(&symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }

        let mut limit_price = current_spot_price * (1.0 + self.slippage);
        info!("Initial spot price: {}, placing limit sell at {} for qty {}", current_spot_price, limit_price, spot_order_qty);
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Sell, spot_order_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);
        let mut start = Instant::now();

        loop {
            sleep(Duration::from_secs(1)).await;
             let status: OrderStatus = match self.exchange.get_order_status(&symbol, &spot_order.id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to get order status for {}: {}. Retrying...", spot_order.id, e);
                    continue;
                }
            };

            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                 info!("Spot order {} filled.", spot_order.id);
                break;
            }

            if start.elapsed() > self.max_wait {
                 warn!(
                    "Spot order {} not filled within {:?}. Replacing...",
                    spot_order.id, self.max_wait
                );
                 if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                    if !e.to_string().contains("110025") && !e.to_string().contains("10001") && !e.to_string().contains("170106") {
                        error!("Unexpected error cancelling order {}: {}", spot_order.id, e);
                    }
                     sleep(Duration::from_millis(500)).await;
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 + self.slippage);
                info!("New spot price: {}, placing new limit sell at {} for qty {}", current_spot_price, limit_price, spot_order_qty);

                // --- МЕСТО ДЛЯ ВЫЗОВА КОЛБЭКА (если нужно для unhedge) ---
                // if let Some(ref mut cb) = progress_callback {
                //     let update = HedgeProgressUpdate { current_spot_price, new_limit_price: limit_price, is_replacement: true };
                //     if let Err(e) = cb(update).await {
                //         warn!("Unhedge progress callback failed: {}. Continuing...", e);
                //     }
                // }
                // --- Конец места для вызова колбэка ---

                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Sell, spot_order_qty, limit_price)
                    .await?;
                 info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = Instant::now();
            }
        }

        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "Placing market buy order on futures ({}) for quantity {}",
            futures_symbol, fut_order_qty
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Buy, fut_order_qty) // Передаем полный символ
            .await?;
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Unhedge completed successfully for {}", symbol);
        Ok((spot_order_qty, fut_order_qty))
    }
}