// src/exchange/bybit.rs

use super::Exchange;
use crate::exchange::types::{Balance, Order, OrderSide};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

#[derive(Deserialize)]
struct ApiResponse<T> {
    #[serde(rename = "retCode")]   ret_code:    i32,
    #[serde(rename = "retMsg")]    ret_msg:     String,
    result:                       T,
    #[serde(rename = "time")]      _server_time: i64,
}

#[derive(Deserialize)]
struct BalanceResult { balances: Vec<BalanceEntry> }

#[derive(Deserialize)]
struct BalanceEntry {
    pub coin: String,
    #[serde(rename = "walletBalance")]    pub wallet:    String,
    #[serde(rename = "availableBalance")] pub available: String,
}

#[derive(Deserialize)]
struct PositionResult { list: Vec<PositionEntry> }

#[derive(Deserialize)]
struct PositionEntry {
    #[serde(rename = "maintMargin")]   maint_margin:   String,
    #[serde(rename = "positionValue")] position_value: String,
}

#[derive(Deserialize)]
struct FundingResult { list: Vec<FundingEntry> }

#[derive(Deserialize)]
struct FundingEntry {
    #[serde(rename = "fundingRate")] rate: String,
}

#[derive(Deserialize)]
struct OrderApiResult {
    #[serde(rename = "orderId")]    id:      String,
    pub price:      String,
    pub qty:        String,
    pub side:       String,
    #[serde(rename = "createTime")] pub ts: i64,
}

#[derive(Serialize)]
struct OrderRequest<'a> {
    category:    &'a str,
    symbol:      &'a str,
    side:        String,
    orderType:   &'a str,
    qty:         f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    price:       Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeInForce: Option<&'a str>,
    orderLinkId: String,
}

#[derive(Debug, Clone)]
pub struct Bybit {
    api_key:     String,
    api_secret:  String,
    client:      Client,
    base_url:    String,
    recv_window: u64,
}

impl Bybit {
    pub fn new(key: &str, secret: &str, base_url: &str) -> Result<Self> {
        if !base_url.starts_with("http") { return Err(anyhow!("Invalid URL")) }
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow!(e))?;
        Ok(Self {
            api_key:     key.into(),
            api_secret:  secret.into(),
            client,
            base_url:    base_url.trim_end_matches('/').into(),
            recv_window: 5_000,
        })
    }

    fn url(&self, ep: &str) -> String {
        format!("{}/{}", self.base_url, ep.trim_start_matches('/'))
    }
    fn ts() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string()
    }
    fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
    fn auth(&self, q: &str, b: &str) -> Vec<(&str, String)> {
        let t = Self::ts();
        let r = self.recv_window.to_string();
        let sign = self.sign(&format!("{}{ }{}{}", t, &self.api_key, r, if !q.is_empty(){q}else{b}));
        vec![
            ("X-BAPI-API-KEY",     self.api_key.clone()),
            ("X-BAPI-TIMESTAMP",   t),
            ("X-BAPI-RECV-WINDOW", r),
            ("X-BAPI-SIGN",        sign),
        ]
    }

    async fn call_api<T: for<'de> Deserialize<'de>>(
        &self,
        m: Method,
        ep: &str,
        q: Option<&[(&str,&str)]>,
        b: Option<Value>,
        auth: bool,
    ) -> Result<T> {
        let url = self.url(ep);
        tracing::debug!(%url, method=%m, "Bybit→");
        let mut req = self.client.request(m.clone(), &url);
        let qs = if let Some(qs) = q { req = req.query(qs); qs.iter().map(|(k,v)| format!("{}={}",k,v)).collect::<Vec<_>>().join("&") } else { "".into() };
        let bs = if let Some(ref body) = b { let s = body.to_string(); req = req.json(body); s } else { "".into() };
        if auth { for (h,v) in self.auth(&qs,&bs) { req = req.header(h,v); } }
        let r = req.send().await?;
        let a: ApiResponse<T> = r.json().await?;
        if a.ret_code!=0 { Err(anyhow!("{}: {}", a.ret_code,a.ret_msg)) } else { Ok(a.result) }
    }
}

#[async_trait]
impl Exchange for Bybit {
    async fn check_connection(&mut self) -> Result<()> {
        let url = self.url("v5/market/time");
        tracing::info!(%url, "ping");
        let r = self.client.get(&url).send().await?;
        if r.status().is_success() { Ok(()) }
        else {
            let s = r.status(); let b = r.text().await.unwrap_or_default();
            tracing::error!(%s,%b,"ping failed"); Err(anyhow!("ping {}",s))
        }
    }

    async fn get_balance(&self, coin: &str) -> Result<Balance> {
        let res: BalanceResult = self
            .call_api(Method::GET, "v5/account/wallet-balance", Some(&[("coin", coin)]), None, true)
            .await?;
        let e = res.balances.into_iter()
            .find(|e| e.coin.eq_ignore_ascii_case(coin))
            .ok_or_else(|| anyhow!("no {}",coin))?;
        let f = e.available.parse::<f64>()?;
        let w = e.wallet.parse::<f64>()?;
        Ok(Balance { free: f, locked: w - f })
    }

    async fn get_mmr(&self, sym: &str) -> Result<f64> {
        let res: PositionResult = self
            .call_api(Method::GET, "v5/position/list", Some(&[("symbol", sym),("category","linear")]), None, true)
            .await?;
        let pos = res.list.into_iter().next().ok_or_else(|| anyhow!("no pos"))?;
        let mm = pos.maint_margin.parse::<f64>()?;
        let pv = pos.position_value.parse::<f64>()?;
        Ok(mm / pv)
    }

    async fn get_funding_rate(&self, sym: &str, days: u16) -> Result<f64> {
        let res: FundingResult = self
            .call_api(Method::GET, "v5/market/funding-rate-history", Some(&[("symbol", sym),("limit",&days.to_string())]), None, true)
            .await?;
        let sum = res.list.iter().map(|r| r.rate.parse::<f64>().unwrap_or(0.0)).sum::<f64>();
        Ok(sum / (res.list.len().max(1) as f64))
    }

    async fn place_limit_order(&self, symbol: &str, side: OrderSide, qty: f64, price: f64) -> Result<Order> {
        let body = OrderRequest {
            category:    "linear",
            symbol,
            side:        side.to_string(),
            orderType:   "Limit",
            qty,
            price:       Some(price),
            timeInForce: Some("GoodTillCancel"),
            orderLinkId: Uuid::new_v4().to_string(),
        };
        let r: OrderApiResult = self.call_api(Method::POST, "v5/order/create", None, Some(json!(body)), true).await?;
        Ok(Order {
            id:    r.id,
            side:  r.side.parse()?,
            qty:   r.qty.parse()?,
            price: Some(r.price.parse()?),
            ts:    r.ts,
        })
    }

    async fn place_market_order(&self, symbol: &str, side: OrderSide, qty: f64) -> Result<Order> {
        let body = OrderRequest {
            category:    "linear",
            symbol,
            side:        side.to_string(),
            orderType:   "Market",
            qty,
            price:       None,
            timeInForce: None,
            orderLinkId: Uuid::new_v4().to_string(),
        };
        let r: OrderApiResult = self.call_api(Method::POST, "v5/order/create", None, Some(json!(body)), true).await?;
        Ok(Order {
            id:    r.id,
            side:  r.side.parse()?,
            qty:   r.qty.parse()?,
            price: None,
            ts:    r.ts,
        })
    }

    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()> {
        let body = json!({
            "category":"linear","symbol":symbol,"orderId":order_id
        });
        let _: Value = self.call_api(Method::POST, "v5/order/cancel", None, Some(body), true).await?;
        Ok(())
    }
}
