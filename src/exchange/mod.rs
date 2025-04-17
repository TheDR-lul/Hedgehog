// src/exchange/mod.rs

pub mod types;
pub mod bybit;

pub use bybit::Bybit;
pub use types::{Balance, OrderSide, Order};

use async_trait::async_trait;

#[async_trait]
pub trait Exchange {
    /// Проверить подключение к бирже
    async fn check_connection(&mut self) -> anyhow::Result<()>;

    /// Получить баланс по символу (например, "USDT")
    async fn get_balance(&self, symbol: &str) -> anyhow::Result<Balance>;

    /// Получить поддерживающую маржу (MMR) по символу
    async fn get_mmr(&self, symbol: &str) -> anyhow::Result<f64>;

    /// Получить среднюю ставку финансирования за заданное число дней
    async fn get_funding_rate(&self, symbol: &str, days: u16) -> anyhow::Result<f64>;

    /// Разместить лимитный ордер
    async fn place_limit_order(
        &self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
        price: f64,
    ) -> anyhow::Result<Order>;

    /// Разместить рыночный ордер
    async fn place_market_order(
        &self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
    ) -> anyhow::Result<Order>;

    /// Отменить ордер по ID
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> anyhow::Result<()>;
}
