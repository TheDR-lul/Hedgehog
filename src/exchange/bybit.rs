// src/exchange/bybit.rs

use super::{Exchange, OrderStatus};
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

/// Универсальная обёртка для ответов Bybit API v5
#[derive(Deserialize)]
struct ApiResponse<T> {
    #[serde(rename = "retCode")]    ret_code:     i32,
    #[serde(rename = "retMsg")]     ret_msg:      String,
    result:                          T,
    #[serde(rename = "time")]       _server_time: i64,
}

/// Ответ по балансу: список Unified‑аккаунтов
#[derive(Deserialize)]
struct BalanceResult {
    #[serde(rename = "list")]
    pub list: Vec<UnifiedAccountBalance>,
}

/// Один Unified‑аккаунт с массивом монет
#[derive(Deserialize)]
struct UnifiedAccountBalance {
    #[serde(rename = "accountType")]
    pub account_type: String,
    #[serde(rename = "coin")]
    pub coins: Vec<BalanceEntry>,
}

/// Запись баланса для конкретной монеты
#[derive(Deserialize)]
struct BalanceEntry {
    #[serde(rename = "coin")]
    pub coin: String,
    #[serde(rename = "walletBalance")]
    pub wallet: String,
    #[serde(rename = "locked")]
    pub locked: String,
}

/// Ответ по позициям
#[derive(Deserialize)]
struct PositionResult { pub list: Vec<PositionEntry> }

#[derive(Deserialize)]
struct PositionEntry {
    #[serde(rename = "maintMargin")]   pub maint_margin:   String,
    #[serde(rename = "positionValue")] pub position_value: String,
}

/// Ответ по funding‑rate
#[derive(Deserialize)]
struct FundingResult { pub list: Vec<FundingEntry> }

#[derive(Deserialize)]
struct FundingEntry { #[serde(rename = "fundingRate")] pub rate: String }

/// Ответ при создании/запросе ордера
#[derive(Deserialize)]
struct OrderApiResult {
    #[serde(rename = "orderId")]     pub id:   String,
    pub price:      String,
    pub qty:        String,
    pub side:       String,
    #[serde(rename = "createTime")]  pub ts:    i64,
}

/// Тело запроса на размещение ордера
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

/// Ответ по тикеру (спот‑цена)
#[derive(Deserialize)]
struct TickerResult {
    pub price: String,
}

/// Ответ при запросе статуса ордера
#[derive(Deserialize)]
struct OrderQueryList {
    #[serde(rename = "orderList")]
    pub list: Vec<OrderQueryEntry>,
}

#[derive(Deserialize)]
struct OrderQueryEntry {
    #[serde(rename = "cumExecQty")] pub cum_exec_qty: String,
    #[serde(rename = "leavesQty")]  pub leaves_qty:   String,
}

/// Клиент Bybit
#[derive(Debug, Clone)]
pub struct Bybit {
    api_key:     String,
    api_secret:  String,
    client:      Client,
    base_url:    String,
    recv_window: u64,
}

impl Bybit {
    /// Создаёт новый экземпляр клиента
    pub fn new(key: &str, secret: &str, base_url: &str) -> Result<Self> {
        if !base_url.starts_with("http") {
            return Err(anyhow!("Invalid URL"));
        }
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

    fn auth(&self, qs: &str, body: &str) -> Vec<(&str, String)> {
        let t = Self::ts();
        let rw = self.recv_window.to_string();
        let payload = format!("{}{ }{}{}", t, &self.api_key, rw, if !qs.is_empty() { qs } else { body });
        let sign = self.sign(&payload);
        vec![
            ("X-BAPI-API-KEY",    self.api_key.clone()),
            ("X-BAPI-TIMESTAMP",  t),
            ("X-BAPI-RECV-WINDOW",rw),
            ("X-BAPI-SIGN",       sign),
        ]
    }

    /// Универсальный вызов Bybit API с логированием "сырого" JSON
    async fn call_api<T: for<'de> Deserialize<'de>>(
        &self,
        method:   Method,
        endpoint: &str,
        query:    Option<&[(&str, &str)]>,
        body:     Option<Value>,
        auth:     bool,
    ) -> Result<T> {
        let url = self.url(endpoint);
        tracing::debug!(%url, method=%method, "Bybit→");
        let mut req = self.client.request(method.clone(), &url);

        // Собираем query-string
        let qs = if let Some(q) = query {
            req = req.query(q);
            q.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("&")
        } else {
            String::new()
        };

        // Собираем тело
        let bs = if let Some(ref b) = body {
            let s = b.to_string();
            req = req.json(b);
            s
        } else {
            String::new()
        };

        // Добавляем заголовки аутентификации
        if auth {
            for (h, v) in self.auth(&qs, &bs) {
                req = req.header(h, v);
            }
        }

        // Отправляем и логируем сырое тело ответа
        let resp = req.send().await?;
        let raw  = resp.text().await?;
        tracing::info!(%raw, "Raw Bybit response");

        // Десериализуем и проверяем код
        let api: ApiResponse<T> = serde_json::from_str(&raw)?;
        if api.ret_code != 0 {
            Err(anyhow!("Bybit API error {}: {}", api.ret_code, api.ret_msg))
        } else {
            Ok(api.result)
        }
    }
}

#[async_trait]
impl Exchange for Bybit {
    async fn check_connection(&mut self) -> Result<()> {
        let url = self.url("v5/market/time");
        tracing::info!(%url, "ping");
        let res = self.client.get(&url).send().await?;
        if res.status().is_success() {
            Ok(())
        } else {
            let status = res.status();
            let body   = res.text().await.unwrap_or_default();
            tracing::error!(%status, %body, "ping failed");
            Err(anyhow!("ping {}", status))
        }
    }

    async fn get_balance(&self, coin: &str) -> Result<Balance> {
        let all = self.get_all_balances().await?;
        all.into_iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(coin))
            .map(|(_, b)| b)
            .ok_or_else(|| anyhow!("no balance for {}", coin))
    }

    async fn get_all_balances(&self) -> Result<Vec<(String, Balance)>> {
        let res: BalanceResult = self.call_api(
            Method::GET,
            "v5/account/wallet-balance",
            Some(&[("accountType", "UNIFIED")]),
            None,
            true,
        )
        .await?;

        let unified = res
            .list
            .into_iter()
            .find(|acc| acc.account_type.eq_ignore_ascii_case("UNIFIED"))
            .ok_or_else(|| anyhow!("no unified account"))?;

        unified
            .coins
            .into_iter()
            .map(|e| {
                let total = e.wallet.parse::<f64>()?;
                let locked = e.locked.parse::<f64>()?;
                Ok((e.coin.clone(), Balance { free: total - locked, locked }))
            })
            .collect()
    }

    async fn get_mmr(&self, sym: &str) -> Result<f64> {
        let pos: PositionResult = self
            .call_api(
                Method::GET,
                "v5/position/list",
                Some(&[("symbol", sym), ("category", "linear")]),
                None,
                true,
            )
            .await?;
        let p = pos.list.into_iter().next().ok_or_else(|| anyhow!("no pos for {}", sym))?;
        let mm = p.maint_margin.parse::<f64>()?;
        let pv = p.position_value.parse::<f64>()?;
        Ok(mm / pv)
    }

    async fn get_funding_rate(&self, sym: &str, days: u16) -> Result<f64> {
        let fr: FundingResult = self
            .call_api(
                Method::GET,
                "v5/market/funding-rate-history",
                Some(&[("symbol", sym), ("limit", &days.to_string())]),
                None,
                true,
            )
            .await?;
        let sum: f64 = fr.list.iter().map(|r| r.rate.parse().unwrap_or(0.0)).sum();
        Ok(sum / (fr.list.len().max(1) as f64))
    }

    async fn place_limit_order(
        &self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
        price: f64,
    ) -> Result<Order> {
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
        let o: OrderApiResult = self
            .call_api(Method::POST, "v5/order/create", None, Some(json!(body)), true)
            .await?;
        Ok(Order {
            id:    o.id,
            side:  o.side.parse()?,
            qty:   o.qty.parse()?,
            price: Some(o.price.parse()?),
            ts:    o.ts,
        })
    }

    async fn place_market_order(
        &self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
    ) -> Result<Order> {
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
        let o: OrderApiResult = self
            .call_api(Method::POST, "v5/order/create", None, Some(json!(body)), true)
            .await?;
        Ok(Order {
            id:    o.id,
            side:  o.side.parse()?,
            qty:   o.qty.parse()?,
            price: None,
            ts:    o.ts,
        })
    }

    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()> {
        let body = json!({ "category": "linear", "symbol": symbol, "orderId": order_id });
        let _: Value = self.call_api(Method::POST, "v5/order/cancel", None, Some(body), true).await?;
        Ok(())
    }

    async fn get_spot_price(&self, symbol: &str) -> Result<f64> {
        // Получаем спотовую цену через ticker
        let tick: ApiResponse<TickerResult> = self
            .call_api(Method::GET, "v5/market/ticker/price", Some(&[("symbol", symbol)]), None, false)
            .await?;
        Ok(tick.result.price.parse()?)
    }

    async fn get_order_status(&self, symbol: &str, order_id: &str) -> Result<OrderStatus> {
        let oq: ApiResponse<OrderQueryList> = self
            .call_api(
                Method::GET,
                "v5/order",
                Some(&[("symbol", symbol), ("orderId", order_id)]),
                None,
                true,
            )
            .await?;
        let e = oq.result.list.into_iter().next().ok_or_else(|| anyhow!("no order"))?;
        let filled    = e.cum_exec_qty.parse()?;
        let remaining = e.leaves_qty.parse()?;
        Ok(OrderStatus { filled_qty: filled, remaining_qty: remaining })
    }
}
