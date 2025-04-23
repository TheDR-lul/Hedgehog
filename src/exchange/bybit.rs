// src/exchange/bybit.rs

// --- Используемые типажи и структуры ---
use super::{Exchange, OrderStatus};
use crate::exchange::types::{Balance, Order, OrderSide};
// --- Стандартные и внешние зависимости ---
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::{Client, Method};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::str::FromStr; // Для Decimal::from_str
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{debug, error, info, trace, warn}; // Логирование
use uuid::Uuid;
// --- Зависимости для форматирования чисел ---
use rust_decimal::prelude::*;
use rust_decimal_macros::dec; // Нужен для макроса dec!

type HmacSha256 = Hmac<Sha256>;

// --- Константы для категорий ---
const SPOT_CATEGORY: &str = "spot";
const LINEAR_CATEGORY: &str = "linear";

// --- Структуры для парсинга ответов API ---

/// Универсальная обёртка для ответов Bybit API v5 (без дженерика)
#[derive(Deserialize, Debug)]
struct ApiResponse {
    #[serde(rename = "retCode")]
    ret_code: i32,
    #[serde(rename = "retMsg")]
    ret_msg: String,
    #[serde(rename = "result")]
    result_raw: Value, // Парсим result как необработанное Value
    #[serde(rename = "time")]
    _server_time: Option<i64>,
}

/// Пустой результат для запросов без данных (например, отмена ордера)
#[derive(Deserialize, Debug, Default)] // Уже есть Default
struct EmptyResult {}

/// Ответ по балансу
#[derive(Deserialize, Debug, Default)] // <-- Добавлен Default
struct BalanceResult {
    #[serde(rename = "list")]
    list: Vec<UnifiedAccountBalance>,
}

#[derive(Deserialize, Debug)]
struct UnifiedAccountBalance {
    #[serde(rename = "accountType")]
    account_type: String,
    #[serde(rename = "coin")]
    coins: Vec<BalanceEntry>,
}

#[derive(Deserialize, Debug)]
struct BalanceEntry {
    #[serde(rename = "coin")]
    coin: String,
    #[serde(rename = "walletBalance")]
    wallet: String, // Используем для проверки наличия актива
    #[serde(rename = "locked")]
    locked: String,
    #[serde(rename = "availableToWithdraw")]
    available: String, // Используем для расчета free
}

// --- Структуры для информации об инструменте (Общие фильтры) ---
#[derive(Deserialize, Debug, Clone)]
pub struct LotSizeFilter {
    // --- ИЗМЕНЕНО: Используем base_precision ---
    #[serde(rename = "basePrecision")]
    pub base_precision: String,
    // --- Конец изменений ---
    #[serde(rename = "maxOrderQty")]
    pub max_order_qty: String,
    #[serde(rename = "minOrderQty")]
    pub min_order_qty: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PriceFilter {
    #[serde(rename = "tickSize")]
    pub tick_size: String,
}

// --- Структуры для информации об инструменте СПОТ ---
#[derive(Deserialize, Debug, Clone, Default)] // <-- Добавлен Default
struct SpotInstrumentsInfoResult {
    list: Vec<SpotInstrumentInfo>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SpotInstrumentInfo {
    symbol: String,
    #[serde(rename = "lotSizeFilter")]
    pub lot_size_filter: LotSizeFilter,
    #[serde(rename = "priceFilter")]
    pub price_filter: PriceFilter,
}

// --- Структуры для информации об инструменте ЛИНЕЙНОМ ---
#[derive(Deserialize, Debug, Clone, Default)] // <-- Добавлен Default
struct LinearInstrumentsInfoResult {
    list: Vec<LinearInstrumentInfo>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LinearInstrumentInfo {
    symbol: String,
    #[serde(rename = "lotSizeFilter")]
    pub lot_size_filter: LotSizeFilter,
    #[serde(rename = "priceFilter")]
    pub price_filter: PriceFilter,
}

// --- Структуры для risk-limit (для MMR) ---
#[derive(Deserialize, Debug, Default)] // <-- Добавлен Default
struct RiskLimitResult {
    category: String,
    list: Vec<RiskLimitEntry>,
}

#[derive(Deserialize, Debug)]
struct RiskLimitEntry {
    id: u64,
    symbol: String,
    #[serde(rename = "riskLimitValue")]
    _risk_limit_value: String,
    #[serde(rename = "maintenanceMargin")]
    mmr: String,
    #[serde(rename = "initialMargin")]
    _initial_margin: String,
    #[serde(rename = "isLowestRisk")]
    is_lowest_risk: u8,
    #[serde(rename = "maxLeverage")]
    _max_leverage: String,
}

/// Ответ по funding‑rate
#[derive(Deserialize, Debug, Default)] // <-- Добавлен Default
struct FundingResult {
    list: Vec<FundingEntry>,
}

#[derive(Deserialize, Debug)]
struct FundingEntry {
    #[serde(rename = "fundingRate")]
    rate: String,
}

/// Ответ при создании ордера
#[derive(Deserialize, Debug, Default)] // <-- Добавлен Default
struct OrderCreateResult {
    #[serde(rename = "orderId")]
    id: String,
    #[serde(rename = "orderLinkId")]
    link_id: String,
}

/// Ответ при запросе статуса ордера
#[derive(Deserialize, Debug, Default)] // <-- Добавлен Default
struct OrderQueryResult {
    list: Vec<OrderQueryEntry>,
}

#[derive(Deserialize, Debug)]
struct OrderQueryEntry {
    #[serde(rename = "orderId")]
    id: String,
    #[serde(rename = "symbol")]
    _symbol: String,
    #[serde(rename = "side")]
    _side: String,
    #[serde(rename = "orderStatus")]
    status: String,
    #[serde(rename = "cumExecQty")]
    cum_exec_qty: String,
    #[serde(rename = "cumExecValue")]
    _cum_exec_value: String,
    #[serde(rename = "leavesQty")]
    leaves_qty: String,
    #[serde(rename = "price")]
    _price: String,
    #[serde(rename = "avgPrice")]
    _avg_price: String,
    #[serde(rename = "createdTime")]
    _created_time: String,
}

/// Ответ по тикерам
#[derive(Deserialize, Debug, Default)] // <-- Добавлен Default
struct TickersResult {
    category: String,
    list: Vec<TickerInfo>,
}

#[derive(Deserialize, Debug)]
struct TickerInfo {
    symbol: String,
    #[serde(rename = "lastPrice")]
    price: String,
}

/// Ответ по времени сервера
#[derive(Deserialize, Debug, Default)] // <-- Добавлен Default
struct ServerTimeResult {
    #[serde(rename = "timeNano")]
    time_nano: String,
    #[serde(rename = "timeSecond")]
    _time_second: String,
}

/// Клиент Bybit
#[derive(Debug, Clone)]
pub struct Bybit {
    api_key: String,
    api_secret: String,
    client: Client,
    base_url: String,
    recv_window: u64,
    quote_currency: String,
    time_offset_ms: Arc<Mutex<Option<i64>>>,
    balance_cache: Arc<Mutex<Option<(Vec<(String, Balance)>, SystemTime)>>>,
}

impl Bybit {
    /// Создаёт новый экземпляр клиента и синхронизирует время
    pub async fn new(key: &str, secret: &str, base_url: &str, quote_currency: &str) -> Result<Self> {
        info!(base_url, quote_currency, "Initializing Bybit client...");
        if !base_url.starts_with("http") {
            error!("Invalid base URL provided: {}", base_url);
            return Err(anyhow!("Invalid base URL"));
        }
        if quote_currency.is_empty() {
            error!("Quote currency cannot be empty");
            return Err(anyhow!("Quote currency cannot be empty"));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        let instance = Self {
            api_key: key.into(),
            api_secret: secret.into(),
            client,
            base_url: base_url.trim_end_matches('/').into(),
            recv_window: 5_000,
            quote_currency: quote_currency.to_uppercase(),
            time_offset_ms: Arc::new(Mutex::new(None)),
            balance_cache: Arc::new(Mutex::new(None)),
        };

        if let Err(e) = instance.sync_time().await {
            error!("Initial time sync failed: {}. Authentication might fail later.", e);
        } else {
            info!("Initial time sync successful.");
        }

        info!("Bybit client initialized successfully.");
        Ok(instance)
    }

    /// Формирует полный URL эндпоинта
    fn url(&self, ep: &str) -> String {
        format!("{}/{}", self.base_url, ep.trim_start_matches('/'))
    }

    /// Формирует символ пары (например, BTC + USDT -> BTCUSDT)
    fn format_pair(&self, base_symbol: &str) -> String {
        format!("{}{}", base_symbol.to_uppercase(), self.quote_currency)
    }

    /// Синхронизация времени с сервером
    async fn sync_time(&self) -> Result<()> {
        let url = self.url("v5/market/time");
        debug!(%url, "Syncing server time");

        let res = self.client.get(&url).send().await
            .map_err(|e| {
                error!("Failed to send time sync request: {}", e);
                anyhow!("Failed to send time sync request: {}", e)
            })?;

        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_else(|_| "Failed to read body".to_string());
            error!(%status, %body, "Time sync request failed with non-success status");
            return Err(anyhow!("Failed to sync server time: status {}", status));
        }

        // Используем временную структуру для парсинга
        #[derive(Deserialize)]
        struct TempTimeResponse {
            #[serde(rename = "retCode")] ret_code: i32,
            #[serde(rename = "retMsg")] ret_msg: String,
            result: ServerTimeResult,
        }

        let parsed: TempTimeResponse = res.json().await
            .map_err(|e| {
                error!("Failed to parse time sync response JSON: {}", e);
                anyhow!("Failed to parse time sync response: {}", e)
            })?;

        if parsed.ret_code != 0 {
             error!(code=parsed.ret_code, msg=%parsed.ret_msg, "Bybit API Error during time sync");
             return Err(anyhow!("Bybit API Error during time sync ({}): {}", parsed.ret_code, parsed.ret_msg));
        }

        let server_time_ms = match parsed.result.time_nano.parse::<u128>() {
            Ok(t) => t / 1_000_000,
            Err(e) => {
                error!("Failed to parse server time nano '{}': {}", parsed.result.time_nano, e);
                return Err(anyhow!("Failed to parse server time nano: {}", e));
            }
        };

        let local_time_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
             Ok(d) => d.as_millis(),
             Err(e) => {
                 error!("System time error: {}", e);
                 return Err(anyhow!("System time error: {}", e));
             }
        };

        let offset = server_time_ms as i64 - local_time_ms as i64;
        info!(offset_ms = offset, "Server time synced.");

        let mut time_offset_guard = self.time_offset_ms.lock().await;
        *time_offset_guard = Some(offset);

        Ok(())
    }

    /// Получение скорректированной временной метки в миллисекундах
    async fn get_timestamp_ms(&self) -> Result<i64> {
        let local_time_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
             Ok(d) => d.as_millis() as i64,
             Err(e) => {
                 error!("System time error: {}", e);
                 return Err(anyhow!("System time error: {}", e));
             }
        };

        let time_offset_guard = self.time_offset_ms.lock().await;
        let offset = (*time_offset_guard).ok_or_else(|| {
            error!("Time offset is not available. Cannot generate timestamp.");
            anyhow!("Time not synchronized with server")
        })?;

        Ok(local_time_ms + offset)
    }

    /// Генерация подписи
    fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Формирование заголовков аутентификации
    async fn auth_headers(&self, qs: &str, body: &str) -> Result<Vec<(&str, String)>> {
        let timestamp_ms = self.get_timestamp_ms().await?;
        let t = timestamp_ms.to_string();
        let rw = self.recv_window.to_string();
        let payload_str = format!("{}{}{}{}", t, self.api_key, rw, if !qs.is_empty() { qs } else { body });
        let sign = self.sign(&payload_str);
        trace!(payload=%payload_str, "Generated signature payload");

        Ok(vec![
            ("X-BAPI-API-KEY", self.api_key.clone()),
            ("X-BAPI-TIMESTAMP", t),
            ("X-BAPI-RECV-WINDOW", rw),
            ("X-BAPI-SIGN", sign),
            ("Content-Type", "application/json".to_string()),
        ])
    }

    /// Универсальный вызов Bybit API
    async fn call_api<T: for<'de> Deserialize<'de> + Default>( // Добавляем Default для T
        &self,
        method: Method,
        endpoint: &str,
        query: Option<&[(&str, &str)]>,
        body: Option<Value>,
        auth: bool,
    ) -> Result<T> {
        let url = self.url(endpoint);
        debug!(%url, method=%method, ?query, ?body, auth, "Bybit API Call ->");

        let mut req = self.client.request(method.clone(), &url);

        let qs = if let Some(q) = query {
            let encoded_query = q.iter()
                .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            req = req.query(q);
            encoded_query
        } else {
            String::new()
        };

        let body_str = if let Some(ref b) = body {
            match serde_json::to_string(b) {
                Ok(s) => {
                    req = req.json(b);
                    s
                }
                Err(e) => {
                    error!("Failed to serialize request body: {}", e);
                    return Err(anyhow!("Failed to serialize request body: {}", e));
                }
            }
        } else {
            String::new()
        };

        if auth {
            match self.auth_headers(&qs, &body_str).await {
                Ok(headers) => {
                    for (h, v) in headers {
                        req = req.header(h, v);
                    }
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                error!(%url, error=%e, "Request failed");
                return Err(anyhow!("Request failed to {}: {}", url, e));
            }
        };
        let status = resp.status();
        let raw_body = match resp.text().await {
             Ok(text) => text,
             Err(e) => {
                 error!(%url, %status, error=%e, "Failed to read response body");
                 return Err(anyhow!("Failed to read response body from {}: {}", url, e));
             }
        };
        debug!(%url, %status, body_len=raw_body.len(), "Bybit API Response <-");

        if !status.is_success() || tracing::enabled!(tracing::Level::TRACE) {
             if !status.is_success() {
                 info!(%url, %status, response_body=%raw_body, "Non-success HTTP status received");
             } else {
                 trace!(%url, %status, response_body=%raw_body, "Full Bybit Response Body");
             }
        }

        // Сначала парсим как Value, проверяем retCode, потом парсим result
        let json_value: Value = match serde_json::from_str(&raw_body) {
             Ok(v) => v,
             Err(e) => {
                 error!(error=%e, raw_body, "Failed to parse response body as JSON");
                 return Err(anyhow!("Failed to parse response body as JSON: {}", e));
             }
        };

        let ret_code = json_value.get("retCode").and_then(Value::as_i64).unwrap_or(-1);
        let ret_msg = json_value.get("retMsg").and_then(Value::as_str).unwrap_or("Unknown error");

        if ret_code != 0 {
            error!(code = ret_code, msg = ret_msg, %url, "Bybit API Error");
            return Err(anyhow!("Bybit API Error ({}): {}. Raw: {}", ret_code, ret_msg, raw_body));
        }

        // Если retCode == 0, пытаемся извлечь и распарсить поле result в тип T
        match json_value.get("result") {
            Some(result_val) => {
                match serde_json::from_value(result_val.clone()) {
                    Ok(result_data) => {
                        debug!(%url, "Bybit API call successful (retCode=0)");
                        Ok(result_data)
                    }
                    Err(e) => {
                        // Если парсинг result не удался, но retCode=0, это может быть EmptyResult
                        if result_val.is_object() && result_val.as_object().map_or(false, |obj| obj.is_empty()) {
                             warn!("'result' field is an empty object, attempting to return default value for type.");
                             Ok(T::default()) // Используем Default для T
                        } else {
                            error!(error=%e, result_value=?result_val, "Failed to deserialize 'result' field into expected type");
                            Err(anyhow!("Failed to deserialize 'result' field: {}", e))
                        }
                    }
                }
            }
            None => {
                // Если 'result' отсутствует, но retCode=0, это тоже может быть EmptyResult
                warn!("'result' field missing in successful Bybit response, returning default value for type.");
                Ok(T::default()) // Используем Default для T
            }
        }
    }
}

#[async_trait]
impl Exchange for Bybit {
    /// Проверка соединения
    async fn check_connection(&mut self) -> Result<()> {
        info!("Checking Bybit connection...");
        if let Err(e) = self.sync_time().await {
            error!("Time sync failed during check_connection: {}", e);
        }
        // Указываем тип T явно, т.к. call_api его требует
        self.call_api::<ServerTimeResult>(Method::GET, "v5/market/time", None, None, false).await?;
        info!("Connection check successful (ping OK).");
        Ok(())
    }

    /// Получение баланса для конкретной монеты
    async fn get_balance(&self, coin: &str) -> Result<Balance> {
        debug!(%coin, "Getting balance for specific coin");
        let all_balances = self.get_all_balances().await?;
        all_balances.into_iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(coin))
            .map(|(_, b)| b)
            .ok_or_else(|| {
                warn!("No balance entry found for {}", coin);
                anyhow!("No balance entry found for {}", coin)
            })
    }

    /// Получение всех балансов (с кэшированием)
    async fn get_all_balances(&self) -> Result<Vec<(String, Balance)>> {
        let cache_duration = Duration::from_secs(5);
        let now = SystemTime::now();
        debug!("Attempting to get all balances (cache duration: {:?})", cache_duration);

        let mut cache_guard = self.balance_cache.lock().await;

        if let Some((cached_balances, timestamp)) = &*cache_guard {
            match now.duration_since(*timestamp) {
                Ok(age) if age < cache_duration => {
                    info!("Returning cached balances (age: {:?}).", age);
                    return Ok(cached_balances.clone());
                }
                Ok(age) => {
                    info!("Balance cache expired (age: {:?}).", age);
                }
                Err(e) => {
                    warn!("System time error checking cache age: {}. Fetching fresh balances.", e);
                }
            }
        } else {
            info!("Balance cache is empty.");
        }

        info!("Fetching fresh balances from API...");
        let res: BalanceResult = self.call_api(
            Method::GET,
            "v5/account/wallet-balance",
            Some(&[("accountType", "UNIFIED")]),
            None,
            true,
        ).await?;

        let account = res.list.into_iter()
            .find(|acc| acc.account_type == "UNIFIED")
            .ok_or_else(|| {
                error!("UNIFIED account type not found in balance response");
                anyhow!("UNIFIED account type not found in balance response")
            })?;

        let mut balances = Vec::new();
        for entry in account.coins {
            let total = if entry.wallet.is_empty() { 0.0 } else {
                entry.wallet.parse::<f64>().map_err(|e| {
                    error!("Failed to parse wallet balance for {}: {} (value: '{}')", entry.coin, e, entry.wallet);
                    anyhow!("Failed to parse wallet balance for {}: {}", entry.coin, e)
                })?
            };
            let locked = if entry.locked.is_empty() { 0.0 } else {
                entry.locked.parse::<f64>().map_err(|e| {
                    error!("Failed to parse locked balance for {}: {} (value: '{}')", entry.coin, e, entry.locked);
                    anyhow!("Failed to parse locked balance for {}: {}", entry.coin, e)
                })?
            };

            let free = if !entry.available.is_empty() {
                 entry.available.parse::<f64>().map_err(|e| {
                    error!("Failed to parse available balance for {}: {} (value: '{}')", entry.coin, e, entry.available);
                    anyhow!("Failed to parse available balance for {}: {}", entry.coin, e)
                })?
            } else {
                (total - locked).max(0.0)
            };

            if total > 1e-9 {
                 debug!(coin=%entry.coin, total, free, locked, "Parsed balance entry");
                 balances.push((entry.coin, Balance { free, locked }));
            } else {
                 trace!(coin=%entry.coin, total, "Skipping asset with zero or negligible total balance");
            }
        }

        *cache_guard = Some((balances.clone(), now));
        info!("Balances updated and cached ({} assets).", balances.len());

        Ok(balances)
    }

    /// Получить информацию об инструменте СПОТ (для точности цены/кол-ва)
    async fn get_spot_instrument_info(&self, symbol: &str) -> Result<SpotInstrumentInfo> {
        let spot_pair = self.format_pair(symbol);
        debug!(symbol=%spot_pair, category=SPOT_CATEGORY, "Fetching spot instrument info");

        let params = [("category", SPOT_CATEGORY), ("symbol", &spot_pair)];
        let info_result: SpotInstrumentsInfoResult = self.call_api(
            Method::GET,
            "v5/market/instruments-info",
            Some(&params),
            None,
            false,
        ).await?;

        info_result.list.into_iter()
            .find(|i| i.symbol == spot_pair)
            .ok_or_else(|| {
                error!("Spot instrument info not found for {}", spot_pair);
                anyhow!("Spot instrument info not found for {}", spot_pair)
            })
    }

    /// Получить информацию об инструменте ЛИНЕЙНОМ (для точности кол-ва)
    async fn get_linear_instrument_info(&self, symbol: &str) -> Result<LinearInstrumentInfo> {
        debug!(%symbol, category=LINEAR_CATEGORY, "Fetching linear instrument info");

        let params = [("category", LINEAR_CATEGORY), ("symbol", symbol)];
        let info_result: LinearInstrumentsInfoResult = self.call_api(
            Method::GET,
            "v5/market/instruments-info",
            Some(&params),
            None,
            false,
        ).await?;

        info_result.list.into_iter()
            .find(|i| i.symbol == symbol)
            .ok_or_else(|| {
                error!("Linear instrument info not found for {}", symbol);
                anyhow!("Linear instrument info not found for {}", symbol)
            })
    }


    /// Размещение лимитного ордера (для СПОТА)
    async fn place_limit_order(
        &self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
        price: f64,
    ) -> Result<Order> {
        let spot_pair = self.format_pair(symbol);

        let instrument_info = self.get_spot_instrument_info(symbol).await
            .map_err(|e| anyhow!("Failed to get instrument info for {}: {}", symbol, e))?;
        let tick_size_str = &instrument_info.price_filter.tick_size;
        let base_precision_str = &instrument_info.lot_size_filter.base_precision;
        let min_order_qty_str = &instrument_info.lot_size_filter.min_order_qty;

        let price_decimals = tick_size_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        debug!("Using precision for {}: price_decimals={}", spot_pair, price_decimals);

        let formatted_price = match Decimal::from_f64(price) {
            Some(d) => d.round_dp(price_decimals).to_string(),
            None => {
                error!("Failed to convert price {} to Decimal", price);
                return Err(anyhow!("Invalid price value {}", price));
            }
        };

        let qty_decimals = base_precision_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        debug!("Using precision for {}: qty_decimals={}", spot_pair, qty_decimals);

        let formatted_qty = match Decimal::from_f64(qty) {
             (Some(qty_d)) if qty_d > dec!(0.0) => {
                 let rounded_down = qty_d.trunc_with_scale(qty_decimals);
                 let min_qty = Decimal::from_str(min_order_qty_str).unwrap_or(dec!(0.0));
                 if rounded_down < min_qty && qty_d >= min_qty {
                     warn!("Quantity {} rounded down below min_order_qty {}. Using min_order_qty.", qty, min_order_qty_str);
                     min_qty.normalize().to_string()
                 } else if rounded_down <= dec!(0.0) {
                     error!("Requested quantity {} is less than the minimum precision {} for {}", qty, base_precision_str, spot_pair);
                     return Err(anyhow!("Quantity {} is less than minimum precision {}", qty, base_precision_str));
                 } else {
                     rounded_down.normalize().to_string()
                 }
             }
             (Some(qty_d)) if qty_d == dec!(0.0) => {
                 "0".to_string()
             }
             _ => {
                 error!("Failed to convert qty {} to Decimal or invalid qty_step", qty);
                 return Err(anyhow!("Invalid qty value {}", qty));
             }
        };

        if formatted_qty == "0" {
             error!("Formatted quantity is zero for symbol {}. Aborting order.", spot_pair);
             return Err(anyhow!("Formatted quantity is zero"));
        }

        info!(symbol=%spot_pair, %side, %formatted_qty, %formatted_price, category=SPOT_CATEGORY, "Placing SPOT limit order");

        let body = json!({
            "category": SPOT_CATEGORY,
            "symbol": spot_pair,
            "side": side.to_string(),
            "orderType": "Limit",
            "qty": formatted_qty,
            "price": formatted_price,
            "timeInForce": "GTC",
            "orderLinkId": format!("spot-limit-{}", Uuid::new_v4()),
        });

        let result: OrderCreateResult = self
            .call_api(Method::POST, "v5/order/create", None, Some(body), true)
            .await?;

        info!(order_id=%result.id, link_id=%result.link_id, "SPOT limit order placed successfully");

        Ok(Order {
            id: result.id,
            side,
            qty,
            price: Some(price),
            ts: self.get_timestamp_ms().await?,
        })
    }

    /// Размещение рыночного ордера (для ФЬЮЧЕРСОВ)
    async fn place_market_order(
        &self,
        symbol: &str, // Full futures symbol (e.g., BTCUSDT)
        side: OrderSide,
        qty: f64,
    ) -> Result<Order> {

        let instrument_info = self.get_linear_instrument_info(symbol).await
             .map_err(|e| anyhow!("Failed to get instrument info for {}: {}", symbol, e))?;
        let base_precision_str = &instrument_info.lot_size_filter.base_precision;
        let min_order_qty_str = &instrument_info.lot_size_filter.min_order_qty;

        let qty_decimals = base_precision_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        debug!("Using precision for {}: qty_decimals={}", symbol, qty_decimals);

        let formatted_qty = match Decimal::from_f64(qty) {
             (Some(qty_d)) if qty_d > dec!(0.0) => {
                 let rounded_down = qty_d.trunc_with_scale(qty_decimals);
                 let min_qty = Decimal::from_str(min_order_qty_str).unwrap_or(dec!(0.0));
                 if rounded_down < min_qty && qty_d >= min_qty {
                     warn!("Quantity {} rounded down below min_order_qty {}. Using min_order_qty.", qty, min_order_qty_str);
                     min_qty.normalize().to_string()
                 } else if rounded_down <= dec!(0.0) {
                     error!("Requested quantity {} is less than the minimum precision {} for {}", qty, base_precision_str, symbol);
                     return Err(anyhow!("Quantity {} is less than minimum precision {}", qty, base_precision_str));
                 } else {
                     rounded_down.normalize().to_string()
                 }
             }
             (Some(qty_d)) if qty_d == dec!(0.0) => {
                 "0".to_string()
             }
             _ => {
                 error!("Failed to convert qty {} to Decimal or invalid qty_step", qty);
                 return Err(anyhow!("Invalid qty value {}", qty));
             }
        };
        if formatted_qty == "0" {
             error!("Formatted quantity is zero for symbol {}. Aborting order.", symbol);
             return Err(anyhow!("Formatted quantity is zero"));
        }

        info!(%symbol, %side, %formatted_qty, category=LINEAR_CATEGORY, "Placing FUTURES market order");

        let body = json!({
            "category": LINEAR_CATEGORY,
            "symbol": symbol,
            "side": side.to_string(),
            "orderType": "Market",
            "qty": formatted_qty, // Используем отформатированное значение
            "orderLinkId": format!("fut-market-{}", Uuid::new_v4()),
        });

        let result: OrderCreateResult = self
            .call_api(Method::POST, "v5/order/create", None, Some(body), true)
            .await?;

        info!(order_id=%result.id, link_id=%result.link_id, "FUTURES market order placed successfully");

        Ok(Order {
            id: result.id,
            side,
            qty,
            price: None,
            ts: self.get_timestamp_ms().await?,
        })
    }

    /// Отмена ордера (предполагаем СПОТ)
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()> {
        let spot_pair = self.format_pair(symbol);
        info!(symbol=%spot_pair, order_id, category=SPOT_CATEGORY, "Cancelling SPOT order");

        let body = json!({
            "category": SPOT_CATEGORY,
            "symbol": spot_pair,
            "orderId": order_id,
        });

        // Ожидаем EmptyResult, call_api обработает случай отсутствия 'result'
        let _: EmptyResult = self
            .call_api(Method::POST, "v5/order/cancel", None, Some(body), true)
            .await?;

        info!(order_id, "SPOT Order cancelled successfully");
        Ok(())
    }

    /// Получение спотовой цены
    async fn get_spot_price(&self, symbol: &str) -> Result<f64> {
        let spot_pair = self.format_pair(symbol);
        debug!(symbol=%spot_pair, category=SPOT_CATEGORY, "Fetching spot price");

        let params = [("category", SPOT_CATEGORY), ("symbol", &spot_pair)];
        let tickers_result: TickersResult = self.call_api(
                Method::GET,
                "v5/market/tickers",
                Some(&params),
                None,
                false,
            )
            .await?;

        let ticker = tickers_result.list.into_iter()
             .find(|t| t.symbol == spot_pair)
             .ok_or_else(|| {
                 warn!("No ticker info found for {}", spot_pair);
                 anyhow!("No ticker info found for {}", spot_pair)
             })?;

        ticker.price.parse::<f64>()
             .map_err(|e| {
                 error!("Failed to parse spot price for {}: {} (value: '{}')", spot_pair, e, ticker.price);
                 anyhow!("Failed to parse spot price for {}: {}", spot_pair, e)
             })
    }

    /// Получение статуса ордера (предполагаем СПОТ)
    async fn get_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus> {
        let spot_pair = self.format_pair(symbol);
        debug!(symbol=%spot_pair, order_id, category=SPOT_CATEGORY, "Fetching SPOT order status");

        let params = [
            ("category", SPOT_CATEGORY),
            ("orderId", order_id),
        ];
        let query_result: OrderQueryResult = self
            .call_api(
                Method::GET,
                "v5/order/realtime",
                Some(&params),
                None,
                true,
            )
            .await?;

        let order_entry = query_result.list.into_iter().next()
            .ok_or_else(|| {
                warn!("Order ID {} not found in realtime query", order_id);
                anyhow!("Order ID {} not found in realtime query", order_id)
            })?;

        let filled = if order_entry.cum_exec_qty.is_empty() { 0.0 } else {
            order_entry.cum_exec_qty.parse::<f64>().map_err(|e| {
                error!("Failed to parse filled qty for order {}: {} (value: '{}')", order_id, e, order_entry.cum_exec_qty);
                anyhow!("Failed to parse filled qty for order {}: {}", order_id, e)
            })?
        };
        let remaining = if order_entry.leaves_qty.is_empty() { 0.0 } else {
            order_entry.leaves_qty.parse::<f64>().map_err(|e| {
                error!("Failed to parse remaining qty for order {}: {} (value: '{}')", order_id, e, order_entry.leaves_qty);
                anyhow!("Failed to parse remaining qty for order {}: {}", order_id, e)
            })?
        };

        info!(order_id, status=%order_entry.status, filled, remaining, "Order status received");

        Ok(OrderStatus {
            filled_qty: filled,
            remaining_qty: remaining,
        })
    }

    /// Получение MMR для ЛИНЕЙНОГО контракта (используя Risk Limit)
    async fn get_mmr(&self, symbol: &str) -> Result<f64> {
        let linear_pair = self.format_pair(symbol);
        debug!(symbol=%linear_pair, category=LINEAR_CATEGORY, "Fetching MMR via risk limit");

        let params = [("category", LINEAR_CATEGORY), ("symbol", &linear_pair)];
        trace!("Attempting to fetch risk limit for {}", linear_pair);
        let risk_limit_result: RiskLimitResult = self.call_api(
            Method::GET,
            "v5/market/risk-limit",
            Some(&params),
            None,
            false,
        ).await?;
        trace!(?risk_limit_result, "Received risk limit data");

        let risk_level = risk_limit_result.list.into_iter()
            .find(|level| level.id == 1 || level.is_lowest_risk == 1)
            .ok_or_else(|| {
                warn!("No risk limit level 1 found for {}", linear_pair);
                anyhow!("No risk limit level 1 found for {}", linear_pair)
            })?;
        trace!(?risk_level, "Found risk level 1 data");

        if risk_level.mmr.is_empty() {
            if self.base_url.contains("testnet") {
                let fallback_mmr = 0.005;
                warn!(
                    "MMR (maintenanceMargin) is empty in TESTNET response for {}. Using fallback value: {}. THIS IS A WORKAROUND!",
                    linear_pair, fallback_mmr
                );
                Ok(fallback_mmr)
            } else {
                error!(
                    "MMR (maintenanceMargin) is empty in API response for {}. Cannot proceed.",
                    linear_pair
                );
                Err(anyhow!("MMR (maintenanceMargin) is empty for {}", linear_pair))
            }
        } else {
            risk_level.mmr.parse::<f64>().map_err(|e| {
                error!("Failed to parse MMR for {}: {} (value: '{}')", linear_pair, e, risk_level.mmr);
                anyhow!("Failed to parse MMR for {}: {}", linear_pair, e)
            })
        }
    }

    /// Получение средней ставки финансирования для ЛИНЕЙНОГО контракта
    async fn get_funding_rate(&self, symbol: &str, days: u16) -> Result<f64> {
        let linear_pair = self.format_pair(symbol);
        let limit = days.min(200).to_string();
        debug!(symbol=%linear_pair, days, limit=%limit, "Fetching funding rate history");

        let params = [
            ("category", LINEAR_CATEGORY),
            ("symbol", &linear_pair),
            ("limit", limit.as_str()),
        ];
        let funding_result: FundingResult = self
            .call_api(
                Method::GET,
                "v5/market/funding-rate-history",
                Some(&params),
                None,
                false,
            )
            .await?;

        if funding_result.list.is_empty() {
            warn!("No funding rate history found for {}", linear_pair);
            return Ok(0.0);
        }

        let mut sum = 0.0;
        let mut count = 0;
        for entry in &funding_result.list {
            if entry.rate.is_empty() {
                warn!("Received empty funding rate for {}", linear_pair);
                continue;
            }
            match entry.rate.parse::<f64>() {
                Ok(rate) => {
                    sum += rate;
                    count += 1;
                }
                Err(e) => {
                    warn!("Failed to parse funding rate '{}' for {}: {}", entry.rate, linear_pair, e);
                }
            }
        }

        if count == 0 {
            warn!("Could not parse any funding rates for {}", linear_pair);
            Ok(0.0)
        } else {
            let avg_rate = sum / count as f64;
            info!(symbol=%linear_pair, days, average_rate=avg_rate, "Calculated average funding rate");
            Ok(avg_rate)
        }
    }
}
