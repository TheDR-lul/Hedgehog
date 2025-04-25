use anyhow::Result;
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

use crate::config::Config;
use crate::exchange::Exchange;
use crate::models::HedgeRequest; // Добавлено для реэкспорта, если нужно
use crate::storage::{Db, HedgeOperation}; // Добавлено для сигнатур функций

// Объявляем подмодули
mod common;
mod hedge;
mod params;
mod unhedge;

// --- Константы и Общие Типы ---

pub const ORDER_FILL_TOLERANCE: f64 = 1e-8;

// Основная структура Hedger остается здесь
#[derive(Clone)]
pub struct Hedger<E> {
    exchange: E,
    slippage: f64,
    max_wait: Duration,
    quote_currency: String,
    config: Config, // Оставляем Config здесь, т.к. он нужен в разных местах
}

// Параметры, возвращаемые калькулятором
#[derive(Debug)]
pub struct HedgeParams {
    pub spot_order_qty: f64,
    pub fut_order_qty: f64,
    pub current_spot_price: f64,
    pub initial_limit_price: f64, // Цена для первого спот ордера
    pub symbol: String,
    pub spot_value: f64, // Расчетное значение спота
    pub available_collateral: f64, // Расчетный доступный коллатерал
    // Добавляем информацию, нужную для циклов ордеров
    pub min_spot_qty_decimal: Decimal,
    pub min_fut_qty_decimal: Decimal,
    pub spot_decimals: u32,
    pub fut_decimals: u32,
    pub futures_symbol: String, // Добавим сразу символ фьючерса
}

// Этапы операции
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HedgeStage {
    Spot,
    Futures,
}

// Структура для колбэка прогресса
#[derive(Debug, Clone)]
pub struct HedgeProgressUpdate {
    pub stage: HedgeStage,
    pub current_spot_price: f64, // Может быть цена спота или фьючерса в зависимости от stage
    pub new_limit_price: f64,
    pub is_replacement: bool,
    pub filled_qty: f64, // Исполнено в текущем ордере
    pub target_qty: f64, // Цель текущего ордера
    pub cumulative_filled_qty: f64, // Общее исполненное количество на данном этапе
    pub total_target_qty: f64, // Общая цель этапа (спот или фьючерс)
}

// Тип колбэка
pub type HedgeProgressCallback =
    Box<dyn FnMut(HedgeProgressUpdate) -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync>;

// --- Реализация Hedger ---

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

    // Используем функции из подмодулей
    pub async fn calculate_hedge_params(&self, req: &HedgeRequest) -> Result<HedgeParams> {
        params::calculate_hedge_params_impl(
            &self.exchange,
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
        current_order_id_storage: Arc<TokioMutex<Option<String>>>,
        total_filled_qty_storage: Arc<TokioMutex<f64>>,
        operation_id: i64,
        db: &Db,
    ) -> Result<(f64, f64, f64)> { // (spot_filled, fut_filled, spot_value_estimate)
        hedge::run_hedge_impl(
            self, // Передаем всего Hedger, чтобы иметь доступ к exchange, max_wait и т.д.
            params,
            progress_callback,
            current_order_id_storage,
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
    ) -> Result<(f64, f64)> { // (spot_sold, fut_bought)
        unhedge::run_unhedge_impl(
            self, // Передаем всего Hedger
            original_op,
            db,
            progress_callback,
        )
        .await
    }
}

// Реэкспорт для удобства использования извне модуля hedger
pub use hedge::run_hedge; // Если хотим прямой доступ к run_hedge
pub use params::calculate_hedge_params; // Если хотим прямой доступ
pub use unhedge::run_unhedge; // Если хотим прямой доступ
