/// Запрос на хеджирование
#[derive(Debug)]
pub struct HedgeRequest {
    pub sum: f64,
    pub symbol: String,
    pub volatility: f64,
}

/// Запрос на расхеджирование
pub struct UnhedgeRequest {
    pub sum: f64,
    pub symbol: String,
}

/// Отчёт по исполненным ордерам (пока не используется)
pub struct ExecutionReport {
    pub order_id: String,
    pub price: f64,
    pub qty: f64,
    pub timestamp: i64,
}
