// src/hedger.rs

use anyhow::{anyhow, Result}; // Добавляем anyhow
use tracing::{error, info, warn}; // Используем tracing напрямую
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::exchange::{Exchange, OrderStatus};
use crate::exchange::types::OrderSide;
use crate::models::{HedgeRequest, UnhedgeRequest};

// Константа для сравнения остатка по ордеру
const ORDER_FILL_TOLERANCE: f64 = 1e-8;
// Константа для формата фьючерсного символа
const FUTURES_SYMBOL_SUFFIX: &str = "USDT"; // Предполагаем USDT perpetuals

/// Полный цикл хеджирования / расхеджирования
pub struct Hedger<E> {
    exchange:     E,
    slippage:     f64, // 0.005 = 0.5%
    commission:   f64, // 0.001 = 0.1%
    max_wait:     Duration, // Используем Duration для ясности
}

impl<E> Hedger<E>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    /// slippage и commission — доли (0.005 = 0.5%, 0.001 = 0.1%)
    /// max_wait_secs - время в секундах, после которого переставляем лимитный ордер
    pub fn new(exchange: E, slippage: f64, commission: f64, max_wait_secs: u64) -> Self {
        Self {
            exchange,
            slippage,
            commission,
            max_wait: Duration::from_secs(max_wait_secs), // Сохраняем как Duration
        }
    }

    /// Цикл хеджирования:
    /// 1) корректируем sum на комиссию
    /// 2) считаем spot/fut VALUE (стоимость в USDT)
    /// 3) получаем цену, считаем spot/fut QUANTITY (количество базового актива)
    /// 4) ставим limit‐BUY на споте, реплейсим если > max_wait
    /// 5) market‐SELL на фьюче
    pub async fn run_hedge(
        &self,
        req: HedgeRequest,
    ) -> Result<(f64, f64)> { // Возвращаем (spot_order_qty, fut_order_qty)
        let HedgeRequest { sum, symbol, volatility } = req;
        info!(
            "Starting hedge for {} with sum={}, volatility={}",
            symbol, sum, volatility
        );

        // 1) вычитаем одну комиссию от исходной суммы
        let adj_sum = sum * (1.0 - self.commission);
        info!("Adjusted sum after commission: {}", adj_sum);

        // 2) вычисляем VALUE (стоимость в USDT) для спота и фьюча
        let mmr = self.exchange.get_mmr(&symbol).await?;
        let spot_value = adj_sum / ((1.0 + volatility) * (1.0 + mmr));
        let fut_value  = adj_sum - spot_value;
        info!(
            "Calculated values: spot_value={}, fut_value={}, MMR={}",
            spot_value, fut_value, mmr
        );

        if spot_value <= 0.0 || fut_value <= 0.0 {
             error!("Calculated values are non-positive: spot={}, fut={}. Aborting hedge.", spot_value, fut_value);
             return Err(anyhow::anyhow!("Calculated values are non-positive"));
        }

        // 3) Получаем цену и считаем QUANTITY (количество базового актива)
        let mut current_spot_price = self.exchange.get_spot_price(&symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }
        let spot_order_qty = spot_value / current_spot_price;
        // Для рыночного ордера на фьюче используем текущую спот цену как оценку
        let fut_order_qty = fut_value / current_spot_price;
        info!(
            "Calculated quantities based on price {}: spot_order_qty={}, fut_order_qty={}",
            current_spot_price, spot_order_qty, fut_order_qty
        );

        if spot_order_qty <= 0.0 || fut_order_qty <= 0.0 {
             error!("Calculated order quantities are non-positive. Aborting hedge.");
             return Err(anyhow::anyhow!("Calculated order quantities are non-positive"));
        }


        // 4) limit BUY на споте с реплейсом
        let mut limit_price = current_spot_price * (1.0 - self.slippage);
        info!("Initial spot price: {}, placing limit buy at {} for qty {}", current_spot_price, limit_price, spot_order_qty);
        // --- ИСПРАВЛЕНО: Передаем spot_order_qty ---
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Buy, spot_order_qty, limit_price)
            .await?;
        // --- Конец исправления ---
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
                    error!(
                        "Failed to cancel order {} before replacing: {}. Continuing, but risk of double order exists!",
                        spot_order.id, e
                    );
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 - self.slippage);
                // Оставляем то же количество spot_order_qty. Пересчет изменит пропорцию хеджа.
                info!("New spot price: {}, placing new limit buy at {} for qty {}", current_spot_price, limit_price, spot_order_qty);
                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Buy, spot_order_qty, limit_price)
                    .await?;
                info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = Instant::now();
            }
        }

        // 5) рыночный SELL на фьюче
        let futures_symbol = format!("{}{}", symbol, FUTURES_SYMBOL_SUFFIX);
        info!(
            "Placing market sell order on futures ({}) for quantity {}",
            futures_symbol, fut_order_qty // Используем fut_order_qty
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Sell, fut_order_qty)
            .await?;
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Hedge completed successfully for {}", symbol);
        // Возвращаем реальные объемы ордеров
        Ok((spot_order_qty, fut_order_qty))
    }

    /// Цикл расхеджирования:
    /// 1) sum - это QUANTITY актива (например, BTC)
    /// 2) корректируем quantity на комиссию (если нужно) -> Решено НЕ корректировать
    /// 3) получаем цену
    /// 4) limit‐SELL на споте с реплейсом
    /// 5) market‐BUY на фьюче
    pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
    ) -> Result<(f64, f64)> { // Возвращаем (spot_order_qty, fut_order_qty)
        let UnhedgeRequest { sum: quantity, symbol } = req; // sum здесь - это QUANTITY актива
         info!(
            "Starting unhedge for {} with quantity={}",
            symbol, quantity
        );

        // 2) Используем quantity напрямую как количество для ордеров
        let spot_order_qty = quantity;
        let fut_order_qty = quantity; // Продаем на споте, покупаем на фьюче то же кол-во
        info!("Using order quantity: {}", spot_order_qty);

        if spot_order_qty <= 0.0 {
             error!("Order quantity is non-positive: {}. Aborting unhedge.", spot_order_qty);
             return Err(anyhow::anyhow!("Order quantity is non-positive"));
        }

        // 3) Получаем цену для установки лимитного ордера
        let mut current_spot_price = self.exchange.get_spot_price(&symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }

        // 4) limit SELL на споте с реплейсом
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
                     error!(
                        "Failed to cancel order {} before replacing: {}. Continuing, but risk of double order exists!",
                        spot_order.id, e
                    );
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 + self.slippage);
                // Оставляем то же количество spot_order_qty
                info!("New spot price: {}, placing new limit sell at {} for qty {}", current_spot_price, limit_price, spot_order_qty);
                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Sell, spot_order_qty, limit_price)
                    .await?;
                 info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = Instant::now();
            }
        }

        // 5) market BUY на фьюче
        let futures_symbol = format!("{}{}", symbol, FUTURES_SYMBOL_SUFFIX);
        info!(
            "Placing market buy order on futures ({}) for quantity {}",
            futures_symbol, fut_order_qty // Используем fut_order_qty
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Buy, fut_order_qty)
            .await?;
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Unhedge completed successfully for {}", symbol);
        // Возвращаем объем, который был обработан на споте и фьюче
        Ok((spot_order_qty, fut_order_qty))
    }
}