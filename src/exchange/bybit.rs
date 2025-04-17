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

/// Клиент Bybit (prod / testnet)
#[derive(Debug, Clone)]
pub struct Bybit {
    api_key:    String,
    api_secret: String,
    client:     Client,
    base_url:   Url,
}

impl Bybit {
    /// key/secret – API‑ключи, use_testnet – переключение на sandbox
    pub fn new(key: &str, secret: &str, use_testnet: bool) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        let base_url = if use_testnet {
            Url::parse("https://api-testnet.bybit.com").unwrap()
        } else {
            Url::parse("https://api.bybit.com").unwrap()
        };

        Self {
            api_key:    key.to_owned(),
            api_secret: secret.to_owned(),
            client,
            base_url,
        }
    }

    /// Генерация подписи по V5 схеме: payload = timestamp + api_key + recv_window + query
    fn sign_v5(&self, timestamp: &str, recv_window: &str, query: &str) -> String {
        let payload = format!("{timestamp}{}{}{}", self.api_key, recv_window, query);
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Текущий UNIX‑timestamp в миллисекундах
    fn timestamp_ms() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string()
    }
}

#[async_trait::async_trait]
impl Exchange for Bybit {
    /// Проверяем подключение к публичному V5-эндпоинту
    async fn check_connection(&mut self) -> Result<()> {
        let url = self.base_url.join("/v5/public/time")?;
        let resp = self.client.get(url).send().await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("Bybit ping failed: {}", resp.status()))
        }
    }

    /// Получаем баланс по V5 API (UNIFIED wallet)
    async fn get_balance(&self, symbol: &str) -> Result<Balance> {
        #[derive(Deserialize)]
        struct V5Resp {
            retCode: i32,
            retMsg:  String,
            result:  WalletBalanceResult,
        }
        #[derive(Deserialize)]
        struct WalletBalanceResult {
            list: Vec<CoinBalance>,
        }
        #[derive(Deserialize)]
        struct CoinBalance {
            coin:                  String,
            totalAvailableBalance: String,
            // другие поля, если надо
        }

        let ts = Self::timestamp_ms();
        let recv = "5000";
        let query = format!("accountType=UNIFIED&coin={symbol}");
        let sign  = self.sign_v5(&ts, recv, &query);

        let url = self.base_url.join("/v5/account/wallet-balance")?;
        let resp = self.client
            .get(url)
            .header("X-BAPI-API-KEY",     &self.api_key)
            .header("X-BAPI-TIMESTAMP",   &ts)
            .header("X-BAPI-RECV-WINDOW", recv)
            .header("X-BAPI-SIGN",        sign)
            .query(&[("accountType","UNIFIED"),("coin",symbol)])
            .send().await?
            .json::<V5Resp>().await?;

        if resp.retCode != 0 {
            return Err(anyhow!("Bybit balance error {}: {}", resp.retCode, resp.retMsg));
        }
        let coin = resp.result.list.into_iter()
            .find(|c| c.coin.eq_ignore_ascii_case(symbol))
            .ok_or_else(|| anyhow!("No balance entry for {}", symbol))?;

        let free = coin.totalAvailableBalance.parse::<f64>()?;
        Ok(Balance { free, locked: 0.0 })
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

    async fn place_market_order(
        &self,
        _symbol: &str,
        _side: OrderSide,
        _qty: f64,
    ) -> Result<Order> {
        unimplemented!()
    }

    async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> Result<()> {
        unimplemented!()
    }
}