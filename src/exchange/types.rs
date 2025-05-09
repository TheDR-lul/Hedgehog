// src/exchange/types.rs
use serde::Deserialize;
use serde::Serialize; // --- ДОБАВЛЕНО: Импорт Serialize для сериализации в JSON ---
use std::fmt;
use std::str::FromStr;
use rust_decimal::Decimal;
use std::primitive::str;
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Balance {
    pub free: f64,
    pub locked: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)] // Добавили Serialize
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

// Реализация FromStr для OrderSide, если Bybit возвращает строки типа "Buy" / "Sell"
impl FromStr for OrderSide {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Buy" => Ok(OrderSide::Buy),
            "Sell" => Ok(OrderSide::Sell),
            _ => Err(anyhow::anyhow!("Invalid OrderSide string: {}", s)),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderStatusText {
    New,
    PartiallyFilled,
    Filled,
    Cancelled,
    PartiallyFilledCanceled,
    Rejected,
    Untriggered,
    Triggered,
    Unknown(String),
}

impl FromStr for OrderStatusText {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "New" => OrderStatusText::New,
            "PartiallyFilled" => OrderStatusText::PartiallyFilled,
            "Filled" => OrderStatusText::Filled,
            "Cancelled" | "Canceled" => OrderStatusText::Cancelled,
            "PartiallyFilledCanceled" => OrderStatusText::PartiallyFilledCanceled,
            "Rejected" => OrderStatusText::Rejected,
            "Untriggered" => OrderStatusText::Untriggered,
            "Triggered" => OrderStatusText::Triggered,
            _ => OrderStatusText::Unknown(s.to_string()),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DetailedOrderStatus {
    pub order_id: String, // --- ДОБАВЛЕНО ID ордера ---
    pub symbol: String,   // --- ДОБАВЛЕНО символ ---
    pub side: OrderSide,    // --- ДОБАВЛЕНО направление ---
    pub filled_qty: f64,
    pub remaining_qty: f64,
    pub cumulative_executed_value: f64,
    pub average_price: f64,
    pub status_text: OrderStatusText,
    pub last_filled_price: Option<f64>, // --- ДОБАВЛЕНО цена последнего исполнения ---
    pub last_filled_qty: Option<f64>,   // --- ДОБАВЛЕНО объем последнего исполнения ---
    pub reject_reason: Option<String>, // --- ДОБАВЛЕНО причина отклонения ---
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
    pub symbol: String,
    #[serde(rename = "lotSizeFilter")]
    pub lot_size_filter: LotSizeFilter,
    #[serde(rename = "priceFilter")]
    pub price_filter: PriceFilter,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LinearInstrumentInfo {
    pub symbol: String,
    #[serde(rename = "lotSizeFilter")]
    pub lot_size_filter: LotSizeFilter,
    #[serde(rename = "priceFilter")]
    pub price_filter: PriceFilter,
}

// --- ДОБАВЛЕНЫ ТИПЫ ДЛЯ WEBSOCKET ---

/// Типы подписок для WebSocket
#[derive(Debug, Clone, PartialEq, Eq, Hash)] // Добавили Hash
pub enum SubscriptionType {
    Order, // Приватные обновления по ордерам
    Orderbook { symbol: String, depth: u32 }, // Стакан L2
    PublicTrade { symbol: String }, // Публичные сделки
}

// --- ДОБАВЛЕНО: Структура для элемента стакана ---
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)] // Добавили Deserialize/Serialize
pub struct OrderbookLevel {
    #[serde(with = "rust_decimal::serde::str")] // Сериализуем/десериализуем как строку
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
}
// --- КОНЕЦ ДОБАВЛЕНИЯ ---

/// Сообщения, получаемые от WebSocket
#[derive(Debug, Clone, PartialEq)]
pub enum WebSocketMessage {
    // Приватные
    OrderUpdate(DetailedOrderStatus), // Используем обновленный DetailedOrderStatus
    Authenticated(bool), // Статус аутентификации

    // Публичные
    OrderBookL2 {
        symbol: String,
        bids: Vec<OrderbookLevel>, // Используем OrderbookLevel
        asks: Vec<OrderbookLevel>, // Используем OrderbookLevel
        is_snapshot: bool, // Флаг, снапшот это или дельта
    },
    PublicTrade {
        symbol: String,
        price: f64, // Используем f64 для простоты
        qty: f64,
        side: OrderSide,
        timestamp: i64, // Время сделки на бирже
    },

    // Системные/Мета
    Pong, // Ответ на наш Ping
    SubscriptionResponse { success: bool, topic: String }, // Ответ на подписку
    Error(String), // Ошибка сокета или парсинга
    Connected, // Событие установки соединения
    Disconnected, // Событие разрыва соединения
}

// --- КОНЕЦ ДОБАВЛЕНИЙ ДЛЯ WEBSOCKET ---