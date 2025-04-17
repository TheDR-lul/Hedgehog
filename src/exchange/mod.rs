use async_trait::async_trait;
use crate::models::*;

#[async_trait]
pub trait Bybit {
    async fn check_connection(&mut self) -> anyhow::Result<()>;
    async fn get_balance(&self, symbol: &str) -> anyhow::Result<Balance>;
    async fn get_mmr(&self, symbol: &str) -> anyhow::Result<f64>;
    async fn get_funding_rate(&self, symbol: &str, days: u16) -> anyhow::Result<f64>;
    async fn place_limit_order(&self, symbol: &str, side: OrderSide, qty: f64, price: f64) -> anyhow::Result<Order>;
    async fn place_market_order(&self, symbol: &str, side: OrderSide, qty: f64) -> anyhow::Result<Order>;
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> anyhow::Result<()>;
}
