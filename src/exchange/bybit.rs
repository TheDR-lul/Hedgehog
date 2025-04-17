// src/exchange/bybit.rs

use super::Exchange;
use crate::exchange::types::{Balance, OrderSide, Order};
use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use reqwest::{Client, Method};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// Универсальная обёртка ответа Bybit V5
#[derive(Deserialize)]
struct ApiResponse<T> {
    #[serde(rename = "retCode")]   pub ret_code:   i32,
    #[serde(rename = "retMsg")]    pub ret_msg:    String,
    pub result:    T,
    #[serde(rename = "time")]      pub server_time: i64,
}

/// Модель для `/v5/account/wallet-balance`
#[derive(Deserialize)]
struct BalanceResult {
    pub balances: Vec<BalanceEntry>,
}

#[derive(Deserialize)]
struct BalanceEntry {
    pub coin:               String,
    #[serde(rename = "walletBalance")]    pub wallet:    String,
    #[serde(rename = "availableBalance")] pub available: String,
}

/// Клиент Bybit V5 (prod или testnet)
#[derive(Debug, Clone)]
pub struct Bybit {
    api_key:     String,
    api_secret:  String,
    client:      Client,
    base_url:    String,
    recv_window: u64,
}

impl Bybit {
    /// `base_url` без завершающего `/`, например `"https://api-testnet.bybit.com"`
    pub fn new(key: &str, secret: &str, base_url: &str) -> Result<Self> {
        if !base_url.starts_with("http") {
            return Err(anyhow!("Invalid Bybit URL: {}", base_url));
        }
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow!("HTTP client build error: {}", e))?;

        Ok(Self {
            api_key:     key.to_string(),
            api_secret:  secret.to_string(),
            client,
            base_url:    base_url.trim_end_matches('/').to_string(),
            recv_window: 5_000,
        })
    }

    /// Конструирует полный URL для REST-запроса
    fn build_url(&self, endpoint: &str) -> String {
        let ep = endpoint.trim_start_matches('/');
        format!("{}/{}", self.base_url, ep)
    }

    /// Текущее время в миллисекундах
    fn timestamp() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string()
    }

    /// Вычисляет HMAC-SHA256 подпись в HEX
    fn sign_payload(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Собирает заголовки X-BAPI-*, принимая строку queryString и bodyString
    fn auth_headers(&self, query: &str, body: &str) -> Vec<(&str, String)> {
        let ts = Self::timestamp();
        let rw = self.recv_window.to_string();
        let to_sign = format!(
            "{}{}{}{}",
            ts,
            self.api_key,
            rw,
            if !query.is_empty() { query } else { body }
        );
        let sign = self.sign_payload(&to_sign);
        vec![
            ("X-BAPI-API-KEY",     self.api_key.clone()),
            ("X-BAPI-TIMESTAMP",   ts),
            ("X-BAPI-RECV-WINDOW", rw),
            ("X-BAPI-SIGN",        sign),
        ]
    }

    /// Универсальный вызов API: метод, endpoint (`"v5/..."`), query, body, auth?
    async fn call_api<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        endpoint: &str,
        query: Option<&[(&str, &str)]>,
        body: Option<Value>,
        auth: bool,
    ) -> Result<T> {
        let url = self.build_url(endpoint);
        let mut req = self.client.request(method.clone(), &url);

        // собираем queryString
        let qs = if let Some(q) = query {
            req = req.query(q);
            q.iter()
             .map(|(k,v)| format!("{}={}", k, v))
             .collect::<Vec<_>>()
             .join("&")
        } else {
            String::new()
        };

        // собираем bodyString
        let bs = if let Some(ref b) = body {
            let s = b.to_string();
            req = req.json(b);
            s
        } else {
            String::new()
        };

        if auth {
            for (h, v) in self.auth_headers(&qs, &bs) {
                req = req.header(h, v);
            }
        }

        let resp = req.send().await?;
        let api: ApiResponse<T> = resp.json().await?;
        if api.ret_code != 0 {
            Err(anyhow!("Bybit API error {}: {}", api.ret_code, api.ret_msg))
        } else {
            Ok(api.result)
        }
    }
}

#[async_trait::async_trait]
impl Exchange for Bybit {
    /// GET /v5/market/time — проверяем HTTP 200
    async fn check_connection(&mut self) -> Result<()> {
        let url = self.build_url("v5/market/time");
        tracing::info!("PING BYBIT → GET {}", url);

        let resp = self.client.get(&url).send().await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!("Bybit ping failed (status {}): {}", status, body);
            Err(anyhow!("Bybit ping failed: {}", status))
        }
    }

    /// GET /v5/account/wallet-balance?coin=<symbol>
    async fn get_balance(&self, symbol: &str) -> Result<Balance> {
        let res: BalanceResult = self
            .call_api(
                Method::GET,
                "v5/account/wallet-balance",
                Some(&[("coin", symbol)]),
                None::<Value>,
                true,
            )
            .await?;

        let entry = res
            .balances
            .into_iter()
            .find(|x| x.coin.eq_ignore_ascii_case(symbol))
            .ok_or_else(|| anyhow!("{} not found in balances", symbol))?;

        let free:   f64 = entry.available.parse()?;
        let wallet: f64 = entry.wallet.parse()?;
        let locked     = wallet - free;

        Ok(Balance { free, locked })
    }

    async fn get_mmr(&self, _symbol: &str) -> Result<f64>         { unimplemented!() }
    async fn get_funding_rate(&self, _symbol: &str, _days: u16) -> Result<f64> { unimplemented!() }
    async fn place_limit_order(
        &self, _symbol: &str, _side: OrderSide, _qty: f64, _price: f64
    ) -> Result<Order> { unimplemented!() }
    async fn place_market_order(
        &self, _symbol: &str, _side: OrderSide, _qty: f64
    ) -> Result<Order> { unimplemented!() }
    async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> Result<()> {
        unimplemented!()
    }
}
