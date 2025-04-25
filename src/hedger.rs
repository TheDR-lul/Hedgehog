// src/hedger.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::exchange::bybit::SPOT_CATEGORY;
use crate::exchange::types::OrderSide;
use crate::exchange::{Exchange, OrderStatus};
use crate::models::HedgeRequest;
use crate::storage::{
    mark_hedge_as_unhedged, update_hedge_final_status, update_hedge_spot_order, Db,
    HedgeOperation,
};

pub const ORDER_FILL_TOLERANCE: f64 = 1e-8;

#[derive(Clone)]
pub struct Hedger<E> {
    exchange: E,
    slippage: f64,
    max_wait: Duration,
    quote_currency: String,
    config: Config,
}

#[derive(Debug)]
pub struct HedgeParams {
    pub spot_order_qty: f64,
    pub fut_order_qty: f64,
    pub current_spot_price: f64,
    pub initial_limit_price: f64,
    pub symbol: String,
    pub spot_value: f64,
    pub available_collateral: f64,
}

#[derive(Debug, Clone)]
pub struct HedgeProgressUpdate {
    pub current_spot_price: f64,
    pub new_limit_price: f64,
    pub is_replacement: bool,
    pub filled_qty: f64,
    pub target_qty: f64,
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

    pub async fn calculate_hedge_params(&self, req: &HedgeRequest) -> Result<HedgeParams> {
        // (Без изменений)
        let HedgeRequest {
            sum,
            symbol,
            volatility,
        } = req;
        debug!("Calculating hedge params for {}...", symbol);

        let spot_info = self
            .exchange
            .get_spot_instrument_info(symbol)
            .await
            .map_err(|e| anyhow!("Failed to get SPOT instrument info: {}", e))?;
        let linear_info = self
            .exchange
            .get_linear_instrument_info(symbol)
            .await
            .map_err(|e| anyhow!("Failed to get LINEAR instrument info: {}", e))?;

        let spot_fee = match self.exchange.get_fee_rate(symbol, SPOT_CATEGORY).await {
            Ok(fee) => {
                info!("Spot fee rate: Taker={}", fee.taker);
                fee.taker
            }
            Err(e) => {
                warn!(
                    "Could not get spot fee rate: {}. Using fallback 0.001",
                    e
                );
                0.001
            }
        };

        let mmr = self.exchange.get_mmr(symbol).await?;
        let initial_spot_value = sum / ((1.0 + volatility) * (1.0 + mmr));

        if initial_spot_value <= 0.0 {
            return Err(anyhow!("Initial spot value is non-positive"));
        }

        let current_spot_price = self.exchange.get_spot_price(symbol).await?;
        if current_spot_price <= 0.0 {
            return Err(anyhow!("Invalid spot price"));
        }

        let ideal_gross_qty = initial_spot_value / current_spot_price;
        debug!(
            "Ideal gross quantity (before fees/rounding): {}",
            ideal_gross_qty
        );

        let fut_qty_step_str = linear_info
            .lot_size_filter
            .qty_step
            .as_deref()
            .ok_or_else(|| anyhow!("Missing qtyStep"))?;
        let fut_decimals = fut_qty_step_str
            .split('.')
            .nth(1)
            .map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        debug!("Futures precision: {} decimals", fut_decimals);

        let target_net_qty_decimal = Decimal::from_f64(ideal_gross_qty)
            .ok_or_else(|| anyhow!("Failed to convert ideal qty"))?
            .trunc_with_scale(fut_decimals);
        let target_net_qty = target_net_qty_decimal
            .to_f64()
            .ok_or_else(|| anyhow!("Failed to convert target net qty"))?;
        debug!(
            "Target NET quantity (rounded to fut_decimals): {}",
            target_net_qty
        );

        if (1.0 - spot_fee).abs() < f64::EPSILON {
            return Err(anyhow!("Fee rate is 100% or invalid"));
        }
        let required_gross_qty = target_net_qty / (1.0 - spot_fee);
        debug!(
            "Required GROSS quantity (to get target net after fee): {}",
            required_gross_qty
        );

        let spot_precision_str = spot_info
            .lot_size_filter
            .base_precision
            .as_deref()
            .ok_or_else(|| anyhow!("Missing basePrecision"))?;
        let spot_decimals = spot_precision_str
            .split('.')
            .nth(1)
            .map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        debug!("Spot precision: {} decimals", spot_decimals);

        let final_spot_gross_qty_decimal = Decimal::from_f64(required_gross_qty)
            .ok_or_else(|| anyhow!("Failed to convert required gross qty"))?
            .trunc_with_scale(spot_decimals);
        let final_spot_gross_qty = final_spot_gross_qty_decimal
            .to_f64()
            .ok_or_else(|| anyhow!("Failed to convert final gross qty"))?;
        debug!(
            "Final SPOT GROSS quantity (rounded to spot_decimals): {}",
            final_spot_gross_qty
        );

        let spot_order_qty = final_spot_gross_qty;
        let fut_order_qty = target_net_qty;

        let adjusted_spot_value = spot_order_qty * current_spot_price;
        let available_collateral = sum - adjusted_spot_value;
        let futures_position_value = fut_order_qty * current_spot_price;

        debug!("Adjusted spot value (cost): {}", adjusted_spot_value);
        debug!(
            "Available collateral after spot buy: {}",
            available_collateral
        );
        debug!(
            "Futures position value (based on net qty): {}",
            futures_position_value
        );

        let initial_limit_price = current_spot_price * (1.0 - self.slippage);
        debug!("Initial limit price: {}", initial_limit_price);

        let min_spot_qty_str = &spot_info.lot_size_filter.min_order_qty;
        let min_spot_qty = Decimal::from_str(min_spot_qty_str).unwrap_or_default();
        if final_spot_gross_qty_decimal < min_spot_qty {
            return Err(anyhow!(
                "Calculated spot quantity {:.8} < min spot quantity {}",
                final_spot_gross_qty,
                min_spot_qty_str
            ));
        }
        let min_fut_qty_str = &linear_info.lot_size_filter.min_order_qty;
        let min_fut_qty = Decimal::from_str(min_fut_qty_str).unwrap_or_default();
        if target_net_qty_decimal < min_fut_qty {
            return Err(anyhow!(
                "Target net quantity {:.8} < min futures quantity {}",
                target_net_qty,
                min_fut_qty_str
            ));
        }
        if spot_order_qty <= 0.0 || fut_order_qty <= 0.0 {
            return Err(anyhow!("Final order quantities are non-positive"));
        }

        if available_collateral <= 0.0 {
            return Err(anyhow!("Available collateral non-positive"));
        }
        let required_leverage = futures_position_value / available_collateral;
        debug!("Calculated required leverage: {}", required_leverage);
        if required_leverage.is_nan() || required_leverage.is_infinite() {
            return Err(anyhow!("Invalid leverage calculation"));
        }
        if required_leverage > self.config.max_allowed_leverage {
            return Err(anyhow!(
                "Required leverage {:.2}x > max allowed {:.2}x",
                required_leverage,
                self.config.max_allowed_leverage
            ));
        }
        info!(
            "Required leverage {:.2}x is within max allowed {:.2}x",
            required_leverage, self.config.max_allowed_leverage
        );

        Ok(HedgeParams {
            spot_order_qty,
            fut_order_qty,
            current_spot_price,
            initial_limit_price,
            symbol: symbol.clone(),
            spot_value: adjusted_spot_value,
            available_collateral,
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
    ) -> Result<(f64, f64, f64)> {
        let HedgeParams {
            spot_order_qty: initial_spot_qty,
            fut_order_qty: initial_fut_qty,
            mut current_spot_price, // Эта цена будет обновляться
            initial_limit_price,
            symbol,
            spot_value: _spot_value, // Не используется напрямую ниже
            available_collateral: _available_collateral, // Не используется напрямую ниже
        } = params;
        let mut limit_price = initial_limit_price; // Цена для текущего спот ордера

        info!(
            "Running hedge op_id:{} for {} with spot_qty(gross_target)={}, fut_qty(net_target)={}",
            operation_id, symbol, initial_spot_qty, initial_fut_qty
        );

        // --- Проверка и установка плеча ---
        let futures_position_value = initial_fut_qty * current_spot_price;
        if _available_collateral <= 0.0 {
            error!("op_id:{}: Available collateral is non-positive. Aborting.", operation_id);
            let msg = "Available collateral is non-positive".to_string();
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
            return Err(anyhow!(msg));
        }
        let required_leverage = futures_position_value / _available_collateral;
        info!("op_id:{}: Confirming required leverage: {:.2}x", operation_id, required_leverage);
        if required_leverage.is_nan() || required_leverage.is_infinite() {
            error!("op_id:{}: Invalid required leverage calculation. Aborting.", operation_id);
            let msg = "Invalid required leverage calculation".to_string();
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
            return Err(anyhow!(msg));
        }
        let current_leverage = match self.exchange.get_current_leverage(&symbol).await {
             Ok(l) => l,
             Err(e) => {
                 error!("op_id:{}: Failed to get current leverage: {}. Aborting.", operation_id, e);
                 let msg = format!("Leverage check failed: {}", e);
                 let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
                 return Err(anyhow!(msg));
             }
        };
        let target_leverage_set = (required_leverage * 100.0).round() / 100.0;
        if (target_leverage_set - current_leverage).abs() > 0.01 {
            info!("op_id:{}: Setting leverage {} -> {}", operation_id, current_leverage, target_leverage_set);
            if let Err(e) = self.exchange.set_leverage(&symbol, target_leverage_set).await {
                error!("op_id:{}: Failed to set leverage to {:.2}x: {}. Aborting.", operation_id, target_leverage_set, e);
                let msg = format!("Failed to set leverage: {}", e);
                let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
                return Err(anyhow!(msg));
            }
            info!("op_id:{}: Leverage set/confirmed.", operation_id);
            sleep(Duration::from_millis(500)).await;
        } else {
            info!("op_id:{}: Leverage already set.", operation_id);
        }
        // ---

        // --- Цикл управления СПОТ-ордером ---
        let mut cumulative_filled_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty;
        let mut qty_filled_in_current_order = 0.0;
        let mut current_spot_order_id: Option<String> = None;

        let update_current_order_id_local = |id: Option<String>| {
            let storage_clone = Arc::clone(&current_order_id_storage);
            let filled_storage_clone = Arc::clone(&total_filled_qty_storage);
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

        // Размещение начального СПОТ-ордера
        info!("op_id:{}: Placing initial spot limit buy at {} for qty {}", operation_id, limit_price, current_order_target_qty);
        let spot_order = match self.exchange.place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price).await {
            Ok(o) => o,
            Err(e) => {
                 error!("op_id:{}: Failed place initial spot order: {}", operation_id, e);
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

        // --- Основной цикл управления СПОТ-ордером ---
        let hedge_loop_result: Result<()> = async {
            loop {
                sleep(Duration::from_millis(500)).await; // Проверяем чаще
                let now = Instant::now();
                let id_to_check_opt = current_spot_order_id.clone();
                let order_id_to_check = match id_to_check_opt {
                    Some(id) => id,
                    None => {
                         if cumulative_filled_qty < initial_spot_qty - ORDER_FILL_TOLERANCE {
                            if now.duration_since(start) > Duration::from_secs(1) {
                                warn!("op_id:{}: No active spot order ID, but target not reached. Aborting hedge.", operation_id);
                                return Err(anyhow!("No active spot order, but target not reached after fill/cancellation."));
                            } else {
                                debug!("op_id:{}: Spot order ID is None immediately after placement? Waiting.", operation_id);
                            }
                         } else {
                             info!("op_id:{}: No active spot order and target reached. Exiting loop.", operation_id);
                             break Ok(());
                         }
                         continue; // Ждем появления ID или выхода
                    }
                };

                // --- Используем ПРАВИЛЬНЫЙ метод для статуса СПОТА ---
                let status_result = self.exchange.get_spot_order_status(&symbol, &order_id_to_check).await;
                let status: OrderStatus = match status_result {
                    Ok(s) => s,
                    Err(e) => {
                        // Обработка "Order not found" - предполагаем исполнение, если ошибка возникла не сразу
                         if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                             warn!("op_id:{}: Spot Order {} not found after delay, assuming it filled for its target qty {}. Continuing...", operation_id, order_id_to_check, current_order_target_qty);
                             let assumed_filled_in_this_order = current_order_target_qty;
                             cumulative_filled_qty += assumed_filled_in_this_order;
                             cumulative_filled_qty = cumulative_filled_qty.min(initial_spot_qty);
                             *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                             current_spot_order_id = None;
                             _ = update_current_order_id_local(None).await; // Обновляем хранилище
                              if let Err(db_err) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                  error!("op_id:{}: Failed update DB after spot order not found: {}", operation_id, db_err);
                              }
                             if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                 info!("op_id:{}: Spot target reached after order not found assumption. Exiting loop.", operation_id);
                                 break Ok(());
                             } else {
                                 warn!("op_id:{}: Spot target not reached after assumption. Triggering replacement.", operation_id);
                                 start = now - self.max_wait - Duration::from_secs(1); // Форсируем замену
                                 qty_filled_in_current_order = 0.0;
                                 continue;
                             }
                         } else {
                             warn!("op_id:{}: Failed to get spot order status for {}: {}. Aborting hedge.", operation_id, order_id_to_check, e);
                             return Err(anyhow!("Hedge failed during spot status check: {}", e));
                         }
                    }
                };


                // Обновление cumulative_filled_qty
                 let previously_filled_in_current = qty_filled_in_current_order;
                 qty_filled_in_current_order = status.filled_qty;
                 let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current;
                 if filled_since_last_check.abs() > ORDER_FILL_TOLERANCE {
                     cumulative_filled_qty += filled_since_last_check;
                     cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_spot_qty * 1.00001);
                     *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                     if let Err(e) = update_hedge_spot_order(db, operation_id, current_spot_order_id.as_deref(), cumulative_filled_qty).await {
                         error!("op_id:{}: Failed to update spot filled qty in DB: {}", operation_id, e);
                     }
                 }

                // Проверка полного исполнения СПОТ ордера
                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                     info!("op_id:{}: Spot order {} considered filled. Exiting loop.", operation_id, order_id_to_check);
                     if (cumulative_filled_qty - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE {
                         warn!("op_id:{}: Spot final fill correction: {} -> {}", operation_id, cumulative_filled_qty, initial_spot_qty);
                         cumulative_filled_qty = initial_spot_qty;
                         *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                         let _ = update_hedge_spot_order(db, operation_id, current_spot_order_id.as_deref(), cumulative_filled_qty).await;
                     }
                     current_spot_order_id = None; _ = update_current_order_id_local(None).await;
                     break Ok(());
                }

                // --- Логика перестановки СПОТ-ордера ---
                let elapsed_since_start = now.duration_since(start);
                let mut is_replacement = false; // Флаг для колбэка
                 if elapsed_since_start > self.max_wait && status.remaining_qty > ORDER_FILL_TOLERANCE {
                    warn!("op_id:{}: Spot order {} timeout. Replacing...", operation_id, order_id_to_check);
                    // --- Используем ПРАВИЛЬНЫЙ метод для отмены СПОТА ---
                    if let Err(e) = self.exchange.cancel_spot_order(&symbol, &order_id_to_check).await { warn!("op_id:{}: Failed cancel spot order {}: {}", operation_id, order_id_to_check, e); sleep(Duration::from_millis(200)).await; } else { info!("op_id:{}: Sent cancel request for spot order {}", operation_id, order_id_to_check); sleep(Duration::from_millis(500)).await; }
                    current_spot_order_id = None; _ = update_current_order_id_local(None).await; qty_filled_in_current_order = 0.0;

                    // Перепроверка статуса после отмены
                    match self.exchange.get_spot_order_status(&symbol, &order_id_to_check).await {
                        Ok(final_status) => {
                             let filled_after_cancel = final_status.filled_qty - previously_filled_in_current;
                             if filled_after_cancel > ORDER_FILL_TOLERANCE {
                                 info!("op_id:{}: Spot Order {} filled further ({}) during/after cancel.", operation_id, order_id_to_check, filled_after_cancel);
                                 cumulative_filled_qty += filled_after_cancel;
                                 cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_spot_qty * 1.00001);
                                 *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                                 let _ = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await;
                             }
                             if final_status.remaining_qty <= ORDER_FILL_TOLERANCE || cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                  info!("op_id:{}: Spot Order {} filled completely after cancel or target reached.", operation_id, order_id_to_check);
                                   if (cumulative_filled_qty - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE {
                                        cumulative_filled_qty = initial_spot_qty; *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                                        let _ = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await;
                                   } break Ok(());
                              } else { info!("op_id:{}: Spot Order {} still has remaining {:.8} after cancel.", operation_id, order_id_to_check, final_status.remaining_qty); }
                        } Err(e) => warn!("op_id:{}: Failed get spot order status after cancel: {}", operation_id, e),
                    }
                    if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE { break Ok(()); } // Выход если цель достигнута

                    // Пересчет остатка и размещение нового ордера
                    let remaining_total_qty = initial_spot_qty - cumulative_filled_qty;
                    if remaining_total_qty <= ORDER_FILL_TOLERANCE { break Ok(()); } // Выход если остаток незначителен
                    current_spot_price = match self.exchange.get_spot_price(&symbol).await { Ok(p) if p > 0.0 => p, _ => return Err(anyhow!("Invalid price")) };
                    limit_price = current_spot_price * (1.0 - self.slippage);
                    current_order_target_qty = remaining_total_qty;
                    info!("op_id:{}: Placing new spot limit buy at {} for remaining qty {}", operation_id, limit_price, current_order_target_qty);
                    let new_spot_order = match self.exchange.place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price).await { Ok(o)=>o, Err(e)=>return Err(anyhow!("Failed place replacement: {}", e)) };
                    info!("op_id:{}: Placed replacement spot limit order: id={}", operation_id, new_spot_order.id);
                    current_spot_order_id = update_current_order_id_local(Some(new_spot_order.id.clone())).await;
                    start = now;
                    is_replacement = true; // Устанавливаем флаг для колбэка
                 }

                // --- Обновление статуса в Telegram (колбэк) ---
                let elapsed_since_update = now.duration_since(last_update_sent);
                 if is_replacement || elapsed_since_update > update_interval {
                     let price_for_cb = if !is_replacement { match self.exchange.get_spot_price(&symbol).await { Ok(p) => p, Err(_) => current_spot_price } } else { current_spot_price };
                     let update = HedgeProgressUpdate { current_spot_price: price_for_cb, new_limit_price: limit_price, is_replacement, filled_qty: qty_filled_in_current_order, target_qty: current_order_target_qty };
                     tokio::task::yield_now().await; // Даем шанс другим задачам
                     if let Err(e) = progress_callback(update).await {
                         if !e.to_string().contains("message is not modified") {
                             warn!("op_id:{}: Hedge progress callback failed: {}", operation_id, e);
                         }
                     }
                     last_update_sent = now;
                 }
                // --- КОНЕЦ ВЫЗОВА КОЛБЭКА ---

            } // --- Конец цикла loop для СПОТА ---
        }.await; // --- Конец async блока hedge_loop_result ---


        // --- Обработка результата цикла СПОТА ---
        if let Err(loop_err) = hedge_loop_result {
            error!("op_id:{}: Hedge spot buy loop failed: {}", operation_id, loop_err);
            let current_filled = *total_filled_qty_storage.lock().await;
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, current_filled, Some(&loop_err.to_string())).await;
            return Err(loop_err);
        }

        // --- Спот куплен, переходим к ФЬЮЧЕРСУ ---
        let final_spot_qty_gross = *total_filled_qty_storage.lock().await;
        info!("op_id:{}: Hedge spot buy loop finished. Final actual spot gross quantity: {:.8}", operation_id, final_spot_qty_gross);
        if (final_spot_qty_gross - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE {
            warn!("op_id:{}: Final GROSS filled {} differs from target {}. Using actual filled.", operation_id, final_spot_qty_gross, initial_spot_qty);
        }
        let final_spot_value_gross = final_spot_qty_gross * current_spot_price; // Используем последнюю известную current_spot_price


        // --- Размещение ФЬЮЧЕРСНОГО ЛИМИТНОГО ордера ---
        let futures_symbol = format!("{}{}", symbol, self.quote_currency);

        // --- Получаем актуальную цену фьючерса (Bid для Sell ордера) ---
        let futures_limit_price = match self.exchange.get_futures_ticker(&futures_symbol).await {
            Ok(ticker) => {
                let target_price = ticker.bid_price * (1.0 - self.slippage / 2.0); // Продаем чуть НИЖЕ лучшего бида
                info!("op_id:{}: Futures ticker received: bid={:.2}, ask={:.2}. Target limit sell price: {:.2}",
                       operation_id, ticker.bid_price, ticker.ask_price, target_price);
                target_price
            }
            Err(e) => {
                warn!("op_id:{}: Failed to get futures ticker: {}. Using last spot price with slippage as fallback.", operation_id, e);
                current_spot_price * (1.0 - self.slippage) // Fallback
            }
        };
        // --- Конец получения цены фьючерса ---

        info!(
            "op_id:{}: Placing futures limit sell order ({}) at price {:.2} for target NET quantity {}",
            operation_id, futures_symbol, futures_limit_price, initial_fut_qty
        );
        tokio::task::yield_now().await;

        // Размещаем фьючерсный лимитный ордер
        let fut_order = match self.exchange.place_futures_limit_order(
            &futures_symbol, OrderSide::Sell, initial_fut_qty, futures_limit_price
        ).await {
            Ok(order) => order,
            Err(e) => {
                warn!("op_id:{}: Failed to place initial futures limit order: {}. Spot was bought!", operation_id, e);
                let _ = update_hedge_final_status(db, operation_id, "Failed", None, final_spot_qty_gross, Some(&format!("Futures order placement failed: {}", e))).await;
                return Err(anyhow!("Failed to place futures order after spot fill: {}", e));
            }
        };
        info!("op_id:{}: Placed futures limit order: id={}", operation_id, fut_order.id);

        // --- НОВЫЙ ЦИКЛ: Ожидание исполнения ФЬЮЧЕРСНОГО ордера ---
        let futures_timeout_duration = Duration::from_secs(300); // Таймаут 5 минут для фьючерса
        let futures_check_interval = Duration::from_secs(2);
        let futures_start_time = Instant::now();
        let mut last_futures_filled_qty = 0.0; // Отслеживаем исполненное кол-во фьюча для записи в БД

        loop {
            if futures_start_time.elapsed() > futures_timeout_duration {
                error!("op_id:{}: Futures order {} did not fill within {:?}. Cancelling and failing hedge.",
                       operation_id, fut_order.id, futures_timeout_duration);
                // Пытаемся отменить фьючерсный ордер
                // --- Используем ПРАВИЛЬНЫЙ метод для отмены ФЬЮЧЕРСА ---
                if let Err(cancel_err) = self.exchange.cancel_futures_order(&futures_symbol, &fut_order.id).await {
                     warn!("op_id:{}: Failed to cancel timed out futures order {}: {}", operation_id, fut_order.id, cancel_err);
                }
                // --- Обновляем статус в БД на Failed ---
                let _ = update_hedge_final_status(db, operation_id, "Failed", Some(&fut_order.id), last_futures_filled_qty, // Записываем последнее известное исполненное кол-во
                    Some("Futures order fill timeout")).await;
                return Err(anyhow!("Futures order {} fill timeout", fut_order.id));
            }

            sleep(futures_check_interval).await;

            // --- Используем ПРАВИЛЬНЫЙ метод для статуса ФЬЮЧЕРСА ---
            match self.exchange.get_futures_order_status(&futures_symbol, &fut_order.id).await {
                Ok(status) => {
                    last_futures_filled_qty = status.filled_qty; // Обновляем последнее известное кол-во
                    info!("op_id:{}: Futures order {} status: filled={:.8}, remaining={:.8}",
                          operation_id, fut_order.id, status.filled_qty, status.remaining_qty);

                    if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                        info!("op_id:{}: Futures order {} considered filled.", operation_id, fut_order.id);
                        // Успех! Обновляем БД и выходим
                        let final_fut_qty = status.filled_qty; // Фактически исполненное кол-во фьюча
                        let _ = update_hedge_final_status(db, operation_id, "Completed", Some(&fut_order.id), final_fut_qty, None).await;
                        info!("op_id:{}: Hedge completed successfully. Spot Gross: {}, Fut Net: {}", operation_id, final_spot_qty_gross, final_fut_qty);
                        return Ok((final_spot_qty_gross, final_fut_qty, final_spot_value_gross)); // Возвращаем факт. кол-во фьюча
                    }
                    // Если не исполнен, продолжаем цикл
                }
                Err(e) => {
                    // Если ошибка "Order not found" - возможно, исполнился очень быстро
                    if e.to_string().contains("Order not found") {
                         warn!("op_id:{}: Futures order {} not found during fill check, assuming filled with target qty.", operation_id, fut_order.id);
                         let assumed_filled = initial_fut_qty; // Считаем исполненным целевое количество
                          let _ = update_hedge_final_status(db, operation_id, "Completed", Some(&fut_order.id), assumed_filled, None).await;
                          info!("op_id:{}: Hedge completed (futures order assumed filled). Spot Gross: {}, Fut Net: {}", operation_id, final_spot_qty_gross, assumed_filled);
                          return Ok((final_spot_qty_gross, assumed_filled, final_spot_value_gross));
                    } else {
                        // Другая ошибка при проверке статуса фьючерса
                        error!("op_id:{}: Failed to get futures order {} status: {}. Cancelling and failing hedge.",
                               operation_id, fut_order.id, e);
                        // --- Используем ПРАВИЛЬНЫЙ метод для отмены ФЬЮЧЕРСА ---
                        if let Err(cancel_err) = self.exchange.cancel_futures_order(&futures_symbol, &fut_order.id).await {
                             warn!("op_id:{}: Also failed to cancel futures order {}: {}", operation_id, fut_order.id, cancel_err);
                        }
                        // --- Обновляем статус в БД на Failed ---
                        let _ = update_hedge_final_status(db, operation_id, "Failed", Some(&fut_order.id), last_futures_filled_qty, // Записываем последнее известное исполненное кол-во
                            Some(&format!("Failed get futures order status: {}", e))).await;
                        return Err(anyhow!("Failed get futures order status for {}: {}", fut_order.id, e));
                    }
                }
            }
        }
                // --- КОНЕЦ ЦИКЛА ОЖИДАНИЯ ФЬЮЧЕРСА ---
    }
    /// Запуск расхеджирования (с ожиданием фьючерса ИГНОРИРОВАНИЕМ ПЫЛИ, ПРОГРЕССОМ)
    pub async fn run_unhedge(
        &self,
        original_op: HedgeOperation,
        db: &Db,
        mut progress_callback: HedgeProgressCallback, // <-- ДОБАВЛЕН КОЛБЭК
    ) -> Result<(f64, f64)>
    {
        let symbol = original_op.base_symbol.clone();
        let original_hedge_op_id = original_op.id;
        // Количество фьючерса для откупа берем из ИСХОДНОЙ операции хеджа
        let futures_buy_qty = original_op.target_futures_qty;
        // --- Цель продажи спота - фактически купленное в ходе хеджа ---
        let target_spot_sell_qty = original_op.spot_filled_qty;
        // ---

        info!(
            "Starting unhedge op_id={} ({}), target spot sell qty={:.8}, target futures qty to buy back={}",
            original_hedge_op_id, symbol, target_spot_sell_qty, futures_buy_qty
        );

        // Проверяем, что целевое количество для продажи > 0
        if target_spot_sell_qty <= ORDER_FILL_TOLERANCE {
            error!("op_id={}: Target spot sell quantity ({:.8}) based on original operation is too low. Cannot unhedge.", original_hedge_op_id, target_spot_sell_qty);
            return Err(anyhow!("Target spot sell quantity based on original operation is too low ({:.8})", target_spot_sell_qty));
        }

        let mut cumulative_filled_qty = 0.0;
        let mut qty_filled_in_current_order = 0.0;
        let mut current_spot_order_id: Option<String> = None;

        // --- Проверка баланса и определение реального кол-ва для продажи ---
        let available_balance = match self.exchange.get_balance(&symbol).await {
            Ok(balance) => {
                info!("op_id={}: Checked available balance {} for {}", original_hedge_op_id, balance.free, symbol);
                balance.free
            },
            Err(e) => {
                error!("op_id={}: Failed to get balance for {} before unhedge: {}. Aborting.", original_hedge_op_id, symbol, e);
                return Err(anyhow!("Failed to get balance before unhedge: {}", e));
            }
        };

        // Определяем, сколько реально можем продать: минимум из цели и доступного баланса
        let actual_spot_sell_qty = target_spot_sell_qty.min(available_balance);

        if actual_spot_sell_qty < target_spot_sell_qty - ORDER_FILL_TOLERANCE {
            warn!(
                "op_id={}: Available balance {:.8} is less than target sell quantity {:.8}. Selling available amount.",
                original_hedge_op_id, available_balance, target_spot_sell_qty
            );
        }

        // --- Получаем Min Order Qty и проверяем РЕАЛЬНОЕ количество для продажи ---
        let spot_info = self.exchange.get_spot_instrument_info(&symbol).await
            .map_err(|e| anyhow!("op_id={}: Failed to get SPOT instrument info for {}: {}", original_hedge_op_id, symbol, e))?;
        let min_spot_qty_decimal = Decimal::from_str(&spot_info.lot_size_filter.min_order_qty)
            .map_err(|e| anyhow!("op_id={}: Failed to parse min_order_qty '{}': {}", original_hedge_op_id, &spot_info.lot_size_filter.min_order_qty, e))?;
        info!("op_id={}: Minimum spot order quantity for {}: {}", original_hedge_op_id, symbol, min_spot_qty_decimal);

        let actual_spot_sell_qty_decimal = Decimal::from_f64(actual_spot_sell_qty).unwrap_or_else(|| Decimal::ZERO);

        if actual_spot_sell_qty <= ORDER_FILL_TOLERANCE || actual_spot_sell_qty_decimal < min_spot_qty_decimal {
            error!(
                "op_id={}: Actual quantity to sell ({:.8}) is below minimum order size ({}) or zero. Cannot unhedge.",
                original_hedge_op_id, actual_spot_sell_qty, min_spot_qty_decimal
            );
            return Err(anyhow!(
                "Actual quantity to sell ({:.8}) is below minimum order size ({})",
                actual_spot_sell_qty, min_spot_qty_decimal
            ));
        }
        // --- КОНЕЦ ПРОВЕРКИ РЕАЛЬНОГО КОЛ-ВА ---

        // Используем РЕАЛЬНОЕ количество как цель для ордеров
        let initial_order_target_qty = actual_spot_sell_qty;
        info!("op_id={}: Proceeding unhedge with actual spot sell quantity target: {:.8}", original_hedge_op_id, initial_order_target_qty);


        // --- Получение начальной цены ---
        let mut current_spot_price = match self.exchange.get_spot_price(&symbol).await {
            Ok(p) if p > 0.0 => p,
            Ok(p) => {
                error!("op_id={}: Invalid initial spot price received: {}", original_hedge_op_id, p);
                return Err(anyhow!("Invalid initial spot price: {}", p));
            }
            Err(e) => {
                error!("op_id={}: Failed to get initial spot price: {}. Aborting.", original_hedge_op_id, e);
                return Err(anyhow!("Failed to get initial spot price: {}", e));
            }
        };

        let mut limit_price = current_spot_price * (1.0 + self.slippage); // Цена ВЫШЕ для продажи
        info!("op_id={}: Initial spot price: {}, placing limit sell at {} for qty {:.8}", original_hedge_op_id, current_spot_price, limit_price, initial_order_target_qty);

        // --- Размещение ПЕРВОГО спот-ордера ---
        let spot_order = match self
            .exchange
            .place_limit_order(&symbol, OrderSide::Sell, initial_order_target_qty, limit_price)
            .await
        {
            Ok(o) => o,
            Err(e) => {
                if e.to_string().contains("170136") || e.to_string().contains("lower limit") {
                    error!("op_id={}: Initial unhedge quantity {} is already below minimum limit for {}. Error: {}", original_hedge_op_id, initial_order_target_qty, symbol, e);
                    return Err(anyhow!("Initial quantity {:.8} is below minimum order size for {}: {}", initial_order_target_qty, symbol, e));
                } else {
                    error!("op_id={}: Failed to place initial unhedge order: {}", original_hedge_op_id, e);
                    return Err(e);
                }
            }
        };

        info!("op_id={}: Placed initial spot limit order: id={}", original_hedge_op_id, spot_order.id);
        current_spot_order_id = Some(spot_order.id.clone());
        let mut current_order_target_qty = initial_order_target_qty; // Цель текущего ордера
        let mut start = Instant::now();
        // --- Переменные для колбэка прогресса ---
        let mut last_update_sent = Instant::now();
        let update_interval = Duration::from_secs(5);
        // ---

        // --- Основной цикл управления спот-ордером на ПРОДАЖУ ---
        let unhedge_loop_result: Result<()> = async {
            loop {
                sleep(Duration::from_millis(500)).await;
                let now = Instant::now();
                let id_to_check_opt = current_spot_order_id.clone();
                let order_id_to_check = match id_to_check_opt { Some(id)=>id, None=>{ if cumulative_filled_qty >= actual_spot_sell_qty - ORDER_FILL_TOLERANCE { break Ok(()) } else if now.duration_since(start) > Duration::from_secs(2) { return Err(anyhow!("No active spot order")); } continue } };

                // --- Используем get_spot_order_status ---
                let status_result = self.exchange.get_spot_order_status(&symbol, &order_id_to_check).await;
                let status: OrderStatus = match status_result {
                    Ok(s) => s,
                    Err(e) => {
                        if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                            warn!("op_id:{}: Unhedge Spot Order {} not found, assuming filled.", original_hedge_op_id, order_id_to_check);
                            // Аккуратно обновляем cumulative_filled_qty, не превышая цели
                            let filled_before = cumulative_filled_qty;
                            cumulative_filled_qty = (filled_before + current_order_target_qty).min(actual_spot_sell_qty);
                            current_spot_order_id = None; qty_filled_in_current_order = 0.0; // Сбрасываем счетчики
                            if cumulative_filled_qty >= actual_spot_sell_qty - ORDER_FILL_TOLERANCE { break Ok(()) } else { start = now - self.max_wait - Duration::from_secs(1); continue; } // Форсируем замену
                        } else {
                            return Err(anyhow!("Unhedge failed status check: {}", e));
                        }
                    }
                };

                // Обновление cumulative_filled_qty
                let previously_filled_in_current = qty_filled_in_current_order;
                qty_filled_in_current_order = status.filled_qty;
                let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current;
                if filled_since_last_check.abs() > ORDER_FILL_TOLERANCE {
                    cumulative_filled_qty += filled_since_last_check;
                    cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(actual_spot_sell_qty * 1.00001);
                    debug!("op_id={}: Unhedge spot fill update. Filled: {:.8}, Cum: {:.8}", original_hedge_op_id, qty_filled_in_current_order, cumulative_filled_qty);
                }

                // Проверка полного исполнения СПОТ ордера
                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                    info!("op_id:{}: Unhedge spot order {} considered filled. Exiting loop.", original_hedge_op_id, order_id_to_check);
                    if (cumulative_filled_qty - actual_spot_sell_qty).abs() > ORDER_FILL_TOLERANCE {
                        warn!("op_id={}: Unhedge spot final fill correction: {} -> {}", original_hedge_op_id, cumulative_filled_qty, actual_spot_sell_qty);
                        cumulative_filled_qty = actual_spot_sell_qty;
                    }
                    current_spot_order_id = None;
                    break Ok(());
                }

                // --- Логика перестановки СПОТ-ордера на ПРОДАЖУ ---
                let elapsed_since_start = now.duration_since(start);
                let mut is_replacement = false; // Флаг для колбэка
                if elapsed_since_start > self.max_wait && status.remaining_qty > ORDER_FILL_TOLERANCE {
                    warn!("op_id:{}: Unhedge spot order {} timeout. Replacing...", original_hedge_op_id, order_id_to_check);
                    let remaining_total_qty = actual_spot_sell_qty - cumulative_filled_qty;
                    let remaining_total_qty_decimal = Decimal::from_f64(remaining_total_qty).unwrap_or_default();

                    // Проверка на пыль
                    if remaining_total_qty > ORDER_FILL_TOLERANCE && remaining_total_qty_decimal < min_spot_qty_decimal {
                        warn!("op_id={}: Unhedge remaining qty {:.8} is dust. Ignoring.", original_hedge_op_id, remaining_total_qty);
                        // --- Используем cancel_spot_order ---
                        if let Err(e) = self.exchange.cancel_spot_order(&symbol, &order_id_to_check).await { warn!("op_id:{}: Failed cancel spot order {}: {}", original_hedge_op_id, order_id_to_check, e); } else { info!("op_id:{}: Cancel request sent for spot order {}", original_hedge_op_id, order_id_to_check); sleep(Duration::from_millis(500)).await; }
                        current_spot_order_id = None; qty_filled_in_current_order = 0.0;
                        match self.exchange.get_spot_order_status(&symbol, &order_id_to_check).await { Ok(fs)=>{ let filled_after = fs.filled_qty - previously_filled_in_current; if filled_after > ORDER_FILL_TOLERANCE { cumulative_filled_qty += filled_after; cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(actual_spot_sell_qty * 1.00001); } }, Err(e)=>{warn!("op_id:{}: Failed get final status after cancel: {}", original_hedge_op_id, e);}}
                        break Ok(());
                    }

                    // Отмена и перепроверка статуса
                    if let Err(e) = self.exchange.cancel_spot_order(&symbol, &order_id_to_check).await { warn!("op_id:{}: Failed cancel spot order {}: {}", original_hedge_op_id, order_id_to_check, e); sleep(Duration::from_millis(200)).await;} else { info!("op_id:{}: Sent cancel request for spot order {}", original_hedge_op_id, order_id_to_check); sleep(Duration::from_millis(500)).await; }
                    current_spot_order_id = None; qty_filled_in_current_order = 0.0;
                    match self.exchange.get_spot_order_status(&symbol, &order_id_to_check).await { Ok(fs) => { let filled_after = fs.filled_qty - previously_filled_in_current; if filled_after > ORDER_FILL_TOLERANCE { cumulative_filled_qty += filled_after; cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(actual_spot_sell_qty * 1.00001); } if fs.remaining_qty <= ORDER_FILL_TOLERANCE || cumulative_filled_qty >= actual_spot_sell_qty - ORDER_FILL_TOLERANCE { break Ok(()) } } Err(e) => { warn!("op_id:{}: Failed get spot status after cancel: {}", original_hedge_op_id, e); } }
                    if cumulative_filled_qty >= actual_spot_sell_qty - ORDER_FILL_TOLERANCE { break Ok(()) }

                    // Пересчет остатка и размещение нового ордера
                    let remaining_total_qty = actual_spot_sell_qty - cumulative_filled_qty;
                    if remaining_total_qty <= ORDER_FILL_TOLERANCE { break Ok(()) }
                    current_spot_price = match self.exchange.get_spot_price(&symbol).await { Ok(p) if p > 0.0 => p, _ => return Err(anyhow!("Invalid price")) };
                    limit_price = current_spot_price * (1.0 + self.slippage); // Цена ВЫШЕ для продажи
                    current_order_target_qty = remaining_total_qty;
                    info!("op_id={}: Placing new spot limit sell at {} for remaining qty {:.8}", original_hedge_op_id, limit_price, current_order_target_qty);
                    let new_spot_order = match self.exchange.place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price).await { Ok(o)=>o, Err(e)=>return Err(anyhow!("Failed place replacement: {}", e)) };
                    info!("op_id:{}: Placed replacement spot limit sell order: id={}", original_hedge_op_id, new_spot_order.id);
                    current_spot_order_id = Some(new_spot_order.id.clone());
                    start = now;
                    is_replacement = true; // Устанавливаем флаг для колбэка
                }

                // --- ВЫЗОВ КОЛБЭКА ПРОГРЕССА ---
                let elapsed_since_update = now.duration_since(last_update_sent);
                if is_replacement || elapsed_since_update > update_interval {
                    // Получаем актуальную цену спота для колбэка
                    let price_for_cb = if !is_replacement {
                        match self.exchange.get_spot_price(&symbol).await {
                            Ok(p) => p, Err(_) => current_spot_price // Используем старую, если не удалось получить новую
                        }
                    } else {
                        current_spot_price // Используем цену, по которой разместили замену
                    };
                    let update = HedgeProgressUpdate { // Используем ту же структуру
                        current_spot_price: price_for_cb,
                        new_limit_price: limit_price, // Текущая лимитная цена активного ордера
                        is_replacement,
                        filled_qty: qty_filled_in_current_order, // Сколько исполнено в ТЕКУЩЕМ ордере
                        target_qty: current_order_target_qty,   // Цель ТЕКУЩЕГО ордера
                    };
                    tokio::task::yield_now().await; // Даем шанс другим задачам
                    if let Err(e) = progress_callback(update).await {
                        if !e.to_string().contains("message is not modified") {
                            warn!("op_id:{}: Unhedge progress callback failed: {}", original_hedge_op_id, e);
                        }
                    }
                    last_update_sent = now;
                }
                // --- КОНЕЦ ВЫЗОВА КОЛБЭКА ---

            } // --- Конец цикла loop для СПОТА ---
        }.await; // --- Конец async блока unhedge_loop_result ---

        // --- Обработка результата цикла продажи СПОТА ---
        if let Err(loop_err) = unhedge_loop_result {
            error!("op_id={}: Unhedge spot sell loop failed: {}", original_hedge_op_id, loop_err);
            return Err(loop_err);
        }

        let final_spot_sold_qty = cumulative_filled_qty;
        info!("op_id={}: Unhedge spot sell loop finished. Final actual spot sold quantity: {:.8}", original_hedge_op_id, final_spot_sold_qty);
        if (final_spot_sold_qty - target_spot_sell_qty).abs() > ORDER_FILL_TOLERANCE {
            warn!("op_id={}: Final spot sold qty {:.8} differs from target {:.8}.", original_hedge_op_id, final_spot_sold_qty, target_spot_sell_qty);
        }

        // --- Спот продан, переходим к ФЬЮЧЕРСУ (с ожиданием исполнения) ---
        let futures_symbol = format!("{}{}", symbol, self.quote_currency);

        // --- Получаем актуальную цену фьючерса (Ask для Buy ордера) ---
        let futures_limit_price = match self.exchange.get_futures_ticker(&futures_symbol).await {
            Ok(ticker) => {
                let target_price = ticker.ask_price * (1.0 + self.slippage / 2.0); // Покупаем чуть выше лучшего аска
                info!("op_id:{}: Futures ticker received: bid={:.2}, ask={:.2}. Target limit buy price: {:.2}",
                    original_hedge_op_id, ticker.bid_price, ticker.ask_price, target_price);
                target_price
            }
            Err(e) => {
                warn!("op_id:{}: Failed to get futures ticker: {}. Using last spot price with slippage as fallback.", original_hedge_op_id, e);
                current_spot_price * (1.0 + self.slippage) // Fallback
            }
        };
        // --- Конец получения цены фьючерса ---

        info!(
            "op_id={}: Placing futures limit buy order ({}) at price {:.2} for original hedge net quantity {}",
            original_hedge_op_id, futures_symbol, futures_limit_price, futures_buy_qty
        );
        tokio::task::yield_now().await;

        // Размещаем фьючерсный ЛИМИТНЫЙ ордер на ПОКУПКУ
        let fut_order = match self.exchange.place_futures_limit_order(
            &futures_symbol, OrderSide::Buy, futures_buy_qty, futures_limit_price
        ).await {
            Ok(order) => order,
            Err(e) => {
                warn!("op_id:{}: Failed to place initial futures limit buy order: {}. Spot was sold!", original_hedge_op_id, e);
                return Err(anyhow!("Failed to place futures order after spot sell: {}", e));
            }
        };
        info!("op_id:{}: Placed futures limit buy order: id={}", original_hedge_op_id, fut_order.id);

        // --- ЦИКЛ ОЖИДАНИЯ ИСПОЛНЕНИЯ ФЬЮЧЕРСА НА ПОКУПКУ ---
        let futures_timeout_duration = Duration::from_secs(300);
        let futures_check_interval = Duration::from_secs(2);
        let futures_start_time = Instant::now();
        let mut last_futures_filled_qty = 0.0;

        loop {
            if futures_start_time.elapsed() > futures_timeout_duration {
                error!("op_id:{}: Futures buy order {} did not fill within {:?}. Aborting unhedge completion.", original_hedge_op_id, fut_order.id, futures_timeout_duration);
                if let Err(cancel_err) = self.exchange.cancel_futures_order(&futures_symbol, &fut_order.id).await { warn!("op_id:{}: Failed cancel timed out futures buy {}: {}", original_hedge_op_id, fut_order.id, cancel_err); }
                return Err(anyhow!("Futures buy order {} fill timeout", fut_order.id));
            }

            sleep(futures_check_interval).await;

            // --- Используем get_futures_order_status ---
            match self.exchange.get_futures_order_status(&futures_symbol, &fut_order.id).await {
                Ok(status) => {
                    last_futures_filled_qty = status.filled_qty;
                    info!("op_id:{}: Futures buy order {} status: filled={:.8}, remaining={:.8}", original_hedge_op_id, fut_order.id, status.filled_qty, status.remaining_qty);
                    if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                        info!("op_id:{}: Futures buy order {} considered filled.", original_hedge_op_id, fut_order.id);
                        if let Err(e) = mark_hedge_as_unhedged(db, original_hedge_op_id).await { error!("op_id:{}: Failed mark hedge {} unhedged: {}", original_hedge_op_id, original_hedge_op_id, e); }
                        info!("op_id:{}: Unhedge completed successfully for original op_id={}. Spot Sold: {:.8}, Fut Bought: {:.8}", original_hedge_op_id, original_hedge_op_id, final_spot_sold_qty, status.filled_qty);
                        return Ok((final_spot_sold_qty, status.filled_qty)); // Успех
                    }
                }
                Err(e) => {
                    if e.to_string().contains("Order not found") {
                        warn!("op_id:{}: Futures buy order {} not found, assuming filled.", original_hedge_op_id, fut_order.id);
                        let assumed_filled = futures_buy_qty;
                        if let Err(e_db) = mark_hedge_as_unhedged(db, original_hedge_op_id).await { error!("op_id:{}: Failed mark hedge {} unhedged: {}", original_hedge_op_id, original_hedge_op_id, e_db); }
                        info!("op_id:{}: Unhedge completed (futures assumed filled). Spot Sold: {:.8}, Fut Bought: {:.8}", original_hedge_op_id, final_spot_sold_qty, assumed_filled);
                        return Ok((final_spot_sold_qty, assumed_filled));
                    } else {
                        error!("op_id:{}: Failed get futures buy order {} status: {}. Aborting.", original_hedge_op_id, fut_order.id, e);
                        // --- Используем cancel_futures_order ---
                        if let Err(cancel_err) = self.exchange.cancel_futures_order(&futures_symbol, &fut_order.id).await { warn!("op_id:{}: Also failed cancel futures buy {}: {}", original_hedge_op_id, fut_order.id, cancel_err); }
                        return Err(anyhow!("Failed get futures buy order status for {}: {}", fut_order.id, e));
                    }
                }
            }
        }
        // --- КОНЕЦ ЦИКЛА ОЖИДАНИЯ ФЬЮЧЕРСА ---

    } // Конец run_unhedge
}