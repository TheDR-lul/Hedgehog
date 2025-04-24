// src/hedger.rs

use anyhow::{anyhow, Result};
use tracing::{debug, error, info, warn};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use futures::future::BoxFuture;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
// --- ДОБАВЛЕНО: Импорт Config и Decimal ---
use crate::config::Config;
use rust_decimal::prelude::*;
use std::str::FromStr;
// --- КОНЕЦ ДОБАВЛЕНИЯ ---

use crate::exchange::{Exchange, OrderStatus};
use crate::exchange::bybit::{SPOT_CATEGORY, LINEAR_CATEGORY};
use crate::exchange::types::OrderSide;
use crate::models::{HedgeRequest, UnhedgeRequest};
use crate::storage::{Db, update_hedge_spot_order, update_hedge_final_status};

pub const ORDER_FILL_TOLERANCE: f64 = 1e-8; // Допуск для сравнения float

#[derive(Clone)]
pub struct Hedger<E> {
    exchange:     E,
    slippage:     f64,
    max_wait:     Duration,
    quote_currency: String,
    config: Config, // Храним конфиг
}

#[derive(Debug)]
pub struct HedgeParams {
    pub spot_order_qty: f64, // Округленное
    pub fut_order_qty: f64,  // Равно spot_order_qty
    pub current_spot_price: f64,
    pub initial_limit_price: f64,
    pub symbol: String,
    pub spot_value: f64, // Скорректированное значение спота
    pub available_collateral: f64, // Исходный остаток для залога
}

#[derive(Debug, Clone)]
pub struct HedgeProgressUpdate {
    pub current_spot_price: f64,
    pub new_limit_price: f64,
    pub is_replacement: bool,
    pub filled_qty: f64,
    pub target_qty: f64,
}

pub type HedgeProgressCallback = Box<dyn FnMut(HedgeProgressUpdate) -> BoxFuture<'static, Result<()>> + Send + Sync>;

impl<E> Hedger<E>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    /// Создаем Hedger, передавая Exchange и Config
    pub fn new(exchange: E, config: Config) -> Self {
        Self {
            exchange,
            slippage: config.slippage,
            max_wait: Duration::from_secs(config.max_wait_secs),
            quote_currency: config.quote_currency.clone(),
            config, // Сохраняем конфиг
        }
    }

    /// Расчет параметров хеджирования с учетом точности и проверкой плеча
    pub async fn calculate_hedge_params(&self, req: &HedgeRequest) -> Result<HedgeParams> {
        let HedgeRequest { sum, symbol, volatility } = req;
        debug!(
            "Calculating hedge params for {} with sum={}, volatility={}",
            symbol, sum, volatility
        );

        // Шаг 1: Получаем информацию для обоих рынков
        let spot_info = self.exchange.get_spot_instrument_info(symbol).await
            .map_err(|e| anyhow!("Failed to get SPOT instrument info: {}", e))?;
        let linear_info = self.exchange.get_linear_instrument_info(symbol).await
             .map_err(|e| anyhow!("Failed to get LINEAR instrument info: {}", e))?;

        // ... (Логирование комиссий, если нужно) ...

        // Шаг 2: Расчет SpotValue и AvailableCollateral
        let mmr = self.exchange.get_mmr(symbol).await?;
        let spot_value = sum / ((1.0 + volatility) * (1.0 + mmr));
        let available_collateral = sum - spot_value;
        debug!("Initial calculated values: spot_value={}, available_collateral={}, MMR={}", spot_value, available_collateral, mmr);

        if spot_value <= 0.0 || available_collateral <= 0.0 {
             error!("Calculated values are non-positive: spot={}, collateral={}", spot_value, available_collateral);
             return Err(anyhow::anyhow!("Calculated values are non-positive"));
        }

        let current_spot_price = self.exchange.get_spot_price(symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }

        // Шаг 3: Определяем точность фьючерсов
        let fut_qty_step_str = linear_info.lot_size_filter.qty_step.as_deref()
             .ok_or_else(|| anyhow!("Missing qtyStep for linear symbol {}", linear_info.symbol))?;
        let fut_decimals = fut_qty_step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        debug!("Futures quantity precision requires {} decimal places (qtyStep: {})", fut_decimals, fut_qty_step_str);

        // Рассчитываем идеальное кол-во
        let ideal_spot_qty = spot_value / current_spot_price;
        debug!("Ideal calculated quantity: {}", ideal_spot_qty);

        // Шаг 4 и 5: Округляем кол-во до точности фьючерсов и используем его
        let final_order_qty_decimal = Decimal::from_f64(ideal_spot_qty)
            .ok_or_else(|| anyhow!("Failed to convert ideal qty {} to Decimal", ideal_spot_qty))?
            .trunc_with_scale(fut_decimals); // Усекаем до нужной точности

        let final_order_qty = final_order_qty_decimal.to_f64()
             .ok_or_else(|| anyhow!("Failed to convert final qty {} back to f64", final_order_qty_decimal))?;

        // Используем это финальное количество для обоих ордеров
        let spot_order_qty = final_order_qty;
        let fut_order_qty = final_order_qty;

        // Пересчитываем SpotValue для консистентности в расчетах плеча
        let adjusted_spot_value = final_order_qty * current_spot_price;
        debug!(
            "Final quantities rounded to futures precision: spot_qty={}, fut_qty={}",
             spot_order_qty, fut_order_qty
        );
        debug!("Adjusted spot value based on rounded quantity: {}", adjusted_spot_value);

        let initial_limit_price = current_spot_price * (1.0 - self.slippage);
        debug!("Initial limit price: {}", initial_limit_price);

        // Шаг 6: Проверки с округленным количеством
        // Проверка на минимальное количество (фьючерсы)
        let min_fut_qty_str = &linear_info.lot_size_filter.min_order_qty;
        let min_fut_qty = Decimal::from_str(min_fut_qty_str).unwrap_or(rust_decimal::Decimal::ZERO);
        if final_order_qty_decimal < min_fut_qty {
            error!("Final quantity {} is less than minimum futures order quantity {}", final_order_qty_decimal, min_fut_qty_str);
            return Err(anyhow!("Calculated quantity {:.8} is less than minimum futures quantity {}", final_order_qty, min_fut_qty_str));
        }
         if spot_order_qty <= 0.0 {
             error!("Final order quantity is non-positive.");
             return Err(anyhow::anyhow!("Final order quantity is non-positive"));
         }

        // Проверка требуемого плеча (используем adjusted_spot_value и исходный available_collateral)
        // Делаем проверку на available_collateral > 0 перед делением
        if available_collateral <= 0.0 {
            error!("Available collateral ({}) is zero or negative, cannot calculate required leverage.", available_collateral);
            return Err(anyhow!("Available collateral is zero or negative"));
        }
        let required_leverage = adjusted_spot_value / available_collateral;
        debug!("Calculated required leverage based on adjusted value: {}", required_leverage);

        if required_leverage.is_nan() || required_leverage.is_infinite() {
            error!("Required leverage calculation resulted in NaN or Infinity.");
             return Err(anyhow!("Invalid required leverage calculation"));
        }

        // Сравнение с максимальным плечом из конфига
        if required_leverage > self.config.max_allowed_leverage {
            error!(
                "Required leverage {:.2}x exceeds max allowed leverage {:.2}x from config.",
                required_leverage, self.config.max_allowed_leverage
            );
            return Err(anyhow!(
                "Required leverage {:.2}x exceeds max allowed {:.2}x",
                required_leverage, self.config.max_allowed_leverage
            ));
        }
        info!("Required leverage {:.2}x is within max allowed {:.2}x", required_leverage, self.config.max_allowed_leverage);

        Ok(HedgeParams {
            spot_order_qty, // Округленное
            fut_order_qty,  // Такое же округленное
            current_spot_price,
            initial_limit_price,
            symbol: symbol.clone(),
            spot_value: adjusted_spot_value, // Передаем скорректированное значение
            available_collateral, // Передаем исходный остаток
        })
    }

    /// Запуск хеджирования с проверкой и установкой плеча
    pub async fn run_hedge(
        &self,
        params: HedgeParams,
        mut progress_callback: HedgeProgressCallback,
        current_order_id_storage: Arc<TokioMutex<Option<String>>>,
        total_filled_qty_storage: Arc<TokioMutex<f64>>,
        operation_id: i64,
        db: &Db,
    ) -> Result<(f64, f64)>
    {
        let HedgeParams {
            spot_order_qty: initial_spot_qty, // Уже округлено
            fut_order_qty: initial_fut_qty,   // Уже округлено и равно spot_qty
            current_spot_price: mut current_spot_price,
            initial_limit_price: mut limit_price,
            symbol,
            spot_value, // Скорректированное значение спота
            available_collateral, // Исходный остаток для залога
        } = params;

        info!(
            "Running hedge op_id:{} for {} with spot_qty={}, fut_qty={}, spot_value={}, avail_collateral={}",
            operation_id, symbol, initial_spot_qty, initial_fut_qty, spot_value, available_collateral
        );

        // Шаги 3, 5 и 6: Проверка и Установка Плеча перед операциями
        // Используем значения, переданные из calculate_hedge_params
        if available_collateral <= 0.0 { // Доп. проверка
            error!("op_id:{}: Available collateral is non-positive. Aborting.", operation_id);
            let msg = "Available collateral is non-positive".to_string();
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
            return Err(anyhow!(msg));
        }
        let required_leverage = spot_value / available_collateral;
        info!("Confirming required leverage: {:.2}x", required_leverage);

        if required_leverage.is_nan() || required_leverage.is_infinite() {
            error!("op_id:{}: Invalid required leverage calculation. Aborting.", operation_id);
            let msg = "Invalid required leverage calculation".to_string();
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
            return Err(anyhow!(msg));
        }


        // Шаг 5: Получаем текущее плечо с биржи
        let current_leverage = match self.exchange.get_current_leverage(&symbol).await {
             Ok(l) => { info!("Current leverage on exchange for {}: {:.2}x", symbol, l); l }
             Err(e) => {
                 error!("op_id:{}: Failed to get current leverage: {}. Aborting hedge.", operation_id, e);
                 let msg = format!("Leverage check failed: {}", e);
                 let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
                 return Err(anyhow!(msg)); // Возвращаем ошибку
             }
        };

        // Шаг 5: Проверяем, достаточно ли текущего плеча
        if current_leverage < required_leverage - 0.01 { // Добавляем небольшой допуск
            error!(
                "op_id:{}: Current leverage on exchange {:.2}x is less than required {:.2}x. Please increase leverage manually on Bybit. Aborting hedge.",
                operation_id, current_leverage, required_leverage
            );
             let msg = format!("Current leverage {:.2}x < required {:.2}x", current_leverage, required_leverage);
             let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
             return Err(anyhow!(msg));
        }

        // Шаг 6: Устанавливаем плечо (пытаемся установить требуемое)
        // Округляем до 2 знаков, как требует Bybit
        let target_leverage_set = (required_leverage * 100.0).round() / 100.0;
        // Проверяем, нужно ли вообще устанавливать (избегаем лишних запросов)
        if (target_leverage_set - current_leverage).abs() > 0.01 {
             info!("op_id:{}: Attempting to set leverage to {:.2}x (required: {:.2}x)", operation_id, target_leverage_set, required_leverage);
             if let Err(e) = self.exchange.set_leverage(&symbol, target_leverage_set).await {
                 // Если установка не удалась - это критично
                 error!("op_id:{}: Failed to set leverage to {:.2}x: {}. Aborting hedge.", operation_id, target_leverage_set, e);
                 let msg = format!("Failed to set leverage: {}", e);
                 let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
                 return Err(anyhow!(msg)); // Возвращаем ошибку
             }
             info!("op_id:{}: Leverage set/confirmed to ~{:.2}x successfully.", operation_id, target_leverage_set);
             // Даем бирже немного времени на применение плеча перед размещением ордера
             sleep(Duration::from_millis(500)).await;
        } else {
             info!("op_id:{}: Current exchange leverage {:.2}x is sufficient. No need to set.", operation_id, current_leverage);
        }
        // --- Конец Шагов 3, 5, 6 ---


        // --- Дальнейшая логика (размещение ордеров, цикл) остается как в предыдущей версии ---
        let mut cumulative_filled_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty; // Уже округлено
        let mut qty_filled_in_current_order = 0.0;
        let mut current_spot_order_id: Option<String> = None;

        let update_current_order_id_local = |id: Option<String>| {
            let storage_clone = current_order_id_storage.clone();
            let filled_storage_clone = total_filled_qty_storage.clone();
            let db_clone = db.clone();
            async move {
                let id_clone_for_storage = id.clone();
                let id_clone_for_db = id.clone();
                let filled_qty_for_db = *filled_storage_clone.lock().await;
                *storage_clone.lock().await = id_clone_for_storage;
                if let Err(e) = update_hedge_spot_order(&db_clone, operation_id, id_clone_for_db.as_deref(), filled_qty_for_db).await {
                    error!("op_id:{}: Failed to update spot order info in DB: {}", operation_id, e);
                }
                id
            }
        };

        tokio::task::yield_now().await;
        info!("op_id:{}: Placing initial limit buy at {} for qty {}", operation_id, limit_price, current_order_target_qty);
        tokio::task::yield_now().await;
        let spot_order = match self
            .exchange
            .place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price)
            .await {
                Ok(o) => o,
                Err(e) => {
                    error!("op_id:{}: Failed to place initial order: {}", operation_id, e);
                    let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&e.to_string())).await;
                    return Err(e);
                }
            };
        info!("op_id:{}: Placed initial spot limit order: id={}", operation_id, spot_order.id);
        current_spot_order_id = update_current_order_id_local(Some(spot_order.id.clone())).await;
        *total_filled_qty_storage.lock().await = 0.0;

        let mut start = Instant::now();
        let mut last_update_sent = Instant::now();
        let update_interval = Duration::from_secs(5);

        let hedge_loop_result: Result<()> = async {
             loop {
                sleep(Duration::from_secs(1)).await;
                let now = Instant::now();

                let order_id_to_check = match &current_spot_order_id {
                    Some(id) => id.clone(),
                    None => {
                        if cumulative_filled_qty < initial_spot_qty - ORDER_FILL_TOLERANCE && now.duration_since(start) > Duration::from_secs(2) {
                            warn!("op_id:{}: No active order ID, but target not reached. Aborting hedge.", operation_id);
                            return Err(anyhow!("No active spot order, but target not reached after fill/cancellation."));
                        } else {
                             if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                info!("op_id:{}: No active order and target reached. Exiting loop.", operation_id);
                                break Ok(());
                             }
                            continue;
                        }
                    }
                };

                tokio::task::yield_now().await;
                let status_result = self.exchange.get_order_status(&symbol, &order_id_to_check).await;

                let status: OrderStatus = match status_result {
                    Ok(s) => s,
                    Err(e) => {
                        if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                            warn!("op_id:{}: Order {} not found after delay, assuming it filled for its target qty {}. Continuing...", operation_id, order_id_to_check, current_order_target_qty);
                            cumulative_filled_qty += current_order_target_qty;
                            *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                            current_spot_order_id = update_current_order_id_local(None).await;
                            if let Err(db_err) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                error!("op_id:{}: Failed to update spot filled qty in DB after order not found: {}", operation_id, db_err);
                            }
                            if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                info!("op_id:{}: Total filled spot quantity {} meets overall target {} after order not found assumption. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                                break Ok(());
                            } else {
                                warn!("op_id:{}: Assumed order filled, but overall target {} not reached. Triggering replacement logic.", operation_id, initial_spot_qty);
                                start = now - self.max_wait - Duration::from_secs(1);
                                qty_filled_in_current_order = 0.0;
                                continue;
                            }
                        } else {
                            warn!("op_id:{}: Failed to get order status for {}: {}. Aborting hedge.", operation_id, order_id_to_check, e);
                            return Err(anyhow!("Hedge failed during status check: {}", e));
                        }
                    }
                };

                let previously_filled_in_current = qty_filled_in_current_order;
                qty_filled_in_current_order = status.filled_qty;
                let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current;

                if filled_since_last_check.abs() > ORDER_FILL_TOLERANCE {
                    cumulative_filled_qty += filled_since_last_check;
                    cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_spot_qty * 1.01);
                    *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                    if let Err(e) = update_hedge_spot_order(db, operation_id, current_spot_order_id.as_deref(), cumulative_filled_qty).await {
                        error!("op_id:{}: Failed to update spot filled qty in DB: {}", operation_id, e);
                    }
                }

                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                    info!("op_id:{}: Spot order {} considered filled by exchange (remaining_qty: {}, current order filled: {}, total cumulative: {}). Exiting loop.",
                           operation_id, order_id_to_check, status.remaining_qty, status.filled_qty, cumulative_filled_qty);
                    current_spot_order_id = update_current_order_id_local(None).await;
                    break Ok(());
                }


                let elapsed_since_start = now.duration_since(start);
                let elapsed_since_update = now.duration_since(last_update_sent);

                let mut is_replacement = false;
                let mut price_for_update = current_spot_price;

                if elapsed_since_start > self.max_wait && status.remaining_qty > ORDER_FILL_TOLERANCE {
                    is_replacement = true;
                    warn!(
                        "op_id:{}: Spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                        operation_id, order_id_to_check, qty_filled_in_current_order, current_order_target_qty, self.max_wait
                    );

                    let remaining_total_qty = initial_spot_qty - cumulative_filled_qty;

                    tokio::task::yield_now().await;
                    if let Err(e) = self.exchange.cancel_order(&symbol, &order_id_to_check).await {
                        warn!("op_id:{}: Attempt to cancel order {} failed or was ignored: {}", operation_id, order_id_to_check, e);
                        sleep(Duration::from_millis(500)).await;
                    } else {
                        info!("op_id:{}: Successfully sent cancel request for order {}", operation_id, order_id_to_check);
                        sleep(Duration::from_millis(500)).await;
                    }
                    current_spot_order_id = update_current_order_id_local(None).await;
                    qty_filled_in_current_order = 0.0;

                     tokio::task::yield_now().await;
                     match self.exchange.get_order_status(&symbol, &order_id_to_check).await {
                         Ok(final_status) if final_status.remaining_qty <= ORDER_FILL_TOLERANCE => {
                             info!("op_id:{}: Order {} filled completely after cancel request. Adjusting cumulative qty.", operation_id, order_id_to_check);
                             let filled_after_cancel = final_status.filled_qty - previously_filled_in_current;
                             if filled_after_cancel > 0.0 {
                                 cumulative_filled_qty += filled_after_cancel;
                                 *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                                 if let Err(e) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                     error!("op_id:{}: Failed to update spot filled qty in DB after fill during cancel: {}", operation_id, e);
                                 }
                             }
                             if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                 info!("op_id:{}: Total filled quantity {} meets target {} after fill during cancel. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                                 break Ok(());
                             }
                         }
                         Ok(_) => { /* Order still active */ }
                         Err(e) => { warn!("op_id:{}: Failed to get order status after cancel request: {}", operation_id, e); }
                     }


                    if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                         info!("op_id:{}: Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                         break Ok(());
                    }


                    tokio::task::yield_now().await;
                    current_spot_price = match self.exchange.get_spot_price(&symbol).await {
                        Ok(p) if p > 0.0 => p,
                        Ok(p) => {
                             error!("op_id:{}: Received non-positive spot price during replacement: {}", operation_id, p);
                             return Err(anyhow!("Received non-positive spot price during replacement: {}", p));
                        }
                        Err(e) => {
                            warn!("op_id:{}: Failed to get spot price during replacement: {}. Aborting hedge.", operation_id, e);
                            return Err(anyhow!("Hedge failed during replacement (get price step): {}", e));
                        }
                    };

                    limit_price = current_spot_price * (1.0 - self.slippage);
                    price_for_update = current_spot_price;
                    current_order_target_qty = remaining_total_qty;

                    info!("op_id:{}: New spot price: {}, placing new limit buy at {} for remaining qty {}", operation_id, current_spot_price, limit_price, current_order_target_qty);
                    tokio::task::yield_now().await;
                    let new_spot_order = match self.exchange.place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price).await {
                        Ok(o) => o,
                        Err(e) => {
                            warn!("op_id:{}: Failed to place replacement order: {}. Aborting hedge.", operation_id, e);
                            return Err(anyhow!("Hedge failed during replacement (place order step): {}", e));
                        }
                    };
                    info!("op_id:{}: Placed replacement spot limit order: id={}", operation_id, new_spot_order.id);
                    current_spot_order_id = update_current_order_id_local(Some(new_spot_order.id.clone())).await;
                    start = now;
                }

                if is_replacement || elapsed_since_update > update_interval {
                    if !is_replacement {
                        tokio::task::yield_now().await;
                        match self.exchange.get_spot_price(&symbol).await {
                            Ok(p) if p > 0.0 => price_for_update = p,
                            Ok(_) => { /* Use old price */ }
                            Err(e) => {
                                warn!("op_id:{}: Failed to get spot price for update: {}", operation_id, e);
                            }
                        }
                    }

                    let update = HedgeProgressUpdate {
                        current_spot_price: price_for_update,
                        new_limit_price: limit_price,
                        is_replacement,
                        filled_qty: qty_filled_in_current_order,
                        target_qty: current_order_target_qty,
                    };

                    tokio::task::yield_now().await;
                    if let Err(e) = progress_callback(update).await {
                        if !e.to_string().contains("message is not modified") {
                            warn!("op_id:{}: Progress callback failed: {}. Continuing hedge...", operation_id, e);
                        }
                    }
                    last_update_sent = now;
                }
            }
        }.await;

        if let Err(loop_err) = hedge_loop_result {
            error!("op_id:{}: Hedge loop failed: {}", operation_id, loop_err);
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, cumulative_filled_qty, Some(&loop_err.to_string())).await;
            return Err(loop_err);
        }

        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "op_id:{}: Placing market sell order on futures ({}) for final quantity {}",
            operation_id, futures_symbol, initial_fut_qty // Используем финальное округленное initial_fut_qty
        );
        tokio::task::yield_now().await;
        let fut_order_result = self
            .exchange
            .place_futures_market_order(&futures_symbol, OrderSide::Sell, initial_fut_qty) // Передаем initial_fut_qty
            .await;

        match fut_order_result {
            Ok(fut_order) => {
                info!("op_id:{}: Placed futures market order: id={}", operation_id, fut_order.id);
                info!("op_id:{}: Hedge completed successfully. Total spot filled: {}", operation_id, cumulative_filled_qty);
                let _ = update_hedge_final_status(db, operation_id, "Completed", Some(&fut_order.id), initial_fut_qty, None).await;
                // Возвращаем округленные и равные initial_spot_qty и initial_fut_qty
                Ok((cumulative_filled_qty, initial_fut_qty))
            }
            Err(e) => {
                warn!("op_id:{}: Failed to place futures order: {}. Spot was already bought!", operation_id, e);
                let _ = update_hedge_final_status(db, operation_id, "Failed", None, cumulative_filled_qty, Some(&format!("Futures order failed: {}", e))).await;
                Err(anyhow!("Failed to place futures order after spot fill: {}", e))
            }
        }
    }


    // --- run_unhedge остается без изменений в этой итерации ---
    // (При необходимости в него можно добавить аналогичные проверки, но для unhedge
    // обычно не требуется сложный расчет плеча, т.к. закрывается существующая позиция)
     pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
        // ... (параметры для БД и ID)
    ) -> Result<(f64, f64)> {
        let UnhedgeRequest { quantity: initial_spot_qty, symbol } = req;
        let initial_fut_qty = initial_spot_qty;

         info!(
            "Starting unhedge for {} with initial_quantity={}",
            symbol, initial_spot_qty
        );

        let mut cumulative_filled_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty;
        let mut qty_filled_in_current_order = 0.0;
         let mut current_spot_order_id: Option<String> = None;

        if initial_spot_qty <= 0.0 {
             error!("Initial quantity is non-positive: {}. Aborting unhedge.", initial_spot_qty);
             return Err(anyhow::anyhow!("Initial quantity is non-positive"));
        }

        let mut current_spot_price = self.exchange.get_spot_price(&symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }

        let mut limit_price = current_spot_price * (1.0 + self.slippage);
        info!("Initial spot price: {}, placing limit sell at {} for qty {}", current_spot_price, limit_price, current_order_target_qty);
        let spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);
        current_spot_order_id = Some(spot_order.id.clone());
        let mut start = Instant::now();

        let unhedge_loop_result: Result<()> = async {
            loop {
                sleep(Duration::from_secs(1)).await;
                let now = Instant::now();

                 let order_id_to_check = match &current_spot_order_id {
                    Some(id) => id.clone(),
                    None => {
                        if cumulative_filled_qty < initial_spot_qty - ORDER_FILL_TOLERANCE && now.duration_since(start) > Duration::from_secs(2) {
                            warn!("No active unhedge order ID, but target not reached. Aborting unhedge.");
                            return Err(anyhow!("No active spot order, but target not reached after fill/cancellation during unhedge."));
                        } else if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                             info!("No active unhedge order and target reached. Exiting loop.");
                             break Ok(());
                        }
                        continue;
                    }
                };


                let status: OrderStatus = match self.exchange.get_order_status(&symbol, &order_id_to_check).await {
                    Ok(s) => s,
                    Err(e) => {
                        if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                            warn!("Unhedge order {} not found after delay, assuming it filled for its target qty {}. Continuing...", order_id_to_check, current_order_target_qty);
                            cumulative_filled_qty += current_order_target_qty;
                             current_spot_order_id = None;
                            if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                info!("Total filled spot quantity {} meets target {} after unhedge order not found assumption. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                                break Ok(());
                            } else {
                                warn!("Unhedge order filled assumption, but overall target {} not reached. Triggering replacement logic.", initial_spot_qty);
                                start = now - self.max_wait - Duration::from_secs(1);
                                qty_filled_in_current_order = 0.0;
                                continue;
                            }
                        } else {
                            warn!("Failed to get unhedge order status for {}: {}. Aborting unhedge.", order_id_to_check, e);
                            return Err(anyhow!("Unhedge failed during status check: {}", e));
                        }
                    }
                };

                let previously_filled_in_current = qty_filled_in_current_order;
                qty_filled_in_current_order = status.filled_qty;
                let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current;

                if filled_since_last_check.abs() > ORDER_FILL_TOLERANCE {
                    cumulative_filled_qty += filled_since_last_check;
                    cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_spot_qty * 1.01);
                }

                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                     info!("Unhedge spot order {} considered filled by exchange (remaining_qty: {}, current order filled: {}, total cumulative: {}). Exiting loop.",
                           order_id_to_check, status.remaining_qty, status.filled_qty, cumulative_filled_qty);
                     current_spot_order_id = None;
                     break Ok(());
                }

                if start.elapsed() > self.max_wait && status.remaining_qty > ORDER_FILL_TOLERANCE {
                    warn!(
                        "Unhedge spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                        order_id_to_check, qty_filled_in_current_order, current_order_target_qty, self.max_wait
                    );

                    let remaining_total_qty = initial_spot_qty - cumulative_filled_qty;

                    if let Err(e) = self.exchange.cancel_order(&symbol, &order_id_to_check).await {
                         warn!("Unhedge order {} cancellation failed (likely already inactive): {}", order_id_to_check, e);
                         sleep(Duration::from_millis(500)).await;
                    } else {
                        info!("Successfully sent cancel request for unhedge order {}", order_id_to_check);
                         sleep(Duration::from_millis(500)).await;
                    }
                    current_spot_order_id = None;
                    qty_filled_in_current_order = 0.0;

                     match self.exchange.get_order_status(&symbol, &order_id_to_check).await {
                         Ok(final_status) if final_status.remaining_qty <= ORDER_FILL_TOLERANCE => {
                              info!("Unhedge order {} filled completely after cancel request. Adjusting cumulative qty.", order_id_to_check);
                              let filled_after_cancel = final_status.filled_qty - previously_filled_in_current;
                              if filled_after_cancel > 0.0 {
                                  cumulative_filled_qty += filled_after_cancel;
                              }
                             if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                 info!("Total filled quantity {} meets target {} after fill during cancel. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                                 break Ok(());
                             }
                         }
                         Ok(_) => {}
                         Err(e) => { warn!("Failed to get unhedge order status after cancel request: {}", e); }
                     }


                    if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                        info!("Total filled quantity {} meets target {} during replacement. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                        break Ok(());
                    }

                    current_spot_price = match self.exchange.get_spot_price(&symbol).await {
                         Ok(p) if p > 0.0 => p,
                         Ok(p) => {
                             error!("Received non-positive spot price during unhedge replacement: {}", p);
                             return Err(anyhow!("Received non-positive spot price during unhedge replacement: {}", p));
                         }
                         Err(e) => {
                            warn!("Failed to get spot price during unhedge replacement: {}. Aborting unhedge.", e);
                            return Err(anyhow!("Unhedge failed during replacement (get price step): {}", e));
                         }
                    };

                    limit_price = current_spot_price * (1.0 + self.slippage);
                    current_order_target_qty = remaining_total_qty;

                    info!("New spot price: {}, placing new limit sell at {} for remaining qty {}", current_spot_price, limit_price, current_order_target_qty);
                    let new_spot_order = self
                        .exchange
                        .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
                        .await?;
                    info!("Placed replacement spot limit order: id={}", new_spot_order.id);
                    current_spot_order_id = Some(new_spot_order.id.clone());
                    start = Instant::now();
                }
            }
        }.await;

        if let Err(loop_err) = unhedge_loop_result {
             error!("Unhedge loop failed: {}", loop_err);
             return Err(loop_err);
        }

        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "Placing market buy order on futures ({}) for initial quantity {}",
            futures_symbol, initial_fut_qty
        );
        let fut_order_result = self
            .exchange
            .place_futures_market_order(&futures_symbol, OrderSide::Buy, initial_fut_qty)
            .await;

        match fut_order_result {
             Ok(fut_order) => {
                 info!("Placed futures market order: id={}", fut_order.id);
                 info!("Unhedge completed successfully for {}. Total spot filled: {}", symbol, cumulative_filled_qty);
                 // Возвращаем округленное значение спота и равное ему фьюча
                 Ok((cumulative_filled_qty, initial_fut_qty))
             }
             Err(e) => {
                 warn!("Failed to place futures order during unhedge: {}. Spot was already sold!", e);
                 Err(anyhow!("Failed to place futures order after spot sell: {}", e))
             }
        }
    }
}