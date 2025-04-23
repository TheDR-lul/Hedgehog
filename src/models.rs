// src/models.rs
use serde::Deserialize; // Добавим, если нужно будет сериализовать/десериализовать

/// Запрос на хеджирование
#[derive(Debug, Deserialize)] // Добавим Deserialize, если понадобится
pub struct HedgeRequest {
    pub sum: f64,
    pub symbol: String,
    pub volatility: f64,
}

/// Запрос на расхеджирование
// --- ИЗМЕНЕНО: sum -> quantity, добавлен Debug и Deserialize ---
#[derive(Debug, Deserialize)]
pub struct UnhedgeRequest {
    pub quantity: f64, // <-- Переименовано
    pub symbol: String,
}
// --- Конец изменений ---

/// Отчёт по исполненным ордерам (пока не используется)
#[derive(Debug)] // Добавим Debug
pub struct ExecutionReport {
    pub order_id: String,
    pub price: f64,
    pub qty: f64,
    pub timestamp: i64,
}
