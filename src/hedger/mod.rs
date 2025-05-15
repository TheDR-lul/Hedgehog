// src/hedger/mod.rs
use anyhow::Result;
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

use crate::config::Config;
use crate::exchange::Exchange;
use crate::models::HedgeRequest;
use crate::storage::{Db, HedgeOperation};

mod common;
mod hedge;
pub mod params; // ИЗМЕНЕНО: делаем модуль params публичным
mod unhedge;

pub const ORDER_FILL_TOLERANCE: f64 = 1e-8;

#[derive(Clone)]
pub struct Hedger<E> {
    exchange: E,
    slippage: f64,
    max_wait: Duration,
    quote_currency: String,
    config: Config,
}

#[derive(Debug, Clone)] // Добавил Clone для HedgeParams
pub struct HedgeParams {
    pub spot_order_qty: f64,
    pub fut_order_qty: f64,
    pub current_spot_price: f64,
    pub initial_limit_price: f64,
    pub symbol: String,
    pub spot_value: f64,
    pub available_collateral: f64,
    pub min_spot_qty_decimal: Decimal,
    pub min_fut_qty_decimal: Decimal,
    pub spot_decimals: u32,
    pub fut_decimals: u32,
    pub futures_symbol: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HedgeStage {
    Spot,
    Futures,
}

#[derive(Debug, Clone)]
pub struct HedgeProgressUpdate {
    pub stage: HedgeStage,
    pub current_spot_price: f64,
    pub new_limit_price: f64,
    pub is_replacement: bool,
    pub filled_qty: f64,
    pub target_qty: f64,
    pub cumulative_filled_qty: f64,
    pub total_target_qty: f64,
}

pub type HedgeProgressCallback =
    Box<dyn FnMut(HedgeProgressUpdate) -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync>;

impl<E> Hedger<E>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    pub fn new(exchange: E, config: Config) -> Self {
        Self {
            exchange,
            slippage: config.slippage,
            max_wait: Duration::from_secs(config.max_wait_secs),
            quote_currency: config.quote_currency.clone(),
            config,
        }
    }

    // Этот метод теперь основной для внешнего использования, если есть экземпляр Hedger
    pub async fn calculate_hedge_params(&self, req: &HedgeRequest) -> Result<HedgeParams> {
        params::calculate_hedge_params_impl(
            &self.exchange, // self.exchange здесь это E, которое Clone + Exchange
            req,
            self.slippage,
            &self.quote_currency,
            self.config.max_allowed_leverage,
        )
        .await
    }

    pub async fn run_hedge(
        &self,
        params: HedgeParams,
        progress_callback: HedgeProgressCallback,
        total_filled_qty_storage: Arc<TokioMutex<f64>>,
        operation_id: i64,
        db: &Db,
    ) -> Result<(f64, f64, f64)> {
        hedge::run_hedge_impl(
            self,
            params,
            progress_callback,
            total_filled_qty_storage,
            operation_id,
            db,
        )
        .await
    }

    pub async fn run_unhedge(
        &self,
        original_op: HedgeOperation,
        db: &Db,
        progress_callback: HedgeProgressCallback,
    ) -> Result<(f64, f64)> {
        unhedge::run_unhedge_impl(
            self,
            original_op,
            db,
            progress_callback,
        )
        .await
    }
}