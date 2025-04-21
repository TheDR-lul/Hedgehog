// src/hedger.rs

use anyhow::Result;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::exchange::{Exchange, OrderStatus};
use crate::exchange::types::OrderSide;
use crate::models::{HedgeRequest, UnhedgeRequest};

/// Полный цикл хеджирования / расхеджирования
pub struct Hedger<E> {
    exchange:     E,
    slippage:     f64, // 0.005 = 0.5%
    commission:   f64, // 0.001 = 0.1%
    max_wait:     u64,  // сек, после которых реплейсим лимитный ордер
}

impl<E> Hedger<E>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    /// slippage и commission — доли (0.005 = 0.5%, 0.001 = 0.1%)
    pub fn new(exchange: E, slippage: f64, commission: f64) -> Self {
        Self { exchange, slippage, commission, max_wait: 30 }
    }

    /// Цикл хеджирования:
    /// 1) корректируем sum на комиссию
    /// 2) считаем spot/fut
    /// 3) ставим limit‐BUY на споте, реплейсим если > max_wait
    /// 4) market‐SELL на фьюче
    pub async fn run_hedge(
        &self,
        req: HedgeRequest,
    ) -> Result<(f64, f64)> {
        let HedgeRequest { sum, symbol, volatility } = req;

        // 1) вычитаем одну комиссию от исходной суммы
        let adj_sum = sum * (1.0 - self.commission);

        // 2) вычисляем объёмы
        let mmr = self.exchange.get_mmr(&symbol).await?;
        let spot_qty = adj_sum / ((1.0 + volatility) * (1.0 + mmr));
        let fut_qty  = adj_sum - spot_qty;

        // 3) limit BUY на споте с реплейсом
        let mut price = self.exchange.get_spot_price(&symbol).await?;
        let mut limit_price = price * (1.0 - self.slippage);
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Buy, spot_qty, limit_price)
            .await?;
        let mut start = Instant::now();

        loop {
            sleep(Duration::from_secs(1)).await;
            let status: OrderStatus = self.exchange.get_order_status(&symbol, &spot_order.id).await?;
            if status.remaining_qty <= 1e-8 {
                break;
            }
            if start.elapsed().as_secs() > self.max_wait {
                // отменяем и ставим новый лимитник
                let _ = self.exchange.cancel_order(&symbol, &spot_order.id).await;
                price = self.exchange.get_spot_price(&symbol).await?;
                limit_price = price * (1.0 - self.slippage);
                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Buy, spot_qty, limit_price)
                    .await?;
                start = Instant::now();
            }
        }

        // 4) рыночный SELL на фьюче
        let fut_order = self
            .exchange
            .place_market_order(&format!("{}USDT", symbol), OrderSide::Sell, spot_qty)
            .await?;

        Ok((spot_qty, spot_qty))
    }

    /// Цикл расхеджирования:
    /// mirror: limit‐SELL на споте с реплейсом → market‐BUY на фьюче
    pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
    ) -> Result<(f64, f64)> {
        let UnhedgeRequest { sum, symbol } = req;

        // 1) корректируем сумму
        let adj_sum = sum * (1.0 - self.commission);

        // 2) limit SELL на споте
        let mut price = self.exchange.get_spot_price(&symbol).await?;
        let mut limit_price = price * (1.0 + self.slippage);
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Sell, adj_sum, limit_price)
            .await?;
        let mut start = Instant::now();

        loop {
            sleep(Duration::from_secs(1)).await;
            let status = self.exchange.get_order_status(&symbol, &spot_order.id).await?;
            if status.remaining_qty <= 1e-8 {
                break;
            }
            if start.elapsed().as_secs() > self.max_wait {
                let _ = self.exchange.cancel_order(&symbol, &spot_order.id).await;
                price = self.exchange.get_spot_price(&symbol).await?;
                limit_price = price * (1.0 + self.slippage);
                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Sell, adj_sum, limit_price)
                    .await?;
                start = Instant::now();
            }
        }

        // 3) market BUY на фьюче
        let fut_order = self
            .exchange
            .place_market_order(&format!("{}USDT", symbol), OrderSide::Buy, adj_sum)
            .await?;

        Ok((adj_sum, adj_sum))
    }
}
