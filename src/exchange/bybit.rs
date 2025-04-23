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
use tracing::{debug, error, info, warn}; // Добавляем нужные уровни логирования

type HmacSha256 = Hmac<Sha256>;

// --- Константы для категорий ---
const SPOT_CATEGORY: &str = "spot";
const LINEAR_CATEGORY: &str = "linear";

/// Универсальная обёртка для ответов Bybit API v5
#[derive(Deserialize, Debug)] // Добавим Debug для логирования
struct ApiResponse<T> {
    #[serde(rename = "retCode")] ret_code: i32,
    #[serde(rename = "retMsg")] ret_msg: String,
    result: T,
    // Добавляем time опционально, т.к. не все ответы его содержат
    #[serde(rename = "time")] _server_time: Option<i64>,
}

/// Пустой результат для запросов без данных (например, отмена ордера)
#[derive(Deserialize, Debug)]
struct EmptyResult {}

/// Ответ по балансу: список Unified‑аккаунтов
#[derive(Deserialize, Debug)]
struct BalanceResult {
    #[serde(rename = "list")]
    list: Vec<UnifiedAccountBalance>,
}

/// Один Unified‑аккаунт с массивом монет
#[derive(Deserialize, Debug)]
struct UnifiedAccountBalance {
    #[serde(rename = "accountType")]
    account_type: String,
    #[serde(rename = "coin")]
    coins: Vec<BalanceEntry>,
}

/// Запись баланса для конкретной монеты
#[derive(Deserialize, Debug)]
struct BalanceEntry {
    #[serde(rename = "coin")]
    coin: String,
    #[serde(rename = "walletBalance")]
    wallet: String, // Используем walletBalance как общий
    #[serde(rename = "locked")]
    locked: String,
    // Добавим availableToWithdraw или availableBalance, если нужно точное значение free
    #[serde(rename = "availableToWithdraw")] // или availableBalance
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
    // Можно добавить другие полезные поля: tickSize, lotSizeFilter и т.д.
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

/// Ответ при запросе статуса ордера (используем /v5/order/history или /v5/order/realtime)
#[derive(Deserialize, Debug)]
struct OrderQueryResult {
    list: Vec<OrderQueryEntry>,
}

#[derive(Deserialize, Debug)]
struct OrderQueryEntry {
    #[serde(rename = "orderId")] id: String,
    #[serde(rename = "symbol")] symbol: String,
    #[serde(rename = "side")] side: String,
    #[serde(rename = "orderStatus")] status: String, // e.g., "New", "Filled", "PartiallyFilled", "Cancelled"
    #[serde(rename = "cumExecQty")] cum_exec_qty: String,
    #[serde(rename = "cumExecValue")] cum_exec_value: String, // Суммарная стоимость исполненной части
    #[serde(rename = "leavesQty")] leaves_qty: String,
    #[serde(rename = "price")] price: String, // Цена лимитного ордера
    #[serde(rename = "avgPrice")] avg_price: String, // Средняя цена исполнения
    #[serde(rename = "createdTime")] created_time: String, // Время создания как строка мс
    // Добавить другие нужные поля: orderType, timeInForce и т.д.
}


/// Ответ по тикерам (для спот‑цены)
#[derive(Deserialize, Debug)]
struct TickersResult {
    category: String,
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
    #[serde(rename = "timeNano")] time_nano: String, // Используем наносекунды для большей точности
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
    quote_currency: String, // <-- Добавили валюту котировки
    // Используем Mutex для асинхронного доступа
    time_offset_ms: Arc<Mutex<Option<i64>>>,
    balance_cache: Arc<Mutex<Option<(Vec<(String, Balance)>, SystemTime)>>>, // Кэш для баланса
}

impl Bybit {
    /// Создаёт новый экземпляр клиента и синхронизирует время
    pub async fn new(key: &str, secret: &str, base_url: &str, quote_currency: &str) -> Result<Self> {
        if !base_url.starts_with("http") {
            return Err(anyhow!("Invalid base URL"));
        }
        if quote_currency.is_empty() {
            return Err(anyhow!("Quote currency cannot be empty"));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        // Используем let mut instance, так как sync_time требует &self
        let instance = Self {
            api_key: key.into(),
            api_secret: secret.into(),
            client,
            base_url: base_url.trim_end_matches('/').into(),
            recv_window: 5_000,
            quote_currency: quote_currency.to_uppercase(), // Сохраняем в верхнем регистре
            time_offset_ms: Arc::new(Mutex::new(None)),
            balance_cache: Arc::new(Mutex::new(None)),
        };

        // Синхронизируем время при создании
        if let Err(e) = instance.sync_time().await {
            warn!("Initial time sync failed: {}. Authentication might fail.", e);
            // Можно решить, критична ли эта ошибка для старта
            // return Err(e);
        } else {
            info!("Initial time sync successful.");
        }

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
            .map_err(|e| anyhow!("Failed to send time sync request: {}", e))?;

        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            error!(%status, %body, "Time sync request failed");
            return Err(anyhow!("Failed to sync server time: status {}", status));
        }

        // Парсим ответ времени
        let raw_response: ApiResponse<ServerTimeResult> = res.json().await
            .map_err(|e| anyhow!("Failed to parse time sync response: {}", e))?;

        // Используем timeNano для большей точности, конвертируем в миллисекунды
        let server_time_ms = raw_response.result.time_nano.parse::<u128>()? / 1_000_000;

        let local_time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| anyhow!("System time error: {}", e))?
            .as_millis();

        let offset = server_time_ms as i64 - local_time_ms as i64;
        info!("Server time synced. Offset: {} ms", offset);

        // Блокируем Mutex асинхронно
        let mut time_offset_guard = self.time_offset_ms.lock().await;
        *time_offset_guard = Some(offset);

        Ok(())
    }

    /// Получение скорректированной временной метки в миллисекундах
    async fn get_timestamp_ms(&self) -> Result<i64> {
        let local_time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| anyhow!("System time error: {}", e))?
            .as_millis() as i64;

        // Блокируем Mutex асинхронно
        let time_offset_guard = self.time_offset_ms.lock().await;
        // Используем *time_offset_guard, чтобы получить Option<i64>
        let offset = (*time_offset_guard).ok_or_else(|| {
            warn!("Time offset is not available. Authentication might fail.");
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
        let timestamp_ms = self.get_timestamp_ms().await?;
        let t = timestamp_ms.to_string();
        let rw = self.recv_window.to_string();
        // Формируем строку для подписи: timestamp + apiKey + recvWindow + (queryString || requestBody)
        let payload_str = format!("{}{}{}{}", t, self.api_key, rw, if !qs.is_empty() { qs } else { body });
        let sign = self.sign(&payload_str);

        Ok(vec![
            ("X-BAPI-API-KEY", self.api_key.clone()),
            ("X-BAPI-TIMESTAMP", t),
            ("X-BAPI-RECV-WINDOW", rw),
            ("X-BAPI-SIGN", sign),
            // Добавляем Content-Type для POST запросов
            ("Content-Type", "application/json".to_string()),
        ])
    }

    /// Универсальный вызов Bybit API
    async fn call_api<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        endpoint: &str,
        query: Option<&[(&str, &str)]>, // Используем срезы для query
        body: Option<Value>,
        auth: bool,
    ) -> Result<T> {
        let url = self.url(endpoint);
        debug!(%url, method=%method, ?query, ?body, auth, "Bybit API Call ->");

        let mut req = self.client.request(method.clone(), &url);

        // Собираем query-string для подписи и запроса
        let qs = if let Some(q) = query {
            // URL-кодируем параметры для строки запроса
            let encoded_query = q.iter()
                .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            req = req.query(q); // reqwest сам кодирует query
            encoded_query // Возвращаем кодированную строку для подписи
        } else {
            String::new()
        };

        // Собираем тело для подписи и запроса
        let body_str = if let Some(ref b) = body {
            let s = serde_json::to_string(b)?; // Сериализуем тело в строку для подписи
            req = req.json(b); // Передаем Value в reqwest
            s
        } else {
            String::new()
        };

        // Добавляем заголовки аутентификации, если нужно
        if auth {
            let headers = self.auth_headers(&qs, &body_str).await?;
            for (h, v) in headers {
                req = req.header(h, v);
            }
        }

        // Отправляем запрос
        let resp = req.send().await.map_err(|e| anyhow!("Request failed: {}", e))?;
        let status = resp.status();
        let raw_body = resp.text().await.map_err(|e| anyhow!("Failed to read response body: {}", e))?;
        debug!(%status, body_len=raw_body.len(), "Bybit API Response <-");
        // Логируем тело ответа только если это не успех или включен trace уровень
        if !status.is_success()  {
             info!(response_body=%raw_body, "Full Bybit Response Body");
        }

        // Десериализуем и проверяем код ответа Bybit
        // Используем serde_path_to_error для более детальных ошибок парсинга
        let mut deserializer = serde_json::Deserializer::from_str(&raw_body);
        let api_resp: ApiResponse<T> = serde_path_to_error::deserialize(&mut deserializer)
            .map_err(|e| {
                error!(error=%e, raw_body, "Failed to deserialize Bybit response");
                anyhow!("Failed to parse Bybit response: {}", e)
            })?;

        if api_resp.ret_code != 0 {
            error!(code=api_resp.ret_code, msg=%api_resp.ret_msg, "Bybit API Error");
            Err(anyhow!("Bybit API Error ({}): {}", api_resp.ret_code, api_resp.ret_msg))
        } else {
            Ok(api_resp.result)
        }
    }
}

#[async_trait]
impl Exchange for Bybit {
    /// Проверка соединения (просто пинг времени)
    async fn check_connection(&mut self) -> Result<()> {
        // Попытка синхронизировать время при проверке соединения
        if let Err(e) = self.sync_time().await {
            warn!("Time sync failed during check_connection: {}", e);
            // Не возвращаем ошибку, т.к. сам пинг может пройти
        }
        // Используем эндпоинт времени как пинг
        let _: ServerTimeResult = self.call_api(Method::GET, "v5/market/time", None, None, false).await?;
        info!("Connection check successful (ping OK)");
        Ok(())
    }

    /// Получение баланса для конкретной монеты
    async fn get_balance(&self, coin: &str) -> Result<Balance> {
        let all_balances = self.get_all_balances().await?; // Получаем из кэша или API
        all_balances.into_iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(coin))
            .map(|(_, b)| b)
            .ok_or_else(|| anyhow!("No balance entry found for {}", coin))
    }

    /// Получение всех балансов (с кэшированием)
    async fn get_all_balances(&self) -> Result<Vec<(String, Balance)>> {
        let cache_duration = Duration::from_secs(5); // Увеличиваем время жизни кэша до 5 секунд
        let now = SystemTime::now();

        // Блокируем кэш асинхронно
        let mut cache_guard = self.balance_cache.lock().await;

        // Проверяем кэш
        if let Some((cached_balances, timestamp)) = &*cache_guard {
            // Проверяем время жизни кэша
            // Используем match для обработки ошибки duration_since, хотя она маловероятна
            match now.duration_since(*timestamp) {
                Ok(age) if age < cache_duration => {
                    info!("Returning cached balances.");
                    return Ok(cached_balances.clone());
                }
                Ok(_) => { // Кэш устарел
                    info!("Balance cache expired.");
                }
                Err(e) => { // Ошибка системного времени
                    warn!("System time error checking cache age: {}. Fetching fresh balances.", e);
                }
            }
        } else {
            info!("Balance cache is empty.");
        }

        // Кэш устарел или пуст, запрашиваем API
        info!("Fetching fresh balances from API.");
        let res: BalanceResult = self.call_api(
            Method::GET,
            "v5/account/wallet-balance",
            // Запрашиваем только основной тип аккаунта (UNIFIED или CONTRACT)
            Some(&[("accountType", "UNIFIED")]), // или "CONTRACT", если нужен другой
            None,
            true,
        ).await?;

        // Ищем нужный тип аккаунта
        let account = res.list.into_iter()
            .find(|acc| acc.account_type == "UNIFIED") // Ищем конкретно UNIFIED
            .ok_or_else(|| anyhow!("UNIFIED account type not found in balance response"))?;

        // Парсим балансы монет
        let mut balances = Vec::new();
        for entry in account.coins {
            // Используем availableToWithdraw или availableBalance как 'free'
            let free = entry.available.parse::<f64>()
                .map_err(|e| anyhow!("Failed to parse free balance for {}: {}", entry.coin, e))?;
            let locked = entry.locked.parse::<f64>()
                .map_err(|e| anyhow!("Failed to parse locked balance for {}: {}", entry.coin, e))?;
            balances.push((entry.coin, Balance { free, locked }));
        }

        // Обновляем кэш
        *cache_guard = Some((balances.clone(), now));
        info!("Balances updated and cached.");

        Ok(balances)
    }

    /// Размещение лимитного ордера (для СПОТА)
    async fn place_limit_order(
        &self,
        symbol: &str, // Ожидаем базовый символ, например "BTC"
        side: OrderSide,
        qty: f64,
        price: f64,
    ) -> Result<Order> {
        let spot_pair = self.format_pair(symbol); // Формируем "BTCUSDT"
        info!(symbol=%spot_pair, side=%side, qty, price, category=SPOT_CATEGORY, "Placing SPOT limit order");

        // Bybit ожидает qty и price как строки
        let body = json!({
            "category": SPOT_CATEGORY,
            "symbol": spot_pair,
            "side": side.to_string(),
            "orderType": "Limit",
            "qty": qty.to_string(), // qty как строка
            "price": price.to_string(), // price как строка
            "timeInForce": "GTC", // Good-Till-Cancelled
            "orderLinkId": format!("spot-limit-{}", Uuid::new_v4()), // Уникальный ID
        });

        let result: OrderCreateResult = self
            .call_api(Method::POST, "v5/order/create", None, Some(body), true)
            .await?;

        info!(order_id=%result.id, link_id=%result.link_id, "SPOT limit order placed successfully");

        // Возвращаем базовую информацию об ордере
        // Время создания и исполненные параметры будут доступны через get_order_status
        Ok(Order {
            id: result.id,
            side, // Мы знаем сторону
            qty,  // Мы знаем запрошенное количество
            price: Some(price), // Мы знаем запрошенную цену
            ts: self.get_timestamp_ms().await?, // Примерное время отправки
        })
    }

    /// Размещение рыночного ордера (для ФЬЮЧЕРСОВ)
    async fn place_market_order(
        &self,
        symbol: &str, // Ожидаем ПОЛНЫЙ символ фьючерса, например "BTCUSDT"
        side: OrderSide,
        qty: f64,
    ) -> Result<Order> {
        info!(symbol=%symbol, side=%side, qty, category=LINEAR_CATEGORY, "Placing FUTURES market order");

        // Bybit ожидает qty как строку
        let body = json!({
            "category": LINEAR_CATEGORY,
            "symbol": symbol, // Используем переданный символ (уже должен быть парой)
            "side": side.to_string(),
            "orderType": "Market",
            "qty": qty.to_string(), // qty как строка
            // Для рыночного ордера можно указать timeInForce=IOC или FOK, если нужно
            // "timeInForce": "IOC",
            "orderLinkId": format!("fut-market-{}", Uuid::new_v4()), // Уникальный ID
        });

        let result: OrderCreateResult = self
            .call_api(Method::POST, "v5/order/create", None, Some(body), true)
            .await?;

        info!(order_id=%result.id, link_id=%result.link_id, "FUTURES market order placed successfully");

        // Возвращаем базовую информацию
        Ok(Order {
            id: result.id,
            side,
            qty,
            price: None, // Цена исполнения неизвестна сразу
            ts: self.get_timestamp_ms().await?,
        })
    }

    /// Отмена ордера (нужно знать категорию)
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()> {
        // !!! ВАЖНО: Этот метод не знает, спотовый это ордер или фьючерсный.
        // Нужно либо передавать категорию, либо иметь отдельные методы отмены.
        // Пока предположим, что отменяем СПОТОВЫЙ ордер (как в hedger.rs)
        let spot_pair = self.format_pair(symbol);
        info!(symbol=%spot_pair, order_id, category=SPOT_CATEGORY, "Cancelling SPOT order");

        let body = json!({
            "category": SPOT_CATEGORY, // <-- Указываем категорию
            "symbol": spot_pair,
            "orderId": order_id,
            // Можно использовать orderLinkId, если он известен
            // "orderLinkId": "...",
        });

        // Результат не важен, только факт успеха (retCode=0)
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

        // Используем эндпоинт /v5/market/tickers
        let params = [("category", SPOT_CATEGORY), ("symbol", &spot_pair)];
        let tickers_result: TickersResult = self.call_api(
                Method::GET,
                "v5/market/tickers",
                Some(&params),
                None,
                false, // Не требует аутентификации
            )
            .await?;

        // Ищем нужный тикер в списке
        let ticker = tickers_result.list.into_iter()
             .find(|t| t.symbol == spot_pair) // Убедимся, что это наш символ
             .ok_or_else(|| anyhow!("No ticker info found for {}", spot_pair))?;

        ticker.price.parse::<f64>()
             .map_err(|e| anyhow!("Failed to parse spot price for {}: {}", spot_pair, e))
    }

    /// Получение статуса ордера (нужно знать категорию)
    async fn get_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus> {
        // !!! ВАЖНО: Аналогично cancel_order, нужна категория.
        // Предполагаем, что проверяем СПОТОВЫЙ ордер.
        let spot_pair = self.format_pair(symbol);
        debug!(symbol=%spot_pair, order_id, category=SPOT_CATEGORY, "Fetching SPOT order status");

        // Используем /v5/order/history или /v5/order/realtime
        // /realtime может быть быстрее для активных ордеров
        let params = [
            ("category", SPOT_CATEGORY),
            ("orderId", order_id),
            // Можно добавить symbol, но orderId обычно уникален
            // ("symbol", &spot_pair),
        ];
        let query_result: OrderQueryResult = self
            .call_api(
                Method::GET,
                "v5/order/realtime", // Используем realtime
                Some(&params),
                None,
                true, // Требует аутентификации
            )
            .await?;

        // Ищем ордер в списке (обычно будет один)
        let order_entry = query_result.list.into_iter().next()
            .ok_or_else(|| anyhow!("Order ID {} not found", order_id))?;

        // Парсим исполненное и оставшееся количество
        let filled = order_entry.cum_exec_qty.parse::<f64>()
            .map_err(|e| anyhow!("Failed to parse filled qty for order {}: {}", order_id, e))?;
        let remaining = order_entry.leaves_qty.parse::<f64>()
            .map_err(|e| anyhow!("Failed to parse remaining qty for order {}: {}", order_id, e))?;

        info!(order_id, status=%order_entry.status, filled, remaining, "Order status received");

        Ok(OrderStatus {
            filled_qty: filled,
            remaining_qty: remaining,
        })
    }

    /// Получение MMR (Maintenance Margin Requirement) для ЛИНЕЙНОГО контракта
    async fn get_mmr(&self, symbol: &str) -> Result<f64> {
        let linear_pair = self.format_pair(symbol);
        debug!(symbol=%linear_pair, category=LINEAR_CATEGORY, "Fetching MMR");

        let params = [("category", LINEAR_CATEGORY), ("symbol", &linear_pair)];
        let info_result: InstrumentsInfoResult = self.call_api(
            Method::GET,
            "v5/market/instruments-info",
            Some(&params),
            None,
            false, // Не требует аутентификации
        ).await?;

        let instrument = info_result.list.into_iter()
            .find(|i| i.symbol == linear_pair) // Убедимся, что это наш инструмент
            .ok_or_else(|| anyhow!("No instrument info found for {}", linear_pair))?;

        instrument.mmr.parse::<f64>()
            .map_err(|e| anyhow!("Failed to parse MMR for {}: {}", linear_pair, e))
    }

    /// Получение средней ставки финансирования для ЛИНЕЙНОГО контракта
    async fn get_funding_rate(&self, symbol: &str, days: u16) -> Result<f64> {
        let linear_pair = self.format_pair(symbol);
        // Bybit API позволяет запросить до 200 записей за раз
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
                false, // Не требует аутентификации
            )
            .await?;

        if funding_result.list.is_empty() {
            warn!("No funding rate history found for {}", linear_pair);
            return Ok(0.0); // Или вернуть ошибку?
        }

        let mut sum = 0.0;
        let mut count = 0;
        for entry in &funding_result.list {
            match entry.rate.parse::<f64>() {
                Ok(rate) => {
                    sum += rate;
                    count += 1;
                }
                Err(e) => {
                    warn!("Failed to parse funding rate '{}': {}", entry.rate, e);
                }
            }
        }

        if count == 0 {
            Ok(0.0)
        } else {
            Ok(sum / count as f64)
        }
    }
}
