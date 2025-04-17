// src/exchange/bybit.rs

use super::Exchange;
use crate::exchange::types::{Balance, OrderSide, Order};
use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use reqwest::{Client, Method, Url};
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

/// Клиент Bybit V5
#[derive(Debug, Clone)]
pub struct Bybit {
    api_key:     String,
    api_secret:  String,
    client:      Client,
    base_url:    Url,
    recv_window: u64,
}

impl Bybit {
    /// `base_url` без завершающего `/`
    pub fn new(key: &str, secret: &str, base_url: &str) -> Result<Self> {
        let base_url = Url::parse(base_url)
            .map_err(|e| anyhow!("Invalid Bybit URL `{}`: {}", base_url, e))?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow!("HTTP client build error: {}", e))?;

        Ok(Self {
            api_key:     key.to_string(),
            api_secret:  secret.to_string(),
            client,
            base_url,
            recv_window: 5_000,
        })
    }

    fn timestamp() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string()
    }

    fn sign_payload(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn auth_headers(&self, query: &str, body: &str) -> Vec<(&str, String)> {
        let ts = Self::timestamp();
        let rw = self.recv_window.to_string();
        let to_sign = format!("{}{}{}{}", ts, self.api_key, rw, if !query.is_empty() { query } else { body });
        let sign = self.sign_payload(&to_sign);
        vec![
            ("X-BAPI-API-KEY",     self.api_key.clone()),
            ("X-BAPI-TIMESTAMP",   ts),
            ("X-BAPI-RECV-WINDOW", rw),
            ("X-BAPI-SIGN",        sign),
        ]
    }

    async fn call_api<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        endpoint: &str,
        query: Option<&[(&str, &str)]>,
        body: Option<Value>,
        auth: bool,
    ) -> Result<T> {
        let mut req = self.client.request(method.clone(), self.base_url.join(endpoint)?);

        // сборка строк для подписи
        let qs = if let Some(q) = query {
            req = req.query(q);
            q.iter()
             .map(|(k,v)| format!("{}={}", k, v))
             .collect::<Vec<_>>()
             .join("&")
        } else {
            String::new()
        };

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
    /// GET /v5/public/time
    async fn check_connection(&mut self) -> Result<()> {
        let _ : Value = self
            .call_api(Method::GET, "v5/public/time", None, None::<Value>, false)
            .await?;
        Ok(())
    }

    /// GET /v5/account/wallet-balance?coin=...
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

        // найдём нужную монету
        let entry = res.balances
            .into_iter()
            .find(|x| x.coin.eq_ignore_ascii_case(symbol))
            .ok_or_else(|| anyhow!("{} not found in balances", symbol))?;

        // распарсим строки в f64
        let free:   f64 = entry.available.parse()?;
        let wallet: f64 = entry.wallet.parse()?;
        let locked     = wallet - free;

        Ok(Balance { free, locked })
    }

    async fn get_mmr(&self, _symbol: &str) -> Result<f64>         { unimplemented!() }
    async fn get_funding_rate(&self, _symbol: &str, _days: u16) -> Result<f64> { unimplemented!() }
    async fn place_limit_order(&self, _symbol: &str, _side: OrderSide, _qty: f64, _price: f64) -> Result<Order> {
        unimplemented!()
    }
    async fn place_market_order(&self, _symbol: &str, _side: OrderSide, _qty: f64) -> Result<Order> {
        unimplemented!()
    }
    async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> Result<()> {
        unimplemented!()
    }
}
