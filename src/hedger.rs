// src/hedger.rs

use anyhow::Result;
use tracing::log::{error, info, warn}; // Добавляем для логирования
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::exchange::{Exchange, OrderStatus};
use crate::exchange::types::OrderSide;
use crate::models::{HedgeRequest, UnhedgeRequest};

// Константа для сравнения остатка по ордеру
const ORDER_FILL_TOLERANCE: f64 = 1e-8;
// Константа для формата фьючерсного символа (можно вынести в конфиг или сделать параметр)
const FUTURES_SYMBOL_SUFFIX: &str = "USDT";

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
    /// 2) считаем spot/fut
    /// 3) ставим limit‐BUY на споте, реплейсим если > max_wait
    /// 4) market‐SELL на фьюче
    pub async fn run_hedge(
        &self,
        req: HedgeRequest,
    ) -> Result<(f64, f64)> { // Возвращаем (spot_qty, fut_qty)
        let HedgeRequest { sum, symbol, volatility } = req;
        info!(
            "Starting hedge for {} with sum={}, volatility={}",
            symbol, sum, volatility
        );

        // 1) вычитаем одну комиссию от исходной суммы
        let adj_sum = sum * (1.0 - self.commission);
        info!("Adjusted sum after commission: {}", adj_sum);

        // 2) вычисляем объёмы
        let mmr = self.exchange.get_mmr(&symbol).await?;
        let spot_qty = adj_sum / ((1.0 + volatility) * (1.0 + mmr));
        let fut_qty  = adj_sum - spot_qty;
        info!(
            "Calculated quantities: spot={}, futures={}, MMR={}",
            spot_qty, fut_qty, mmr
        );

        if spot_qty <= 0.0 || fut_qty <= 0.0 {
             error!("Calculated quantities are non-positive: spot={}, fut={}. Aborting hedge.", spot_qty, fut_qty);
             return Err(anyhow::anyhow!("Calculated quantities are non-positive"));
        }


        // 3) limit BUY на споте с реплейсом
        let mut price = self.exchange.get_spot_price(&symbol).await?;
        let mut limit_price = price * (1.0 - self.slippage);
        info!("Initial spot price: {}, placing limit buy at {}", price, limit_price);
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Buy, spot_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);
        let mut start = Instant::now();

        loop {
            sleep(Duration::from_secs(1)).await;
            let status: OrderStatus = match self.exchange.get_order_status(&symbol, &spot_order.id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to get order status for {}: {}. Retrying...", spot_order.id, e);
                    // Можно добавить логику повторных попыток или прерывания, если ошибка постоянная
                    continue;
                }
            };

            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                info!("Spot order {} filled.", spot_order.id);
                break;
            }

            // Сравниваем Duration напрямую
            if start.elapsed() > self.max_wait {
                warn!(
                    "Spot order {} not filled within {:?}. Replacing...",
                    spot_order.id, self.max_wait
                );
                // Пытаемся отменить старый ордер, логируем ошибку
                if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                    error!(
                        "Failed to cancel order {} before replacing: {}. Continuing, but risk of double order exists!",
                        spot_order.id, e
                    );
                    // В критической ситуации можно прервать операцию:
                    // return Err(anyhow::anyhow!("Failed to cancel order {}: {}", spot_order.id, e));
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                // Получаем новую цену и ставим новый ордер
                price = self.exchange.get_spot_price(&symbol).await?;
                limit_price = price * (1.0 - self.slippage);
                info!("New spot price: {}, placing new limit buy at {}", price, limit_price);
                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Buy, spot_qty, limit_price)
                    .await?;
                info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = Instant::now(); // Сбрасываем таймер
            }
        }

        // 4) рыночный SELL на фьюче
        let futures_symbol = format!("{}{}", symbol, FUTURES_SYMBOL_SUFFIX);
        info!(
            "Placing market sell order on futures ({}) for quantity {}",
            futures_symbol, fut_qty // <-- ИСПРАВЛЕНО: используем fut_qty
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Sell, fut_qty) // <-- ИСПРАВЛЕНО: используем fut_qty
            .await?;
        info!("Placed futures market order: id={}", fut_order.id); // Предполагаем, что place_market_order возвращает OrderInfo

        info!("Hedge completed successfully for {}", symbol);
        // Возвращаем рассчитанные объемы
        Ok((spot_qty, fut_qty)) // <-- ИСПРАВЛЕНО: возвращаем правильные значения
    }

    /// Цикл расхеджирования:
    /// 1) корректируем sum (объем) на комиссию
    /// 2) limit‐SELL на споте с реплейсом
    /// 3) market‐BUY на фьюче
    pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
    ) -> Result<(f64, f64)> { // Возвращаем (spot_qty, fut_qty) - здесь они равны adj_sum
        let UnhedgeRequest { sum, symbol } = req; // sum здесь - это объем для расхеджирования
         info!(
            "Starting unhedge for {} with quantity={}",
            symbol, sum
        );

        // 1) корректируем объем на комиссию (если нужно, зависит от логики)
        // Возможно, комиссию здесь учитывать не нужно, если 'sum' - это точный объем актива,
        // который нужно продать на споте и откупить на фьюче.
        // Оставим пока так, но это место для уточнения бизнес-логики.
        let adj_sum = sum * (1.0 - self.commission);
        info!("Adjusted quantity after commission: {}", adj_sum);

        if adj_sum <= 0.0 {
             error!("Adjusted quantity is non-positive: {}. Aborting unhedge.", adj_sum);
             return Err(anyhow::anyhow!("Adjusted quantity is non-positive"));
        }

        // 2) limit SELL на споте с реплейсом
        let mut price = self.exchange.get_spot_price(&symbol).await?;
        let mut limit_price = price * (1.0 + self.slippage);
        info!("Initial spot price: {}, placing limit sell at {}", price, limit_price);
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Sell, adj_sum, limit_price)
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
                // Пытаемся отменить старый ордер, логируем ошибку
                if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                     error!(
                        "Failed to cancel order {} before replacing: {}. Continuing, but risk of double order exists!",
                        spot_order.id, e
                    );
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                // Получаем новую цену и ставим новый ордер
                price = self.exchange.get_spot_price(&symbol).await?;
                limit_price = price * (1.0 + self.slippage);
                info!("New spot price: {}, placing new limit sell at {}", price, limit_price);
                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Sell, adj_sum, limit_price)
                    .await?;
                 info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = Instant::now();
            }
        }

        // 3) market BUY на фьюче
        let futures_symbol = format!("{}{}", symbol, FUTURES_SYMBOL_SUFFIX);
        info!(
            "Placing market buy order on futures ({}) for quantity {}",
            futures_symbol, adj_sum
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Buy, adj_sum)
            .await?;
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Unhedge completed successfully for {}", symbol);
        // Возвращаем объем, который был обработан на споте и фьюче
        Ok((adj_sum, adj_sum))
    }
}

