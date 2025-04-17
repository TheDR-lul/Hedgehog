// src/exchange/types.rs

/// Баланс по активу
#[derive(Debug)]
pub struct Balance {
    pub free: f64,
    pub locked: f64,
}

/// Направление ордера
#[derive(Debug)]
pub enum OrderSide {
    Buy,
    Sell,
}

/// Ответ от биржи после размещения ордера
#[derive(Debug)]
pub struct Order {
    pub id: String,
    pub filled: f64,
}
