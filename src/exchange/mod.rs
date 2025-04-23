// src/exchange/mod.rs

pub mod types;
pub mod bybit;

pub use bybit::Bybit;
// --- ИСПРАВЛЕНО: Импортируем LinearInstrumentInfo из bybit ---
pub use types::{Balance, OrderSide, Order};
pub use bybit::{SpotInstrumentInfo, LinearInstrumentInfo}; // <-- Исправлено здесь
// --- Конец исправления ---

use anyhow::Result;
use async_trait::async_trait;

/// Статус исполнения ордера (если понадобится в других модулях)
pub use types::OrderStatus;

#[async_trait]
pub trait Exchange {
    /// Проверить подключение к бирже
    async fn check_connection(&mut self) -> Result<()>;

    /// Баланс по символу (например, "USDT")
    async fn get_balance(&self, symbol: &str) -> Result<Balance>;

    /// Все балансы
    async fn get_all_balances(&self) -> Result<Vec<(String, Balance)>>;

    /// Получить информацию об инструменте СПОТ (для точности цены/кол-ва)
    async fn get_spot_instrument_info(&self, symbol: &str) -> Result<SpotInstrumentInfo>;

    /// Получить информацию об инструменте ЛИНЕЙНОМ (для точности кол-ва)
    async fn get_linear_instrument_info(&self, symbol: &str) -> Result<LinearInstrumentInfo>;

    /// Поддерживающая маржа (MMR)
    async fn get_mmr(&self, symbol: &str) -> Result<f64>;

    /// Средняя ставка финансирования за N дней
    async fn get_funding_rate(&self, symbol: &str, days: u16) -> Result<f64>;

    /// Разместить лимитный ордер
    async fn place_limit_order(
        &self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
        price: f64,
    ) -> Result<Order>;

    /// Разместить рыночный ордер
    async fn place_market_order(
        &self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
    ) -> Result<Order>;

    /// Отменить ордер
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()>;

    /// Получить текущую спотовую цену
    async fn get_spot_price(&self, symbol: &str) -> Result<f64>;

    /// Получить статус ордера (сколько исполнено и сколько осталось)
    async fn get_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus>;
}