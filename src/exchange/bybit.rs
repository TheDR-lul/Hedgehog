use super::Exchange;
use crate::exchange::types::*;
use anyhow::{Result, anyhow};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use reqwest::{Client, Url};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug)]
pub struct Bybit {
    api_key: String,
    api_secret: String,
    client: Client,
    base_url: Url,
}

impl Bybit {
    pub fn new(key: &str, secret: &str) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build().unwrap();
        let base_url = Url::parse("https://api.bybit.com").unwrap();
        Self { api_key: key.into(), api_secret: secret.into(), client, base_url }
    }

    fn sign(&self, params: &mut Vec<(&str, String)>) {
        let to_sign = params.iter()
            .map(|(k,v)| format!("{}={}", k, v))
            .collect::<Vec<_>>().join("&");
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes()).unwrap();
        mac.update(to_sign.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());
        params.push(("sign", signature));
    }

    fn timestamp() -> String {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().to_string()
    }
}

#[async_trait::async_trait]
impl Exchange for Bybit {
    async fn check_connection(&mut self) -> Result<()> {
        let url = self.base_url.join("/v2/public/time")?;
        let resp = self.client.get(url).send().await?;
        if resp.status().is_success() { Ok(()) }
        else { Err(anyhow!("Bybit ping failed: {}", resp.status())) }
    }
    async fn get_balance(&self, symbol: &str) -> Result<Balance> {
        #[derive(Deserialize)]
        struct Resp { ret_code: i32, result: std::collections::HashMap<String, Data> }
        #[derive(Deserialize)]
        struct Data { available_balance: String, locked_balance: String }

        let mut params = vec![
            ("api_key", self.api_key.clone()),
            ("timestamp", Self::timestamp()),
            ("recv_window", "5000".into()),
        ];
        self.sign(&mut params);
        let url = self.base_url.join("/v2/private/wallet/balance")?;
        let resp = self.client.get(url).query(&params).send().await?.json::<Resp>().await?;
        if resp.ret_code != 0 { return Err(anyhow!("Bybit balance error code {}", resp.ret_code)); }
        let entry = resp.result.get(symbol).ok_or_else(|| anyhow!("Balance for {} not found", symbol))?;
        Ok(Balance { free: entry.available_balance.parse()?, locked: entry.locked_balance.parse()? })
    }
    async fn get_mmr(&self, _symbol: &str) -> Result<f64> { unimplemented!() }
    async fn get_funding_rate(&self, _symbol: &str, _days: u16) -> Result<f64> { unimplemented!() }
    async fn place_limit_order(&self, _symbol: &str, _side: OrderSide, _qty: f64, _price: f64) -> Result<Order> { unimplemented!() }
    async fn place_market_order(&self, _symbol: &str, _side: OrderSide, _qty: f64) -> Result<Order> { unimplemented!() }
    async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> Result<()> { unimplemented!() }
}
