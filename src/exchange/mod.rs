// src/exchange/mod.rs

pub mod types;
pub mod bybit;

pub use bybit::Bybit;
// Импортируем типы из types и структуры информации об инструментах из bybit
pub use types::{Balance, OrderSide, Order, OrderStatus};
pub use bybit::{SpotInstrumentInfo, LinearInstrumentInfo}; // Добавили импорт для Info

use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug, Clone, Copy, Default)]
pub struct FeeRate {
    pub maker: f64,
    pub taker: f64,
}


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

    /// Получить ставки комиссии для символа (maker, taker)
    async fn get_fee_rate(&self, symbol: &str, category: &str) -> Result<FeeRate>;

    /// Разместить лимитный ордер
    async fn place_limit_order(
        &self,
        symbol: &str, // Base symbol (e.g., BTC)
        side: OrderSide,
        qty: f64,
        price: f64,
    ) -> Result<Order>;

    /// Разместить рыночный ордер на ФЬЮЧЕРСАХ
    async fn place_futures_market_order(
        &self,
        symbol: &str, // Full symbol (e.g., BTCUSDT)
        side: OrderSide,
        qty: f64,
    ) -> Result<Order>;

    /// Разместить рыночный ордер на СПОТЕ
    async fn place_spot_market_order(
        &self,
        symbol: &str, // Base symbol (e.g., BTC)
        side: OrderSide,
        qty: f64,
    ) -> Result<Order>;

    /// Отменить ордер
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()>;

    /// Получить текущую спотовую цену
    async fn get_spot_price(&self, symbol: &str) -> Result<f64>;

    /// Получить статус ордера (сколько исполнено и сколько осталось)
    async fn get_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus>;

    // --- ДОБАВЛЕНЫ НОВЫЕ МЕТОДЫ ---
    /// Получить текущее кредитное плечо для символа
    async fn get_current_leverage(&self, symbol: &str) -> Result<f64>;

    /// Установить кредитное плечо для символа
    async fn set_leverage(&self, symbol: &str, leverage: f64) -> Result<()>;
    // --- КОНЕЦ ДОБАВЛЕНИЯ ---
}