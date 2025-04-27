use super::{Exchange, OrderStatus, FeeRate, FuturesTickerInfo};
// --- ИСПРАВЛЕНО: Импортируем все нужные типы из types.rs ---
use crate::exchange::types::{
    Balance, Order, OrderSide, DetailedOrderStatus, OrderStatusText,
    SpotInstrumentInfo, LinearInstrumentInfo // Оставляем InstrumentInfo
};
// --- ИСПРАВЛЕНО: Добавляем импорт HedgeOperation из storage ---
// --- Стандартные и внешние зависимости ---
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{debug, error, info, trace, warn};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;

type HmacSha256 = Hmac<Sha256>;

pub const SPOT_CATEGORY: &str = "spot";
pub const LINEAR_CATEGORY: &str = "linear";
/// Универсальная обёртка для ответов Bybit API v5
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
/// Пустой результат для запросов без данных
#[derive(Deserialize, Debug, Default)]
struct EmptyResult {}

/// Ответ по балансу
#[derive(Deserialize, Debug, Default)]
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
    wallet: String,
    #[serde(rename = "locked")]
    locked: String,
    #[serde(rename = "availableToWithdraw")]
    available: String,
}

// --- Структуры для информации об инструменте СПОТ ---
#[derive(Deserialize, Debug, Clone, Default)]
struct SpotInstrumentsInfoResult {
    list: Vec<SpotInstrumentInfo>, // Используем импортированный тип
}

// --- Структуры для информации об инструменте ЛИНЕЙНОМ ---
#[derive(Deserialize, Debug, Clone, Default)]
struct LinearInstrumentsInfoResult {
    list: Vec<LinearInstrumentInfo>, // Используем импортированный тип
}

// --- Структуры для risk-limit (для MMR) ---
#[derive(Deserialize, Debug, Default)]
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
#[derive(Deserialize, Debug, Default)]
struct FundingResult {
    list: Vec<FundingEntry>,
}

#[derive(Deserialize, Debug)]
struct FundingEntry {
    #[serde(rename = "fundingRate")]
    rate: String,
}

/// Ответ при создании ордера
#[derive(Deserialize, Debug, Default)]
struct OrderCreateResult {
    #[serde(rename = "orderId")]
    id: String,
    #[serde(rename = "orderLinkId")]
    link_id: String,
}

/// Ответ при запросе статуса ордера
#[derive(Deserialize, Debug, Default)]
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
    cum_exec_value: String,
    #[serde(rename = "avgPrice")]
    avg_price: String,
    #[serde(rename = "leavesQty")]
    leaves_qty: String,
    #[serde(rename = "price")]
    _price: String,
    #[serde(rename = "createdTime")]
    _created_time: String,
}

/// Ответ по тикерам (общий)
#[derive(Deserialize,Serialize ,Debug, Default)]
struct TickersResult {
    category: String,
    list: Vec<TickerInfo>,
}

#[derive(Deserialize,Serialize, Debug)]
struct TickerInfo {
    symbol: String,
    #[serde(rename = "lastPrice")]
    price: String,
}

// Структуры для фьючерсного тикера
#[derive(Deserialize, Debug)]
struct FuturesTickerApiResponse {
    symbol: String,
    #[serde(rename = "lastPrice")]
    last_price: String,
    #[serde(rename = "bid1Price")]
    bid1_price: String,
    #[serde(rename = "ask1Price")]
    ask1_price: String,
}

#[derive(Deserialize, Debug, Default)]
struct FuturesTickersApiResult {
    category: String,
    list: Vec<FuturesTickerApiResponse>,
}

/// Ответ по времени сервера
#[derive(Deserialize, Debug, Default)]
struct ServerTimeResult {
    #[serde(rename = "timeNano")]
    time_nano: String,
    #[serde(rename = "timeSecond")]
    _time_second: String,
}

/// Ответ по комиссии
#[derive(Deserialize, Debug, Default)]
struct FeeRateResult {
    list: Vec<FeeRateEntry>,
}

#[derive(Deserialize, Debug)]
struct FeeRateEntry {
    symbol: String,
    #[serde(rename = "takerFeeRate")]
    taker_fee_rate: String,
    #[serde(rename = "makerFeeRate")]
    maker_fee_rate: String,
}

/// Ответ по информации о позиции (для плеча)
#[derive(Deserialize, Debug, Default)]
struct PositionInfoResult {
    list: Vec<PositionEntry>,
}

#[derive(Deserialize, Debug)]
struct PositionEntry {
    symbol: String,
    leverage: String,
    #[serde(rename = "positionIdx", default)]
    _position_idx: i32,
    #[serde(rename = "riskId", default)]
    _risk_id: u64,
    #[serde(rename = "riskLimitValue", default)]
    _risk_limit_value: String,
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
    async fn call_api<T: for<'de> Deserialize<'de> + Default>(
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
                    debug!(request_body=%s, "Serialized request body");
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

        let json_value: Value = match serde_json::from_str(&raw_body) {
             Ok(v) => v,
             Err(e) => {
                 error!(error=%e, raw_body, "Failed to parse response body as JSON");
                 return Err(anyhow!("Failed to parse response body as JSON: {}", e));
             }
        };

        let ret_code = json_value.get("retCode").and_then(Value::as_i64).unwrap_or(-1);
        let ret_msg = json_value.get("retMsg").and_then(Value::as_str).unwrap_or("Unknown error");

        // Обработка ошибок API
        if ret_code != 0 {
            // Игнорируем ошибки "Order not found", "already filled/canceled", "leverage not modified"
            if (endpoint.contains("cancel") || endpoint.contains("realtime"))
               && (ret_code == 110025 // Order not found or finished
                   || ret_code == 10001 // Parameter error (может быть, если ордер уже отменен)
                   || ret_code == 170106 // Order is already cancelled
                   || ret_code == 170213 // Order does not exist (Bybit Testnet)
               ) || (endpoint.contains("set-leverage") && ret_code == 110043) // Leverage not modified
            {
                 warn!(code = ret_code, msg = ret_msg, %url, "Bybit API Warning (Order not found/filled/canceled or Leverage not modified - ignoring)");
                 if endpoint.contains("realtime") {
                     // Для realtime возвращаем ошибку, чтобы hedger мог ее обработать
                     return Err(anyhow!("Order not found")); // Используем стандартное сообщение
                 }
                 // Для отмены или установки плеча без изменений просто продолжим, вернув Ok с default
            } else {
                error!(code = ret_code, msg = ret_msg, %url, "Bybit API Error");
                return Err(anyhow!("Bybit API Error ({}): {}. Raw: {}", ret_code, ret_msg, raw_body));
            }
        }


        match json_value.get("result") {
            Some(result_val) => {
                match serde_json::from_value(result_val.clone()) {
                    Ok(result_data) => {
                        debug!(%url, "Bybit API call successful (retCode=0)");
                        Ok(result_data)
                    }
                    Err(e) => {
                        if result_val.is_object() && result_val.as_object().map_or(false, |obj| obj.is_empty()) {
                             warn!("'result' field is an empty object, attempting to return default value for type.");
                             Ok(T::default())
                        } else {
                            error!(error=%e, result_value=?result_val, "Failed to deserialize 'result' field into expected type");
                            Err(anyhow!("Failed to deserialize 'result' field: {}", e))
                        }
                    }
                }
            }
            None => {
                // Если поле 'result' отсутствует, но retCode = 0 (например, при set-leverage),
                // возвращаем значение по умолчанию (например, EmptyResult {})
                warn!("'result' field missing in successful Bybit response, returning default value for type.");
                Ok(T::default())
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
        debug!("Attempting to get all balances...");

        let mut cache_guard = self.balance_cache.lock().await;

        if let Some((cached_balances, timestamp)) = &*cache_guard {
            if let Ok(age) = now.duration_since(*timestamp) {
                if age < cache_duration { info!("Returning cached balances."); return Ok(cached_balances.clone()); }
                else { info!("Balance cache expired."); }
            } else { warn!("System time error checking cache. Fetching fresh."); }
        } else { info!("Balance cache empty."); }

        info!("Fetching fresh balances from API...");
        let res: BalanceResult = self.call_api(Method::GET, "v5/account/wallet-balance", Some(&[("accountType", "UNIFIED")]), None, true).await?;

        let account = res.list.into_iter().find(|acc| acc.account_type == "UNIFIED").ok_or_else(|| anyhow!("UNIFIED account type not found"))?;

        let mut balances = Vec::new();
        for entry in account.coins {
            let total = entry.wallet.parse::<f64>().unwrap_or(0.0);
            let locked = entry.locked.parse::<f64>().unwrap_or(0.0);
            let free = (total - locked).max(0.0);

            if entry.coin.eq_ignore_ascii_case("BTC") || entry.coin.eq_ignore_ascii_case(&self.quote_currency) {
                debug!(
                    "Raw Balance Entry: Coin={}, Wallet='{}', Locked='{}', Available='{}' -> Parsed: Total={}, Free={}, Locked={}",
                    entry.coin, entry.wallet, entry.locked, entry.available,
                    total, free, locked
                );
            }
            if total > 1e-9 || free > 1e-9 || locked > 1e-9 {
                 balances.push((entry.coin, Balance { free, locked }));
            } else {
                 trace!(coin=%entry.coin, total, "Skipping zero balance asset");
            }
        }

        *cache_guard = Some((balances.clone(), now));
        info!("Balances updated and cached ({} assets).", balances.len());

        Ok(balances)
    }

    /// Получить информацию об инструменте СПОТ
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

    /// Получить информацию об инструменте ЛИНЕЙНОМ
    async fn get_linear_instrument_info(&self, symbol: &str) -> Result<LinearInstrumentInfo> {
        let linear_pair = self.format_pair(symbol);
        debug!(symbol=%linear_pair, category=LINEAR_CATEGORY, "Fetching linear instrument info");

        let params = [("category", LINEAR_CATEGORY), ("symbol", &linear_pair)];
        let info_result: LinearInstrumentsInfoResult = self.call_api(
            Method::GET,
            "v5/market/instruments-info",
            Some(&params),
            None,
            false,
        ).await?;

        info_result.list.into_iter()
            .find(|i| i.symbol == linear_pair)
            .ok_or_else(|| {
                error!("Linear instrument info not found for {}", linear_pair);
                anyhow!("Linear instrument info not found for {}", linear_pair)
            })
    }

    /// Получить ставки комиссии
    async fn get_fee_rate(&self, symbol: &str, category: &str) -> Result<FeeRate> {
        let pair = match category {
            SPOT_CATEGORY => self.format_pair(symbol),
            LINEAR_CATEGORY => self.format_pair(symbol),
            _ => return Err(anyhow!("Unsupported category for fee rate: {}", category)),
        };
        debug!(%pair, %category, "Fetching fee rate");

        let params = [("symbol", pair.as_str())];

        let fee_result: FeeRateResult = self.call_api(
            Method::GET,
            "v5/account/fee-rate",
            Some(&params),
            None,
            true,
        ).await?;

        let entry = fee_result.list.into_iter()
            .find(|e| e.symbol == pair)
            .ok_or_else(|| {
                warn!("Fee rate entry not found for {} in category {}", pair, category);
                anyhow!("Fee rate entry not found for {}/{}", category, pair)
            })?;

        let maker = entry.maker_fee_rate.parse::<f64>().map_err(|e| {
            error!("Failed to parse maker fee rate for {}: {} (value: '{}')", pair, e, entry.maker_fee_rate);
            anyhow!("Failed to parse maker fee rate for {}: {}", pair, e)
        })?;
        let taker = entry.taker_fee_rate.parse::<f64>().map_err(|e| {
            error!("Failed to parse taker fee rate for {}: {} (value: '{}')", pair, e, entry.taker_fee_rate);
            anyhow!("Failed to parse taker fee rate for {}: {}", pair, e)
        })?;

        info!(%pair, %category, maker, taker, "Fee rate received");
        Ok(FeeRate { maker, taker })
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
        let base_precision_str = instrument_info.lot_size_filter.base_precision
            .as_deref()
            .ok_or_else(|| anyhow!("Missing basePrecision for spot symbol {}", spot_pair))?;
        let min_order_qty_str = &instrument_info.lot_size_filter.min_order_qty;

        let price_decimals = tick_size_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        let formatted_price = match Decimal::from_f64(price) {
            Some(d) => d.round_dp(price_decimals).to_string(),
            None => return Err(anyhow!("Invalid price value {}", price)),
        };

        let qty_decimals = base_precision_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        let formatted_qty = match Decimal::from_f64(qty) {
             Some(qty_d) if qty_d > dec!(0.0) => {
                 let rounded_down = qty_d.trunc_with_scale(qty_decimals);
                 let min_qty = Decimal::from_str(min_order_qty_str).unwrap_or(dec!(0.0));
                 if rounded_down < min_qty && qty_d >= min_qty {
                     warn!("Limit order quantity {} rounded down below min_order_qty {}. Using min_order_qty.", qty, min_order_qty_str);
                     min_qty.normalize().to_string()
                 } else if rounded_down <= dec!(0.0) {
                     error!("Requested quantity {} is less than the minimum precision {} for {}", qty, base_precision_str, spot_pair);
                     return Err(anyhow!("Quantity {} is less than minimum precision {}", qty, base_precision_str));
                 } else {
                     rounded_down.normalize().to_string()
                 }
             }
             _ => return Err(anyhow!("Invalid qty value {}", qty)),
        };

        if formatted_qty == "0" { return Err(anyhow!("Formatted quantity is zero")); }

        info!(symbol=%spot_pair, %side, %formatted_qty, %formatted_price, category=SPOT_CATEGORY, "Placing SPOT limit order");
        let body = json!({ "category": SPOT_CATEGORY, "symbol": spot_pair, "side": side.to_string(), "orderType": "Limit", "qty": formatted_qty, "price": formatted_price, "timeInForce": "GTC" });
        let result: OrderCreateResult = self.call_api(Method::POST, "v5/order/create", None, Some(body), true).await?;
        info!(order_id=%result.id, "SPOT limit order placed successfully");
        Ok(Order { id: result.id, side, qty, price: Some(price), ts: self.get_timestamp_ms().await? })
    }

    /// Размещение рыночного ордера (для ФЬЮЧЕРСОВ)
    async fn place_futures_market_order(
        &self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
    ) -> Result<Order> {
        let base_symbol = symbol.trim_end_matches(&self.quote_currency);
        if base_symbol.is_empty() || base_symbol == symbol { return Err(anyhow!("Invalid futures symbol format: {}", symbol)); }

        let instrument_info = self.get_linear_instrument_info(base_symbol).await?;
        let qty_step_str = instrument_info.lot_size_filter.qty_step.as_deref().ok_or_else(|| anyhow!("Missing qtyStep for linear symbol {}", symbol))?;
        let min_order_qty_str = &instrument_info.lot_size_filter.min_order_qty;

        let qty_decimals = qty_step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        let formatted_qty = match Decimal::from_f64(qty) {
            Some(qty_d) if qty_d > dec!(0.0) => {
                let rounded_down = qty_d.trunc_with_scale(qty_decimals);
                let min_qty = Decimal::from_str(min_order_qty_str).unwrap_or(dec!(0.0));
                if rounded_down < min_qty && qty_d >= min_qty {
                    warn!("Futures market order quantity {} rounded down below min_order_qty {}. Using min_order_qty.", qty, min_order_qty_str);
                    min_qty.normalize().to_string()
                } else if rounded_down <= dec!(0.0) {
                    error!("Requested futures quantity {} is less than the minimum precision {} (qtyStep) for {}", qty, qty_step_str, symbol);
                    return Err(anyhow!("Quantity {} is less than minimum precision {} (qtyStep)", qty, qty_step_str));
                } else {
                    rounded_down.normalize().to_string()
                }
            }
            _ => return Err(anyhow!("Invalid qty value {}", qty)),
        };
        if formatted_qty == "0" { return Err(anyhow!("Formatted quantity is zero")); }

        info!(
            target: "bybit_futures_order",
            symbol=%symbol,
            side=%side,
            original_qty=qty,
            formatted_qty=%formatted_qty,
            category=LINEAR_CATEGORY,
            "Preparing FUTURES market order parameters"
        );

        let body = json!({
            "category": LINEAR_CATEGORY,
            "symbol": symbol,
            "side": side.to_string(),
            "orderType": "Market",
            "qty": formatted_qty
        });

        let body_string = serde_json::to_string(&body).unwrap_or_else(|_| "Failed to serialize body".to_string());
        info!(
            target: "bybit_futures_order",
            request_body=%body_string,
            "Sending FUTURES market order request"
        );

        let result: OrderCreateResult = self.call_api(
            Method::POST,
            "v5/order/create",
            None,
            Some(body),
            true
        ).await?;

        info!(
            target: "bybit_futures_order",
            order_id = %result.id,
            "FUTURES market order placed successfully: order_id={}", result.id
        );

        Ok(Order { id: result.id, side, qty, price: None, ts: self.get_timestamp_ms().await? })
    }

    /// Размещение ЛИМИТНОГО ордера (для ФЬЮЧЕРСОВ)
    async fn place_futures_limit_order( &self, symbol: &str, side: OrderSide, qty: f64, price: f64 ) -> Result<Order> {
        let base_symbol = symbol.trim_end_matches(&self.quote_currency);
        if base_symbol.is_empty() || base_symbol == symbol { return Err(anyhow!("Invalid futures symbol format: {}", symbol)); }

        let instrument_info = self.get_linear_instrument_info(base_symbol).await?;
        let qty_step_str = instrument_info.lot_size_filter.qty_step.as_deref().ok_or_else(|| anyhow!("Missing qtyStep for linear symbol {}", symbol))?;
        let min_order_qty_str = &instrument_info.lot_size_filter.min_order_qty;
        let tick_size_str = &instrument_info.price_filter.tick_size;

        let qty_decimals = qty_step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        let formatted_qty = match Decimal::from_f64(qty) {
             Some(qty_d) if qty_d > dec!(0.0) => {
                 let rounded_down = qty_d.trunc_with_scale(qty_decimals);
                 let min_qty = Decimal::from_str(min_order_qty_str).unwrap_or(dec!(0.0));
                 if rounded_down < min_qty && qty_d >= min_qty {
                     warn!("Futures limit order quantity {} rounded down below min_order_qty {}. Using min_order_qty.", qty, min_order_qty_str);
                     min_qty.normalize().to_string()
                 } else if rounded_down <= dec!(0.0) {
                     error!("Requested futures quantity {} is less than the minimum precision {} (qtyStep) for {}", qty, qty_step_str, symbol);
                     return Err(anyhow!("Quantity {} is less than minimum precision {} (qtyStep)", qty, qty_step_str));
                 } else {
                     rounded_down.normalize().to_string()
                 }
             }
             _ => return Err(anyhow!("Invalid qty value {}", qty)),
        };
        if formatted_qty == "0" { return Err(anyhow!("Formatted quantity is zero")); }

        let price_decimals = tick_size_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        let formatted_price = match Decimal::from_f64(price) {
            Some(d) => d.round_dp(price_decimals).to_string(),
            None => return Err(anyhow!("Invalid price value {}", price)),
        };

        info!(
            target: "bybit_futures_order", symbol=%symbol, side=%side, order_type="Limit",
            original_qty=qty, formatted_qty=%formatted_qty, original_price=price,
            formatted_price=%formatted_price, category=LINEAR_CATEGORY,
            "Preparing FUTURES limit order parameters"
        );

        let body = json!({
            "category": LINEAR_CATEGORY, "symbol": symbol, "side": side.to_string(),
            "orderType": "Limit", "qty": formatted_qty, "price": formatted_price,
            "timeInForce": "GTC"
        });

        let body_string = serde_json::to_string(&body).unwrap_or_else(|_| "Failed to serialize body".to_string());
        info!(
            target: "bybit_futures_order", request_body=%body_string,
            "Sending FUTURES limit order request"
        );

        let result: OrderCreateResult = self.call_api(
            Method::POST, "v5/order/create", None, Some(body), true
        ).await?;

        info!(
            target: "bybit_futures_order", order_id = %result.id,
            "FUTURES limit order placed successfully: order_id={}", result.id
        );
        Ok(Order { id: result.id, side, qty, price: Some(price), ts: self.get_timestamp_ms().await? })
    }


    /// Размещение рыночного ордера (для СПОТА)
    async fn place_spot_market_order(
        &self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
    ) -> Result<Order> {
        let spot_pair = self.format_pair(symbol);

        let instrument_info = self.get_spot_instrument_info(symbol).await?;
        let base_precision_str = instrument_info.lot_size_filter.base_precision.as_deref().ok_or_else(|| anyhow!("Missing basePrecision for spot symbol {}", spot_pair))?;
        let min_order_qty_str = &instrument_info.lot_size_filter.min_order_qty;

        let qty_decimals = base_precision_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        let formatted_qty = match Decimal::from_f64(qty) {
             Some(qty_d) if qty_d > dec!(0.0) => {
                 let rounded_down = qty_d.trunc_with_scale(qty_decimals);
                 let min_qty = Decimal::from_str(min_order_qty_str).unwrap_or(dec!(0.0));
                 if rounded_down < min_qty && qty_d >= min_qty {
                     warn!("Spot market order quantity {} rounded down below min_order_qty {}. Using min_order_qty.", qty, min_order_qty_str);
                     min_qty.normalize().to_string()
                 } else if rounded_down <= dec!(0.0) {
                     return Err(anyhow!("Quantity {} is less than minimum precision {}", qty, base_precision_str));
                 } else {
                     rounded_down.normalize().to_string()
                 }
             }
             _ => return Err(anyhow!("Invalid qty value {}", qty)),
        };

        if formatted_qty == "0" { return Err(anyhow!("Formatted quantity is zero")); }

        info!(symbol=%spot_pair, %side, %formatted_qty, category=SPOT_CATEGORY, "Placing SPOT market order");

        let body = if side == OrderSide::Sell {
            json!({ "category": SPOT_CATEGORY, "symbol": spot_pair, "side": side.to_string(), "orderType": "Market", "qty": formatted_qty })
        } else {
             error!("Spot Market Buy by base quantity not supported. Use limit order or quote quantity.");
            return Err(anyhow!("Spot Market Buy by base quantity not supported"));
        };

        let result: OrderCreateResult = self.call_api(Method::POST, "v5/order/create", None, Some(body), true).await?;
        info!(order_id=%result.id, "SPOT market order placed successfully");
        Ok(Order { id: result.id, side, qty, price: None, ts: self.get_timestamp_ms().await? })
    }

    /// Отмена ордера (устаревший, используйте cancel_spot_order или cancel_futures_order)
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()> {
        warn!("Deprecated cancel_order called. Assuming SPOT order.");
        self.cancel_spot_order(symbol, order_id).await
    }

    /// Получение спотовой цены
    async fn get_spot_price(&self, symbol: &str) -> Result<f64> {
        let spot_pair = self.format_pair(symbol);
        debug!(symbol=%spot_pair, category=SPOT_CATEGORY, "Fetching spot price");
        let params = [("category", SPOT_CATEGORY), ("symbol", &spot_pair)];
        let tickers_result: TickersResult = self.call_api(Method::GET, "v5/market/tickers", Some(&params), None, false).await?;
        let ticker = tickers_result.list.into_iter().find(|t| t.symbol == spot_pair).ok_or_else(|| anyhow!("No ticker info found for {}", spot_pair))?;
        ticker.price.parse::<f64>().map_err(|e| anyhow!("Failed to parse spot price for {}: {}", spot_pair, e))
    }

    /// Получение статуса ордера (устаревший, используйте get_spot_order_status или get_futures_order_status)
    async fn get_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus> {
        warn!("Deprecated get_order_status called. Assuming SPOT order.");
        self.get_spot_order_status(symbol, order_id).await
    }

    /// Получение MMR для ЛИНЕЙНОГО контракта
    async fn get_mmr(&self, symbol: &str) -> Result<f64> {
        debug!(symbol=%symbol, category=LINEAR_CATEGORY, "Fetching MMR via risk limit");
        let params = [("category", LINEAR_CATEGORY), ("symbol", symbol)];
        let risk_limit_result: RiskLimitResult = self.call_api(Method::GET, "v5/market/risk-limit", Some(&params), None, false).await?;
        let risk_level = risk_limit_result.list.into_iter().find(|level| level.id == 1 || level.is_lowest_risk == 1).ok_or_else(|| anyhow!("No risk limit level 1 found for {}", symbol))?;

        if risk_level.mmr.is_empty() {
            if self.base_url.contains("testnet") {
                 warn!("MMR is empty for {} on testnet, using fallback 0.005", symbol);
                 Ok(0.005)
            } else { Err(anyhow!("MMR (maintenanceMargin) is empty for {}", symbol)) }
        } else {
            risk_level.mmr.parse::<f64>().map_err(|e| anyhow!("Failed to parse MMR for {}: {}", symbol, e))
        }
    }

    /// Получение средней ставки финансирования
    async fn get_funding_rate(&self, symbol: &str, days: u16) -> Result<f64> {
        let limit = days.min(200).to_string();
        debug!(symbol=%symbol, days, limit=%limit, "Fetching funding rate history");
        let params = [("category", LINEAR_CATEGORY), ("symbol", symbol), ("limit", limit.as_str())];
        let funding_result: FundingResult = self.call_api(Method::GET, "v5/market/funding-rate-history", Some(&params), None, false).await?;

        if funding_result.list.is_empty() { return Ok(0.0); }
        let mut sum = 0.0; let mut count = 0;
        for entry in &funding_result.list {
            if let Ok(rate) = entry.rate.parse::<f64>() { sum += rate; count += 1; }
        }
        if count == 0 { Ok(0.0) } else { Ok(sum / count as f64) }
    }

    /// Получить текущее кредитное плечо для символа (linear)
    async fn get_current_leverage(&self, symbol: &str) -> Result<f64> {
        info!(symbol=%symbol, category=LINEAR_CATEGORY, "Fetching current leverage");
        let params = [("category", LINEAR_CATEGORY), ("symbol", symbol)];
        let position_result: PositionInfoResult = self.call_api(
            Method::GET,
            "v5/position/list",
            Some(&params),
            None,
            true,
        ).await?;

        let position = match position_result.list.into_iter().find(|p| p.symbol == symbol) {
             Some(p) => p,
             None => {
                 warn!("No position info found for {} to get leverage. Cannot determine current leverage.", symbol);
                 return Err(anyhow!("Current leverage info not available for {}", symbol));
             }
        };

        position.leverage.parse::<f64>().map_err(|e| {
            error!("Failed to parse current leverage for {}: {} (value: '{}')", symbol, e, position.leverage);
            anyhow!("Failed to parse current leverage for {}: {}", symbol, e)
        })
    }

    /// Установить кредитное плечо для символа (linear)
    async fn set_leverage(&self, symbol: &str, leverage: f64) -> Result<()> {
        let leverage_str = format!("{:.2}", leverage);
        if leverage <= 0.0 {
            error!("Attempted to set non-positive leverage: {}", leverage);
            return Err(anyhow!("Leverage must be positive"));
        }
        info!(symbol=%symbol, leverage=%leverage_str, category=LINEAR_CATEGORY, "Setting leverage");

        let body = json!({
            "category": LINEAR_CATEGORY,
            "symbol": symbol,
            "buyLeverage": leverage_str,
            "sellLeverage": leverage_str,
            "tradeMode": 0,
        });

        self.call_api::<EmptyResult>(
            Method::POST,
            "v5/position/set-leverage",
            None,
            Some(body),
            true,
        ).await?;

        info!(symbol=%symbol, leverage=%leverage_str, "Set leverage request sent successfully (or leverage was already set to this value)");
        Ok(())
    }

    /// Отмена СПОТ ордера
    async fn cancel_spot_order(&self, symbol: &str, order_id: &str) -> Result<()> {
        let spot_pair = self.format_pair(symbol);
        info!(symbol=%spot_pair, order_id, category=SPOT_CATEGORY, "Cancelling SPOT order");
        let body = json!({ "category": SPOT_CATEGORY, "symbol": spot_pair, "orderId": order_id });
        self.call_api::<EmptyResult>(Method::POST, "v5/order/cancel", None, Some(body), true).await?;
        info!(order_id, "SPOT Order cancel request sent (or order was inactive)");
        Ok(())
    }

    /// Отмена ФЬЮЧЕРСНОГО ордера
    async fn cancel_futures_order(&self, symbol: &str, order_id: &str) -> Result<()> {
        info!(symbol=%symbol, order_id, category=LINEAR_CATEGORY, "Cancelling FUTURES order");
        let body = json!({ "category": LINEAR_CATEGORY, "symbol": symbol, "orderId": order_id });
        self.call_api::<EmptyResult>(Method::POST, "v5/order/cancel", None, Some(body), true).await?;
        info!(order_id, "FUTURES Order cancel request sent (or order was inactive)");
        Ok(())
    }

    /// Получение статуса СПОТ ордера
    async fn get_spot_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus> {
        let spot_pair = self.format_pair(symbol);
        debug!(symbol=%spot_pair, order_id, category=SPOT_CATEGORY, "Fetching SPOT order status");
        let params = [("category", SPOT_CATEGORY), ("orderId", order_id)];
        let query_result: OrderQueryResult = self.call_api(Method::GET, "v5/order/realtime", Some(&params), None, true).await?;

        let order_entry = query_result.list.into_iter().next().ok_or_else(|| {
            warn!("Order ID {} not found in realtime query for {}", order_id, spot_pair);
            anyhow!("Order not found")
        })?;

        let filled = if order_entry.cum_exec_qty.is_empty() { 0.0 } else { order_entry.cum_exec_qty.parse::<f64>()? };
        let remaining = if order_entry.leaves_qty.is_empty() { 0.0 } else { order_entry.leaves_qty.parse::<f64>()? };
        info!(order_id, status=%order_entry.status, filled, remaining, "Order status received (SPOT)");
        Ok(OrderStatus { filled_qty: filled, remaining_qty: remaining })
    }

    /// Получение статуса ФЬЮЧЕРСНОГО ордера
    async fn get_futures_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus> {
        debug!(symbol=%symbol, order_id, category=LINEAR_CATEGORY, "Fetching FUTURES order status");
        let params = [("category", LINEAR_CATEGORY), ("orderId", order_id)];
        let query_result: OrderQueryResult = self.call_api(Method::GET, "v5/order/realtime", Some(&params), None, true).await?;

        let order_entry = query_result.list.into_iter().next().ok_or_else(|| {
            warn!("Futures Order ID {} not found in realtime query for {}", order_id, symbol);
            anyhow!("Order not found")
        })?;

        let filled = if order_entry.cum_exec_qty.is_empty() { 0.0 } else { order_entry.cum_exec_qty.parse::<f64>()? };
        let remaining = if order_entry.leaves_qty.is_empty() { 0.0 } else { order_entry.leaves_qty.parse::<f64>()? };
        info!(order_id, status=%order_entry.status, filled, remaining, "Order status received (FUTURES)");
        Ok(OrderStatus { filled_qty: filled, remaining_qty: remaining })
    }

    /// Получение детального статуса СПОТ ордера
    async fn get_spot_order_execution_details(&self, symbol: &str, order_id: &str) -> Result<DetailedOrderStatus> {
        let spot_pair = self.format_pair(symbol);
        debug!(symbol=%spot_pair, order_id, category=SPOT_CATEGORY, "Fetching SPOT order execution details");

        let params = [("category", SPOT_CATEGORY), ("orderId", order_id)];
        let query_result: OrderQueryResult = self.call_api(
            Method::GET,
            "v5/order/realtime",
            Some(&params),
            None,
            true,
        ).await?;

        let order_entry = query_result.list.into_iter().next().ok_or_else(|| {
            warn!("Order ID {} not found in realtime query for {}", order_id, spot_pair);
            anyhow!("Order not found")
        })?;

        let filled_quantity = order_entry.cum_exec_qty.trim().parse::<f64>().unwrap_or_else(|error| {
            warn!(order_id, value=%order_entry.cum_exec_qty, %error, "Failed to parse cumExecQty, using 0.0");
            0.0
        });
        let remaining_quantity = order_entry.leaves_qty.trim().parse::<f64>().unwrap_or_else(|error| {
            warn!(order_id, value=%order_entry.leaves_qty, %error, "Failed to parse leavesQty, using 0.0");
            0.0
        });
        let cumulative_value = order_entry.cum_exec_value.trim().parse::<f64>().unwrap_or_else(|error| {
            warn!(order_id, value=%order_entry.cum_exec_value, %error, "Failed to parse cumExecValue, using 0.0");
            0.0
        });
        let average_price = order_entry.avg_price.trim().parse::<f64>().unwrap_or_else(|error| {
            trace!(order_id, value=%order_entry.avg_price, %error, "Failed to parse avgPrice or it was empty/zero, using 0.0");
            0.0
        });

        info!(
            order_id, status=%order_entry.status,
            filled=filled_quantity, remaining=remaining_quantity,
            cum_value=cumulative_value, avg_price=average_price,
            "Detailed order status received (SPOT)"
        );

        // --- ИСПРАВЛЕНО: Распаковываем Result ---
        let status_text = OrderStatusText::from_str(&order_entry.status).unwrap(); // unwrap() безопасен здесь

        Ok(DetailedOrderStatus {
            filled_qty: filled_quantity,
            remaining_qty: remaining_quantity,
            cumulative_executed_value: cumulative_value,
            average_price: average_price,
            status_text,
            last_filled_price: None, // Или укажите значение, если доступно
            last_filled_qty: None, // Или укажите значение, если доступно
            order_id: String::new(), // Или укажите значение, если доступно
            reject_reason: None, // Или укажите значение, если доступно
            side: OrderSide::Buy, // Или укажите значение, если доступно
            symbol: String::new(), // Или укажите значение, если доступно
        })
    }

    async fn get_futures_ticker(&self, symbol: &str) -> Result<FuturesTickerInfo> {
        debug!(symbol=%symbol, category=LINEAR_CATEGORY, "Fetching futures ticker");
        let params = [("category", LINEAR_CATEGORY), ("symbol", symbol)];

        let api_result: FuturesTickersApiResult = self.call_api(
            Method::GET,
            "v5/market/tickers",
            Some(&params),
            None,
            false
        ).await?;

        let ticker_data = api_result.list.into_iter()
            .find(|t| t.symbol == symbol)
            .ok_or_else(|| anyhow!("No futures ticker info found for {}", symbol))?;

        let bid_price = ticker_data.bid1_price.trim().parse::<f64>().map_err(|e| anyhow!("Failed to parse bid price for {}: {} (value: '{}')", symbol, e, ticker_data.bid1_price))?;
        let ask_price = ticker_data.ask1_price.trim().parse::<f64>().map_err(|e| anyhow!("Failed to parse ask price for {}: {} (value: '{}')", symbol, e, ticker_data.ask1_price))?;
        let last_price = ticker_data.last_price.trim().parse::<f64>().map_err(|e| anyhow!("Failed to parse last price for {}: {} (value: '{}')", symbol, e, ticker_data.last_price))?;

        if bid_price <= 0.0 || ask_price <= 0.0 {
             warn!(symbol, bid=bid_price, ask=ask_price, "Received zero or non-positive bid/ask price from ticker");
             if last_price > 0.0 {
                 warn!(symbol, "Using last_price ({}) as fallback for zero bid/ask", last_price);
                 Ok(FuturesTickerInfo { symbol: ticker_data.symbol, bid_price: last_price, ask_price: last_price, last_price })
             } else {
                 Err(anyhow!("Received invalid bid/ask prices (<= 0.0) for {}", symbol))
             }
        } else {
            Ok(FuturesTickerInfo { symbol: ticker_data.symbol, bid_price, ask_price, last_price })
        }
    }

    // --- ИСПРАВЛЕНО: Реализация get_spot_price_fallback внутри impl Exchange ---
    async fn get_spot_price_fallback(&self, futures_symbol: &str) -> Result<f64> {
        // Используем self.quote_currency
        let base_symbol = futures_symbol.trim_end_matches(&self.quote_currency).trim_end_matches("PERP");
        if base_symbol == futures_symbol {
            return Err(anyhow!("Could not determine base symbol from futures symbol '{}' for fallback", futures_symbol));
        }
        // Вызываем другой метод трейта через self
        self.get_spot_price(base_symbol).await
    }
    // --- КОНЕЦ ИСПРАВЛЕНИЯ ---

    // --- ИСПРАВЛЕНО: Реализация get_market_price внутри impl Exchange ---
    async fn get_market_price(&self, symbol: &str, is_spot: bool) -> Result<f64> {
        // ... (код без изменений) ...
        let price = if is_spot {
            self.get_spot_price(symbol).await?
        } else {
            match self.get_futures_ticker(symbol).await {
                Ok(ticker) if ticker.bid_price > 0.0 && ticker.ask_price > 0.0 => (ticker.bid_price + ticker.ask_price) / 2.0,
                Ok(ticker) => {
                     warn!("Invalid ticker prices bid={}, ask={}. Using last_price if available.", ticker.bid_price, ticker.ask_price);
                     if ticker.last_price > 0.0 {
                         ticker.last_price
                     } else {
                         warn!("Falling back to spot price due to invalid futures ticker data.");
                         self.get_spot_price_fallback(symbol).await? // Вызываем метод трейта
                     }
                }
                Err(e) => {
                     warn!("Failed get futures ticker for price: {}. Falling back to spot price.", e);
                     self.get_spot_price_fallback(symbol).await? // Вызываем метод трейта
                }
            }
        };
        if price <= 0.0 {
            Err(anyhow!("Invalid market price received: {}", price))
        } else {
            Ok(price)
        }
    }
} // --- Конец impl Exchange for Bybit ---
