// src/exchange/types.rs

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use anyhow::anyhow;


#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl fmt::Display for OrderSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl FromStr for OrderSide {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "buy"  => Ok(OrderSide::Buy),
            "sell" => Ok(OrderSide::Sell),
            other  => Err(anyhow!("Invalid order side: {}", other)),
        }
    }
}

// --- ДОБАВЛЕНО: Методы sign() и opposite() ---
impl OrderSide {
    pub fn sign(&self) -> f64 {
        match self {
            OrderSide::Buy => -1.0,
            OrderSide::Sell => 1.0,
        }
    }

    pub fn opposite(&self) -> Self {
        match self {
            OrderSide::Buy => OrderSide::Sell,
            OrderSide::Sell => OrderSide::Buy,
        }
    }
}


/// Описание ордера
// --- ИЗМЕНЕНО: Убрал OrderSide из derive, т.к. он теперь Copy ---
// --- Хотя можно и оставить, Clone все равно нужен ---
#[derive(Debug, Clone)] // OrderSide теперь Copy, так что Order остается Clone
pub struct Order {
    pub id:     String,
    pub side:   OrderSide, // Это поле теперь будет копироваться
    pub qty:    f64,
    pub price:  Option<f64>,
    pub ts:     i64,
}

/// Баланс монеты
#[derive(Debug, Clone)]
pub struct Balance {
    pub free:   f64,
    pub locked: f64,
}

/// Статус исполнения ордера (сколько уже исполнено и сколько осталось)
#[derive(Debug, Clone)]
pub struct OrderStatus {
    pub filled_qty:    f64,
    pub remaining_qty: f64,
}

#[derive(Debug, Clone)]
pub struct FuturesTickerInfo {
    pub symbol: String,
    pub bid_price: f64,
    pub ask_price: f64,
    pub last_price: f64,
}
