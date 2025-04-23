// src/exchange/bybit.rs

use super::{Exchange, OrderStatus};
use crate::exchange::types::{Balance, Order, OrderSide};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::{Client, Method};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use uuid::Uuid;
use std::sync::Arc;
// Добавляем все уровни логирования
use tracing::{debug, error, info, warn, trace};
use serde_path_to_error; // Используем для детальных ошибок парсинга

type HmacSha256 = Hmac<Sha256>;

// --- Константы для категорий ---
const SPOT_CATEGORY: &str = "spot";
const LINEAR_CATEGORY: &str = "linear";

// --- Структуры для парсинга ответов API ---

/// Универсальная обёртка для ответов Bybit API v5
#[derive(Deserialize, Debug)]
struct ApiResponse<T> {
    #[serde(rename = "retCode")] ret_code: i32,
    #[serde(rename = "retMsg")] ret_msg: String,
    result: T,
    #[serde(rename = "time")] _server_time: Option<i64>,
}

/// Пустой результат для запросов без данных
#[derive(Deserialize, Debug)]
struct EmptyResult {}

/// Ответ по балансу
#[derive(Deserialize, Debug)]
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

/// Ответ по информации об инструменте (для MMR)
#[derive(Deserialize, Debug)]
struct InstrumentsInfoResult {
    list: Vec<InstrumentInfo>,
}

#[derive(Deserialize, Debug)]
struct InstrumentInfo {
    #[serde(rename = "symbol")] symbol: String,
    #[serde(rename = "maintMarginRate")] mmr: String,
}

/// Ответ по funding‑rate
#[derive(Deserialize, Debug)]
struct FundingResult {
    list: Vec<FundingEntry>,
}

#[derive(Deserialize, Debug)]
struct FundingEntry {
    #[serde(rename = "fundingRate")] rate: String,
}

/// Ответ при создании ордера
#[derive(Deserialize, Debug)]
struct OrderCreateResult {
    #[serde(rename = "orderId")] id: String,
    #[serde(rename = "orderLinkId")] link_id: String,
}

/// Ответ при запросе статуса ордера
#[derive(Deserialize, Debug)]
struct OrderQueryResult {
    list: Vec<OrderQueryEntry>,
}

#[derive(Deserialize, Debug)]
struct OrderQueryEntry {
    #[serde(rename = "orderId")] id: String,
    // Добавляем '_' к неиспользуемым полям
    #[serde(rename = "symbol")] _symbol: String,
    #[serde(rename = "side")] _side: String,
    #[serde(rename = "orderStatus")] status: String, // Оставляем status для логов
    #[serde(rename = "cumExecQty")] cum_exec_qty: String,
    #[serde(rename = "cumExecValue")] _cum_exec_value: String,
    #[serde(rename = "leavesQty")] leaves_qty: String,
    #[serde(rename = "price")] _price: String,
    #[serde(rename = "avgPrice")] _avg_price: String,
    #[serde(rename = "createdTime")] _created_time: String,
}

/// Ответ по тикерам
#[derive(Deserialize, Debug)]
struct TickersResult {
    // Добавляем '_' к неиспользуемому полю
    _category: String,
    list: Vec<TickerInfo>,
}

#[derive(Deserialize, Debug)]
struct TickerInfo {
    symbol: String,
    #[serde(rename = "lastPrice")] price: String,
}

/// Ответ по времени сервера
#[derive(Deserialize, Debug)]
struct ServerTimeResult {
    #[serde(rename = "timeNano")] time_nano: String,
    #[serde(rename = "timeSecond")] _time_second: String,
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
            .timeout(Duration::from_secs(10)) // Таймаут для запросов
            .build()?;

        let instance = Self {
            api_key: key.into(),
            api_secret: secret.into(),
            client,
            base_url: base_url.trim_end_matches('/').into(),
            recv_window: 5_000, // Стандартное окно приема Bybit
            quote_currency: quote_currency.to_uppercase(),
            time_offset_ms: Arc::new(Mutex::new(None)),
            balance_cache: Arc::new(Mutex::new(None)),
        };

        // Синхронизируем время при создании
        if let Err(e) = instance.sync_time().await {
            // Логируем как ошибку, но не прерываем создание клиента
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

        // Парсим ответ времени
        let raw_response: ApiResponse<ServerTimeResult> = res.json().await
            .map_err(|e| {
                error!("Failed to parse time sync response JSON: {}", e);
                anyhow!("Failed to parse time sync response: {}", e)
            })?;

        // Используем timeNano для большей точности, конвертируем в миллисекунды
        let server_time_ms = match raw_response.result.time_nano.parse::<u128>() {
            Ok(t) => t / 1_000_000,
            Err(e) => {
                error!("Failed to parse server time nano '{}': {}", raw_response.result.time_nano, e);
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

        // Блокируем Mutex асинхронно
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

        // Блокируем Mutex асинхронно
        let time_offset_guard = self.time_offset_ms.lock().await;
        // Используем *time_offset_guard, чтобы получить Option<i64>
        let offset = (*time_offset_guard).ok_or_else(|| {
            // Эта ошибка критична для аутентификации
            error!("Time offset is not available. Cannot generate timestamp.");
            anyhow!("Time not synchronized with server")
        })?;

        Ok(local_time_ms + offset)
    }

    /// Генерация подписи
    fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC can take key of any size"); // unwrap безопасен
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Формирование заголовков аутентификации
    async fn auth_headers(&self, qs: &str, body: &str) -> Result<Vec<(&str, String)>> {
        let timestamp_ms = self.get_timestamp_ms().await?; // Ошибка уже обработана в get_timestamp_ms
        let t = timestamp_ms.to_string();
        let rw = self.recv_window.to_string();
        // Формируем строку для подписи: timestamp + apiKey + recvWindow + (queryString || requestBody)
        let payload_str = format!("{}{}{}{}", t, self.api_key, rw, if !qs.is_empty() { qs } else { body });
        let sign = self.sign(&payload_str);
        debug!("Generated signature for payload: {}", payload_str); // Логируем payload только на debug

        Ok(vec![
            ("X-BAPI-API-KEY", self.api_key.clone()),
            ("X-BAPI-TIMESTAMP", t),
            ("X-BAPI-RECV-WINDOW", rw),
            ("X-BAPI-SIGN", sign),
            ("Content-Type", "application/json".to_string()),
        ])
    }

    /// Универсальный вызов Bybit API
    async fn call_api<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        endpoint: &str,
        query: Option<&[(&str, &str)]>,
        body: Option<Value>,
        auth: bool,
    ) -> Result<T> {
        let url = self.url(endpoint);
        // Логируем параметры запроса на уровне debug
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
                    // Ошибка генерации заголовков (вероятно, из-за времени)
                    return Err(e);
                }
            }
        }

        // Отправляем запрос
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

        // Логируем тело ответа только если это не успех или включен trace уровень
        if !status.is_success() || tracing::enabled!(tracing::Level::TRACE) {
             // Используем info для ошибок HTTP, trace для всех остальных
             if !status.is_success() {
                 info!(%url, %status, response_body=%raw_body, "Non-success HTTP status received");
             } else {
                 trace!(%url, %status, response_body=%raw_body, "Full Bybit Response Body");
             }
        }

        // Десериализуем и проверяем код ответа Bybit
        let mut deserializer = serde_json::Deserializer::from_str(&raw_body);
        let api_resp: ApiResponse<T> = match serde_path_to_error::deserialize(&mut deserializer) {
            Ok(resp) => resp,
            Err(e) => {
                // Логируем ошибку парсинга с путем к полю, если возможно
                error!(error=%e, path=%e.path(), raw_body, "Failed to deserialize Bybit response");
                return Err(anyhow!("Failed to parse Bybit response: {}", e));
            }
        };

        // Проверяем бизнес-логику Bybit
        if api_resp.ret_code != 0 {
            // Логируем как ошибку, если это не ожидаемый код (например, ордер не найден)
            // Можно добавить match для известных кодов ошибок
            error!(code=api_resp.ret_code, msg=%api_resp.ret_msg, %url, "Bybit API Error");
            Err(anyhow!("Bybit API Error ({}): {}", api_resp.ret_code, api_resp.ret_msg))
        } else {
            // Логируем успешный результат на уровне debug
            debug!(%url, "Bybit API call successful (retCode=0)");
            Ok(api_resp.result)
        }
    }
}

#[async_trait]
impl Exchange for Bybit {
    /// Проверка соединения
    async fn check_connection(&mut self) -> Result<()> {
        info!("Checking Bybit connection...");
        // Попытка синхронизировать время при проверке соединения
        if let Err(e) = self.sync_time().await {
            // Логируем как ошибку, но не прерываем проверку
            error!("Time sync failed during check_connection: {}", e);
        }
        // Используем эндпоинт времени как пинг
        let _: ServerTimeResult = self.call_api(Method::GET, "v5/market/time", None, None, false).await?;
        info!("Connection check successful (ping OK).");
        Ok(())
    }

    /// Получение баланса для конкретной монеты
    async fn get_balance(&self, coin: &str) -> Result<Balance> {
        debug!(%coin, "Getting balance for specific coin");
        let all_balances = self.get_all_balances().await?; // Использует кэш или API
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

        // Проверяем кэш
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

        // Кэш устарел или пуст, запрашиваем API
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
            // Парсим wallet (total) и locked, обрабатывая пустые строки
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

            // Вычисляем free. Убедимся, что он не отрицательный.
            let free = (total - locked).max(0.0);

            // Добавляем, если общий баланс (total) больше нуля
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

    /// Размещение лимитного ордера (для СПОТА)
    async fn place_limit_order(
        &self,
        symbol: &str, // Базовый символ
        side: OrderSide,
        qty: f64,
        price: f64,
    ) -> Result<Order> {
        let spot_pair = self.format_pair(symbol);
        info!(symbol=%spot_pair, %side, qty, price, category=SPOT_CATEGORY, "Placing SPOT limit order");

        let body = json!({
            "category": SPOT_CATEGORY,
            "symbol": spot_pair,
            "side": side.to_string(),
            "orderType": "Limit",
            "qty": qty.to_string(),
            "price": price.to_string(),
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
        symbol: &str, // ПОЛНЫЙ символ фьючерса
        side: OrderSide,
        qty: f64,
    ) -> Result<Order> {
        info!(%symbol, %side, qty, category=LINEAR_CATEGORY, "Placing FUTURES market order");

        let body = json!({
            "category": LINEAR_CATEGORY,
            "symbol": symbol,
            "side": side.to_string(),
            "orderType": "Market",
            "qty": qty.to_string(),
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
                // Это может быть нормальной ситуацией, если ордер уже исполнен и ушел в историю
                warn!("Order ID {} not found in realtime query", order_id);
                anyhow!("Order ID {} not found in realtime query", order_id)
                // TODO: Возможно, стоит сделать второй запрос в /v5/order/history, если здесь не найдено?
            })?;

        // Парсим исполненное и оставшееся количество, обрабатывая пустые строки
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

    /// Получение MMR для ЛИНЕЙНОГО контракта
    async fn get_mmr(&self, symbol: &str) -> Result<f64> {
        let linear_pair = self.format_pair(symbol);
        debug!(symbol=%linear_pair, category=LINEAR_CATEGORY, "Fetching MMR");

        let params = [("category", LINEAR_CATEGORY), ("symbol", &linear_pair)];
        let info_result: InstrumentsInfoResult = self.call_api(
            Method::GET,
            "v5/market/instruments-info",
            Some(&params),
            None,
            false,
        ).await?;

        let instrument = info_result.list.into_iter()
            .find(|i| i.symbol == linear_pair)
            .ok_or_else(|| {
                warn!("No instrument info found for {}", linear_pair);
                anyhow!("No instrument info found for {}", linear_pair)
            })?;

        instrument.mmr.parse::<f64>()
            .map_err(|e| {
                 error!("Failed to parse MMR for {}: {} (value: '{}')", linear_pair, e, instrument.mmr);
                 anyhow!("Failed to parse MMR for {}: {}", linear_pair, e)
            })
    }

    /// Получение средней ставки финансирования для ЛИНЕЙНОГО контракта
    async fn get_funding_rate(&self, symbol: &str, days: u16) -> Result<f64> {
        let linear_pair = self.format_pair(symbol);
        let limit = days.min(200).to_string(); // Bybit API limit
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
            return Ok(0.0); // Возвращаем 0, если истории нет
        }

        let mut sum = 0.0;
        let mut count = 0;
        for entry in &funding_result.list {
            // Обрабатываем пустые строки и ошибки парсинга
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
            Ok(0.0) // Возвращаем 0, если не удалось распарсить ни одной ставки
        } else {
            let avg_rate = sum / count as f64;
            info!(symbol=%linear_pair, days, average_rate=avg_rate, "Calculated average funding rate");
            Ok(avg_rate)
        }
    }
}