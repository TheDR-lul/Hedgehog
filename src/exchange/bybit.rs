// src/exchange/bybit.rs

use super::Exchange;
use crate::exchange::types::{Balance, OrderSide, Order};
use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use reqwest::{Client, Url};
use serde::Deserialize;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// Клиент Bybit (spot/futures), умеет работать с любым base_url
#[derive(Debug, Clone)]
pub struct Bybit {
    api_key: String,
    api_secret: String,
    client: Client,
    base_url: Url,
}

impl Bybit {
    /// Создаёт клиента, разбирая строку `base_url`
    pub fn new(key: &str, secret: &str, base_url: &str) -> Result<Self> {
        let base_url = Url::parse(base_url)
            .map_err(|e| anyhow!("Invalid Bybit base URL `{}`: {}", base_url, e))?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow!("Failed to build HTTP client: {}", e))?;

        Ok(Self {
            api_key: key.to_string(),
            api_secret: secret.to_string(),
            client,
            base_url,
        })
    }

    /// Текущий миллисекундный таймстамп
    fn timestamp() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string()
    }

    /// Подписывает параметры HMAC-SHA256 и добавляет параметр `sign`
    fn sign(&self, params: &mut Vec<(&str, String)>) {
        let payload = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());
        params.push(("sign", signature));
    }
}

#[async_trait::async_trait]
impl Exchange for Bybit {
    /// Проверяем соединение через V5 `/v5/public/time`
    async fn check_connection(&mut self) -> Result<()> {
        let url = self.base_url.join("/v5/public/time")?;
        let resp = self.client.get(url).send().await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("Bybit ping failed: {}", resp.status()))
        }
    }

    /// Баланс по символу (например, "USDT")
    async fn get_balance(&self, symbol: &str) -> Result<Balance> {
        #[derive(Deserialize)]
        struct Resp {
            ret_code: i32,
            result: std::collections::HashMap<String, Data>,
        }
        #[derive(Deserialize)]
        struct Data {
            available_balance: String,
            locked_balance:    String,
        }

        let mut params = vec![
            ("api_key",     self.api_key.clone()),
            ("timestamp",   Self::timestamp()),
            ("recv_window", "5000".into()),
        ];
        self.sign(&mut params);

        let url = self.base_url.join("/v2/private/wallet/balance")?;
        let resp: Resp = self
            .client
            .get(url)
            .query(&params)
            .send()
            .await?
            .json()
            .await?;

        if resp.ret_code != 0 {
            return Err(anyhow!("Bybit balance error: code {}", resp.ret_code));
        }
        let entry = resp.result.get(symbol)
            .ok_or_else(|| anyhow!("Symbol {} not found in balance", symbol))?;
        let free   = entry.available_balance.parse()?;
        let locked = entry.locked_balance.parse()?;

        Ok(Balance { free, locked })
    }

    async fn get_mmr(&self, _symbol: &str) -> Result<f64> {
        unimplemented!()
    }
    async fn get_funding_rate(&self, _symbol: &str, _days: u16) -> Result<f64> {
        unimplemented!()
    }
    async fn place_limit_order(
        &self,
        _symbol: &str,
        _side: OrderSide,
        _qty: f64,
        _price: f64,
    ) -> Result<Order> {
        unimplemented!()
    }
    async fn place_market_order(&self, _symbol: &str, _side: OrderSide, _qty: f64) -> Result<Order> {
        unimplemented!()
    }
    async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> Result<()> {
        unimplemented!()
    }
}
