// src/exchange/types.rs

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use anyhow::{Result, anyhow};

/// Сторона ордера
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

/// Описание ордера
#[derive(Debug, Clone)]
pub struct Order {
    pub id:     String,
    pub side:   OrderSide,
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
