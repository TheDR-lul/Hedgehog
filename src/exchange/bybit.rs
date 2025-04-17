use super::Exchange;
use crate::exchange::types::*;
use anyhow::Result;

pub struct Bybit {
    api_key: String,
    api_secret: String,
}

impl Bybit {
    pub fn new(key: &str, secret: &str) -> Self {
        Self { api_key: key.to_string(), api_secret: secret.to_string() }
    }
}

#[async_trait::async_trait]
impl Exchange for Bybit {
    async fn check_connection(&mut self) -> Result<()> {
        Ok(())
    }
    async fn get_balance(&self, symbol: &str) -> Result<Balance> { todo!() }
    async fn get_mmr(&self, symbol: &str) -> Result<f64> { todo!() }
    async fn get_funding_rate(&self, symbol: &str, days: u16) -> Result<f64> { todo!() }
    async fn place_limit_order(&self, symbol: &str, side: OrderSide, qty: f64, price: f64) -> Result<Order> { todo!() }
    async fn place_market_order(&self, symbol: &str, side: OrderSide, qty: f64) -> Result<Order> { todo!() }
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()> { todo!() }
}
