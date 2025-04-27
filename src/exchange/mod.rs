// src/exchange/mod.rs
use anyhow::Result;
use async_trait::async_trait;
use crate::storage::HedgeOperation;
// --- ИСПРАВЛЕНО: Импортируем типы отсюда ---
use crate::exchange::types::{
    Balance, Order, OrderSide, OrderStatus, FeeRate, FuturesTickerInfo, DetailedOrderStatus,
    SpotInstrumentInfo, LinearInstrumentInfo // Добавили InstrumentInfo
};
// --- УДАЛЕНО: use crate::hedger::params::{SpotInstrumentInfo, LinearInstrumentInfo}; ---
#[async_trait]
pub trait Exchange: Send + Sync {
    async fn check_connection(&mut self) -> Result<()>;
    async fn get_balance(&self, coin: &str) -> Result<Balance>;
    async fn get_all_balances(&self) -> Result<Vec<(String, Balance)>>;
    // --- ИСПРАВЛЕНО: Используем типы из types.rs ---
    async fn get_spot_instrument_info(&self, symbol: &str) -> Result<SpotInstrumentInfo>;
    async fn get_linear_instrument_info(&self, symbol: &str) -> Result<LinearInstrumentInfo>;
    async fn get_fee_rate(&self, symbol: &str, category: &str) -> Result<FeeRate>;
    async fn place_limit_order(&self, symbol: &str, side: OrderSide, qty: f64, price: f64) -> Result<Order>;
    async fn place_futures_market_order(&self, symbol: &str, side: OrderSide, qty: f64) -> Result<Order>;
    async fn place_futures_limit_order(&self, symbol: &str, side: OrderSide, qty: f64, price: f64) -> Result<Order>;
    async fn place_spot_market_order(&self, symbol: &str, side: OrderSide, qty: f64) -> Result<Order>;
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()>; // Deprecated
    async fn get_spot_price(&self, symbol: &str) -> Result<f64>;
    async fn get_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus>; // Deprecated
    async fn get_mmr(&self, symbol: &str) -> Result<f64>;
    async fn get_funding_rate(&self, symbol: &str, days: u16) -> Result<f64>;
    async fn get_current_leverage(&self, symbol: &str) -> Result<f64>;
    async fn set_leverage(&self, symbol: &str, leverage: f64) -> Result<()>;
    async fn cancel_spot_order(&self, symbol: &str, order_id: &str) -> Result<()>;
    async fn cancel_futures_order(&self, symbol: &str, order_id: &str) -> Result<()>;
    async fn get_spot_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus>;
    async fn get_futures_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus>;
    async fn get_spot_order_execution_details(&self, symbol: &str, order_id: &str) -> Result<DetailedOrderStatus>;
    async fn get_futures_ticker(&self, symbol: &str) -> Result<FuturesTickerInfo>;
    async fn get_market_price(&self, symbol: &str, is_spot: bool) -> Result<f64>;
    async fn get_spot_price_fallback(&self, futures_symbol: &str) -> Result<f64>;
}

pub mod bybit;
pub mod types;
pub mod bybit_ws; // <-- Изменяем объявление на модуль

// Функция для создания экземпляра биржи
pub async fn create_exchange(
    api_key: &str,
    api_secret: &str,
    base_url: &str,
    quote_currency: &str,
) -> Result<Box<dyn Exchange>> {
    // Пока только Bybit
    let bybit_client = bybit::Bybit::new(api_key, api_secret, base_url, quote_currency).await?;
    Ok(Box::new(bybit_client))
}
