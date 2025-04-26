// src/exchange/types.rs
use serde::Deserialize;
use std::fmt;
use std::str::FromStr; // Добавлено для FromStr

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Balance {
    pub free: f64,
    pub locked: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl fmt::Display for OrderSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OrderSide::Buy => write!(f, "Buy"),
            OrderSide::Sell => write!(f, "Sell"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Order {
    pub id: String,
    pub side: OrderSide,
    pub qty: f64,
    pub price: Option<f64>, // None для рыночных
    pub ts: i64, // Timestamp создания
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrderStatus {
    pub filled_qty: f64,
    pub remaining_qty: f64,
}

// --- ДОБАВЛЕНО: Enum для текстового статуса ордера ---
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderStatusText {
    New,
    PartiallyFilled,
    Filled,
    Cancelled,
    PartiallyFilledCanceled, // Добавлено для Bybit
    Rejected,
    Untriggered, // Для условных ордеров
    Triggered,   // Для условных ордеров
    Unknown(String), // Для неизвестных статусов
}

impl FromStr for OrderStatusText {
    type Err = (); // Ошибки парсинга не ожидаем, просто Unknown

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "New" => OrderStatusText::New,
            "PartiallyFilled" => OrderStatusText::PartiallyFilled,
            "Filled" => OrderStatusText::Filled,
            "Cancelled" | "Canceled" => OrderStatusText::Cancelled, // Учитываем оба варианта
            "PartiallyFilledCanceled" => OrderStatusText::PartiallyFilledCanceled,
            "Rejected" => OrderStatusText::Rejected,
            "Untriggered" => OrderStatusText::Untriggered,
            "Triggered" => OrderStatusText::Triggered,
            _ => OrderStatusText::Unknown(s.to_string()),
        })
    }
}
// --- КОНЕЦ ДОБАВЛЕНИЯ ---

#[derive(Debug, Clone, PartialEq)] // --- ИСПРАВЛЕНО: Убрали Copy ---
pub struct DetailedOrderStatus {
    pub filled_qty: f64,
    pub remaining_qty: f64,
    pub cumulative_executed_value: f64,
    pub average_price: f64,
    // --- ДОБАВЛЕНО: Поле для текстового статуса ---
    pub status_text: OrderStatusText,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeeRate {
    pub maker: f64,
    pub taker: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuturesTickerInfo {
    pub symbol: String,
    pub bid_price: f64,
    pub ask_price: f64,
    pub last_price: f64,
}

// --- ДОБАВЛЕНО: Структуры для информации об инструменте ---
#[derive(Deserialize, Debug, Clone)]
pub struct LotSizeFilter {
    #[serde(rename = "basePrecision")]
    pub base_precision: Option<String>,
    #[serde(rename = "qtyStep")]
    pub qty_step: Option<String>,
    #[serde(rename = "maxOrderQty")]
    pub max_order_qty: String,
    #[serde(rename = "minOrderQty")]
    pub min_order_qty: String,
    #[serde(rename = "maxMktOrderQty")]
    pub max_mkt_order_qty: Option<String>,
    #[serde(rename = "minNotionalValue")]
    pub min_notional_value: Option<String>,
    #[serde(rename = "postOnlyMaxOrderQty")]
    pub post_only_max_order_qty: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PriceFilter {
    #[serde(rename = "tickSize")]
    pub tick_size: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SpotInstrumentInfo {
    pub symbol: String, // Сделали pub
    #[serde(rename = "lotSizeFilter")]
    pub lot_size_filter: LotSizeFilter,
    #[serde(rename = "priceFilter")]
    pub price_filter: PriceFilter,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LinearInstrumentInfo {
    pub symbol: String, // Сделали pub
    #[serde(rename = "lotSizeFilter")]
    pub lot_size_filter: LotSizeFilter,
    #[serde(rename = "priceFilter")]
    pub price_filter: PriceFilter,
}
// --- КОНЕЦ ДОБАВЛЕНИЯ ---
