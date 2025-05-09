// src/models.rs
use serde::Deserialize;

/// Запрос на хеджирование
// ИЗМЕНЕНО: Добавлен Derive Clone
#[derive(Debug, Clone, Deserialize)] 
pub struct HedgeRequest {
    pub sum: f64,
    pub symbol: String,
    pub volatility: f64,
}

/// Запрос на расхеджирование
#[derive(Debug, Deserialize)] // Оставляем Clone здесь, если он был и нужен для UnhedgeRequest
pub struct UnhedgeRequest {
    pub quantity: f64, 
    pub symbol: String,
}

/// Отчёт по исполненным ордерам (пока не используется)
#[derive(Debug)] 
pub struct ExecutionReport {
    pub order_id: String,
    pub price: f64,
    pub qty: f64,
    pub timestamp: i64,
}