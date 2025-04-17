// src/hedger.rs

use crate::exchange::Exchange;
use crate::models::HedgeRequest;
use anyhow::Result;

/// Основная логика хеджирования
pub struct Hedger<E: Exchange> {
    exchange: E,
}

impl<E: Exchange + Clone + Send + Sync> Hedger<E> {
    pub fn new(exchange: E) -> Self {
        Self { exchange }
    }

    /// Рассчитывает объёмы для хеджирования и возвращает (spot_qty, futures_qty)
    pub async fn run_hedge(&self, req: HedgeRequest) -> Result<(f64, f64)> {
        // Получаем поддерживающую маржу (MMR)
        let mmr = self.exchange.get_mmr(&req.symbol).await?;
        // Вычисляем объёмы
        let spot_qty = req.sum / ((1.0 + req.volatility) * (1.0 + mmr));
        let futures_qty = req.sum - spot_qty;
        Ok((spot_qty, futures_qty))
    }
}