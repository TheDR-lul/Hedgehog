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
            mut current_spot_price,
            initial_limit_price,
            symbol,
            spot_value,
            available_collateral,
        } = params;

        let mut limit_price = initial_limit_price;

        info!(
            "Running hedge op_id:{} for {} with spot_qty(gross_target)={}, fut_qty(net_target)={}, spot_value(gross_target)={}, avail_collateral={}",
            operation_id, symbol, initial_spot_qty, initial_fut_qty, spot_value, available_collateral
        );

        // --- Проверка и установка плеча (без изменений) ---
        let futures_position_value = initial_fut_qty * current_spot_price;
        if available_collateral <= 0.0 {
            error!("op_id:{}: Available collateral is non-positive. Aborting.", operation_id);
            let msg = "Available collateral is non-positive".to_string();
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
            return Err(anyhow!(msg));
        }
        let required_leverage = futures_position_value / available_collateral;
        info!("op_id:{}: Confirming required leverage: {:.2}x", operation_id, required_leverage);

        if required_leverage.is_nan() || required_leverage.is_infinite() {
            error!("op_id:{}: Invalid required leverage calculation. Aborting.", operation_id);
            let msg = "Invalid required leverage calculation".to_string();
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
            return Err(anyhow!(msg));
        }

        let current_leverage = match self.exchange.get_current_leverage(&symbol).await {
             Ok(l) => { info!("op_id:{}: Current leverage on exchange for {}: {:.2}x", operation_id, symbol, l); l }
             Err(e) => {
                 error!("op_id:{}: Failed to get current leverage: {}. Aborting hedge.", operation_id, e);
                 let msg = format!("Leverage check failed: {}", e);
                 let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
                 return Err(anyhow!(msg));
             }
        };

        let target_leverage_set = (required_leverage * 100.0).round() / 100.0;
        if (target_leverage_set - current_leverage).abs() > 0.01 {
            info!("op_id:{}: Current leverage {:.2}x differs from target {:.2}x. Attempting to set leverage.", operation_id, current_leverage, target_leverage_set);
            if let Err(e) = self.exchange.set_leverage(&symbol, target_leverage_set).await {
                error!("op_id:{}: Failed to set leverage to {:.2}x: {}. Aborting.", operation_id, target_leverage_set, e);
                let msg = format!("Failed to set leverage: {}", e);
                let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
                return Err(anyhow!(msg));
            }
            info!("op_id:{}: Leverage set/confirmed to ~{:.2}x successfully.", operation_id, target_leverage_set);
            sleep(Duration::from_millis(500)).await;
        } else {
            info!("op_id:{}: Current exchange leverage {:.2}x is already close enough to target {:.2}x. No need to set.", operation_id, current_leverage, target_leverage_set);
        }
        // --- Конец логики установки плеча ---


        // --- Размещение ордеров и цикл управления ---
        let mut cumulative_filled_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty;
        let mut qty_filled_in_current_order = 0.0;
        // Инициализируем как None
        let mut current_spot_order_id: Option<String> = None;

        // Вспомогательная функция для обновления ID в хранилище и БД
        let update_current_order_id_local = |id: Option<String>| {
             let storage_clone = Arc::clone(&current_order_id_storage);
             let filled_storage_clone = Arc::clone(&total_filled_qty_storage);
             let db_clone = db.clone();
             async move {
                 let id_clone_for_storage = id.clone();
                 let id_clone_for_db = id.clone();
                 let filled_qty_for_db = *filled_storage_clone.lock().await;
                 // Обновляем общее хранилище
                 *storage_clone.lock().await = id_clone_for_storage;
                 // Обновляем БД
                 if let Err(e) = update_hedge_spot_order(&db_clone, operation_id, id_clone_for_db.as_deref(), filled_qty_for_db).await {
                     error!("op_id:{}: Failed to update spot order info in DB: {}", operation_id, e);
                 }
                 // Возвращаем ID для присваивания локальной переменной
                 id
             }
        };

        tokio::task::yield_now().await;
        info!("op_id:{}: Placing initial limit buy at {} for qty {}", operation_id, limit_price, current_order_target_qty);
        tokio::task::yield_now().await;
        let spot_order = match self.exchange.place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price).await {
            Ok(o) => o,
            Err(e) => {
                error!("op_id:{}: Failed to place initial order: {}", operation_id, e);
                let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&e.to_string())).await;
                return Err(e);
            }
        };
        info!("op_id:{}: Placed initial spot limit order: id={}", operation_id, spot_order.id);

        // --- ИСПРАВЛЕНИЕ: Присваиваем ID локальной переменной ---
        // Вызываем вспомогательную функцию и присваиваем результат локальной переменной
        current_spot_order_id = update_current_order_id_local(Some(spot_order.id.clone())).await;
        // --- КОНЕЦ ИСПРАВЛЕНИЯ ---

        *total_filled_qty_storage.lock().await = 0.0;

        let mut start = Instant::now();
        let mut last_update_sent = Instant::now();
        let update_interval = Duration::from_secs(5);

        // --- Основной цикл управления спот-ордером ---
        let hedge_loop_result: Result<()> = async {
            loop {
                sleep(Duration::from_secs(1)).await;
                let now = Instant::now();

                // Используем копию ID для проверок в этой итерации
                let id_to_check_opt = current_spot_order_id.clone();

                let order_id_to_check = match id_to_check_opt {
                    Some(id) => id,
                    None => {
                         // Этот блок теперь не должен срабатывать сразу после размещения ордера
                         if cumulative_filled_qty < initial_spot_qty - ORDER_FILL_TOLERANCE && now.duration_since(start) > Duration::from_secs(2) {
                             warn!("op_id:{}: No active order ID, but target not reached. Aborting hedge.", operation_id);
                             return Err(anyhow!("No active spot order, but target not reached after fill/cancellation."));
                         } else if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                              info!("op_id:{}: No active order and target reached. Exiting loop.", operation_id);
                              break Ok(());
                         }
                         debug!("op_id:{}: No active order ID, continuing wait.", operation_id);
                         continue;
                    }
                };


                tokio::task::yield_now().await;
                let status_result = self.exchange.get_order_status(&symbol, &order_id_to_check).await; // Передаем по ссылке

                let status: OrderStatus = match status_result {
                    Ok(s) => s,
                    Err(e) => {
                         if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                             warn!("op_id:{}: Order {} not found after delay, assuming it filled for its target qty {}. Continuing...", operation_id, order_id_to_check, current_order_target_qty);
                             let assumed_filled_in_this_order = current_order_target_qty;
                             cumulative_filled_qty += assumed_filled_in_this_order;
                             cumulative_filled_qty = cumulative_filled_qty.min(initial_spot_qty);

                             *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                             current_spot_order_id = None; // Безопасно
                             _ = update_current_order_id_local(None).await;

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
                    cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_spot_qty * 1.00001);
                    *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                    if let Err(e) = update_hedge_spot_order(db, operation_id, current_spot_order_id.as_deref(), cumulative_filled_qty).await {
                        error!("op_id:{}: Failed to update spot filled qty in DB: {}", operation_id, e);
                    }
                }

                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                    info!("op_id:{}: Spot order {} considered filled by exchange (remaining_qty: {}, current order filled: {}, total cumulative: {}). Exiting loop.",
                           operation_id, order_id_to_check, status.remaining_qty, status.filled_qty, cumulative_filled_qty);

                    if (cumulative_filled_qty - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE {
                         warn!("op_id:{}: Cumulative filled qty {} differs from target {}. Correcting to target.", operation_id, cumulative_filled_qty, initial_spot_qty);
                         cumulative_filled_qty = initial_spot_qty;
                         *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                          if let Err(e) = update_hedge_spot_order(db, operation_id, current_spot_order_id.as_deref(), cumulative_filled_qty).await {
                              error!("op_id:{}: Failed to update final spot filled qty in DB: {}", operation_id, e);
                          }
                    }
                    current_spot_order_id = None; // Безопасно
                    _ = update_current_order_id_local(None).await;
                    break Ok(());
                }


                let elapsed_since_start = now.duration_since(start);
                let elapsed_since_update = now.duration_since(last_update_sent);

                let mut is_replacement = false;
                let mut price_for_update = current_spot_price;

                // --- Логика перестановки ордера ---
                if elapsed_since_start > self.max_wait && status.remaining_qty > ORDER_FILL_TOLERANCE {
                    is_replacement = true;
                    warn!(
                        "op_id:{}: Spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                        operation_id, order_id_to_check, qty_filled_in_current_order, current_order_target_qty, self.max_wait
                    );

                    tokio::task::yield_now().await;
                    if let Err(e) = self.exchange.cancel_order(&symbol, &order_id_to_check).await {
                        warn!("op_id:{}: Attempt to cancel order {} failed or was ignored (may be inactive): {}", operation_id, order_id_to_check, e);
                        sleep(Duration::from_millis(200)).await;
                    } else {
                        info!("op_id:{}: Successfully sent cancel request for order {}", operation_id, order_id_to_check);
                        sleep(Duration::from_millis(500)).await;
                    }

                    current_spot_order_id = None; // Безопасно
                    _ = update_current_order_id_local(None).await;
                    qty_filled_in_current_order = 0.0;

                    tokio::task::yield_now().await;
                    match self.exchange.get_order_status(&symbol, &order_id_to_check).await {
                        Ok(final_status) => {
                             let filled_after_cancel = final_status.filled_qty - previously_filled_in_current;
                             if filled_after_cancel > ORDER_FILL_TOLERANCE {
                                 info!("op_id:{}: Order {} filled further ({}) during/after cancel request.", operation_id, order_id_to_check, filled_after_cancel);
                                 cumulative_filled_qty += filled_after_cancel;
                                 cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_spot_qty * 1.00001);
                                 *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                                 if let Err(e) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                     error!("op_id:{}: Failed to update spot filled qty in DB after fill during cancel: {}", operation_id, e);
                                 }
                             }

                             if final_status.remaining_qty <= ORDER_FILL_TOLERANCE || cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                  info!("op_id:{}: Order {} filled completely after cancel request or target reached. Adjusting cumulative qty.", operation_id, order_id_to_check);
                                  if (cumulative_filled_qty - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE {
                                       cumulative_filled_qty = initial_spot_qty;
                                       *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                                       if let Err(e) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                           error!("op_id:{}: Failed to update corrected spot filled qty in DB: {}", operation_id, e);
                                       }
                                  }
                                  info!("op_id:{}: Total filled quantity {} meets target {}. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                                  break Ok(());
                             } else {
                                info!("op_id:{}: Order {} still has remaining qty {:.8} after cancel check.", operation_id, order_id_to_check, final_status.remaining_qty);
                             }
                        }
                        Err(e) => {
                            warn!("op_id:{}: Failed to get order status after cancel request for {}: {}", operation_id, order_id_to_check, e);
                        }
                    }

                     if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                         info!("op_id:{}: Target quantity {} met after cancellation process. Exiting loop.", operation_id, initial_spot_qty);
                         break Ok(());
                     }

                    let remaining_total_qty = initial_spot_qty - cumulative_filled_qty;
                    if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                         info!("op_id:{}: Remaining total quantity is negligible after cancellation process. Exiting loop.", operation_id);
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
                            error!("op_id:{}: Failed to place replacement order: {}. Aborting hedge.", operation_id, e);
                            return Err(anyhow!("Hedge failed during replacement (place order step): {}", e));
                        }
                    };
                    info!("op_id:{}: Placed replacement spot limit order: id={}", operation_id, new_spot_order.id);
                    // Присваиваем ID и локальной переменной, и обновляем хранилище
                    current_spot_order_id = update_current_order_id_local(Some(new_spot_order.id.clone())).await;
                    start = now;
                }

                // --- Обновление статуса в Telegram (без изменений) ---
                if is_replacement || elapsed_since_update > update_interval {
                    if !is_replacement {
                        match self.exchange.get_spot_price(&symbol).await {
                            Ok(p) if p > 0.0 => price_for_update = p,
                            _ => {}
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
            } // --- Конец цикла loop ---
        }.await; // --- Конец async блока для hedge_loop_result ---


    // --- Обработка результата цикла и размещение фьючерсного ордера ---
    if let Err(loop_err) = hedge_loop_result {
        error!("op_id:{}: Hedge loop failed: {}", operation_id, loop_err);
        let current_filled = *total_filled_qty_storage.lock().await;
        let _ = update_hedge_final_status(db, operation_id, "Failed", None, current_filled, Some(&loop_err.to_string())).await;
        return Err(loop_err);
    }

    let final_spot_qty_gross = *total_filled_qty_storage.lock().await;
    info!("op_id:{}: Hedge spot buy loop finished. Final actual spot gross quantity: {}", operation_id, final_spot_qty_gross);
    if (final_spot_qty_gross - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE {
        warn!("op_id:{}: Final GROSS filled {} differs from target {}. Using actual filled.", operation_id, final_spot_qty_gross, initial_spot_qty);
    }

    let final_spot_value_gross = final_spot_qty_gross * current_spot_price; // Используем последнюю известную current_spot_price

    let futures_symbol = format!("{}{}", symbol, self.quote_currency);

    // --- ИЗМЕНЕНИЕ: Рассчитываем цену и вызываем place_futures_limit_order ---
    // Рассчитываем цену для лимитного ордера Sell - чуть НИЖЕ текущей спотовой цены
    let futures_limit_price = current_spot_price * (1.0 - self.slippage);
    info!(
        "op_id:{}: Placing limit sell order on futures ({}) at price {:.2} for target NET quantity {}",
        operation_id, futures_symbol, futures_limit_price, initial_fut_qty
    );
    tokio::task::yield_now().await;
    let fut_order_result = self.exchange.place_futures_limit_order( // Вызываем новую функцию
        &futures_symbol,
        OrderSide::Sell,
        initial_fut_qty,
        futures_limit_price // Передаем рассчитанную цену
    ).await;
    // --- КОНЕЦ ИЗМЕНЕНИЯ ---

    match fut_order_result {
        Ok(fut_order) => {
            info!("op_id:{}: Placed futures limit order: id={}", operation_id, fut_order.id); // Обновлен лог
            info!("op_id:{}: Hedge completed successfully. Total spot (gross) filled: {}", operation_id, final_spot_qty_gross);
            let _ = update_hedge_final_status(db, operation_id, "Completed", Some(&fut_order.id), initial_fut_qty, None).await;
            Ok((final_spot_qty_gross, initial_fut_qty, final_spot_value_gross))
        }
        Err(e) => {
            warn!("op_id:{}: Failed to place futures limit order: {}. Spot was already bought!", operation_id, e); // Обновлен лог
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, final_spot_qty_gross, Some(&format!("Futures order failed: {}", e))).await;
            Err(anyhow!("Failed to place futures limit order after spot fill: {}", e)) // Обновлен текст ошибки
        }
    }
    } // Конец run_hedge

    /// Запуск расхеджирования (с проверкой баланса и ИГНОРИРОВАНИЕМ ПЫЛИ)
    pub async fn run_unhedge(
        &self,
        original_op: HedgeOperation,
        db: &Db,
    ) -> Result<(f64, f64)> {
        // (Без изменений по сравнению с предыдущей версией)
        let symbol = original_op.base_symbol.clone();
        let original_hedge_op_id = original_op.id;
        let futures_buy_qty = original_op.target_futures_qty;

        info!(
            "Starting unhedge for original hedge op_id={} ({}), target futures qty to buy back={}",
            original_hedge_op_id, symbol, futures_buy_qty
        );

        let mut cumulative_filled_qty = 0.0;
        let mut qty_filled_in_current_order = 0.0;
        let mut current_spot_order_id: Option<String> = None;

        let spot_sell_qty = match self.exchange.get_balance(&symbol).await {
             Ok(balance) => {
                 info!("op_id={}: Checked available balance {} for {}", original_hedge_op_id, balance.free, symbol);
                 if balance.free > ORDER_FILL_TOLERANCE {
                     balance.free
                 } else {
                     error!("op_id={}: Insufficient available balance {} for {} to unhedge", original_hedge_op_id, balance.free, symbol);
                     return Err(anyhow!("Insufficient available balance: {:.8}", balance.free));
                 }
             },
             Err(e) => {
                 error!("op_id={}: Failed to get balance for {} before unhedge: {}. Aborting.", original_hedge_op_id, symbol, e);
                 return Err(anyhow!("Failed to get balance before unhedge: {}", e));
             }
        };

        let spot_info = self.exchange.get_spot_instrument_info(&symbol).await
            .map_err(|e| anyhow!("op_id={}: Failed to get SPOT instrument info for {}: {}", original_hedge_op_id, symbol, e))?;
        let min_spot_qty_decimal = Decimal::from_str(&spot_info.lot_size_filter.min_order_qty)
            .map_err(|e| anyhow!("op_id={}: Failed to parse min_order_qty '{}': {}", original_hedge_op_id, &spot_info.lot_size_filter.min_order_qty, e))?;
        info!("op_id={}: Minimum spot order quantity for {}: {}", original_hedge_op_id, symbol, min_spot_qty_decimal);

        let initial_order_target_qty = spot_sell_qty;
        info!("op_id={}: Proceeding unhedge with actual spot sell quantity target: {}", original_hedge_op_id, initial_order_target_qty);

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

        let mut limit_price = current_spot_price * (1.0 + self.slippage);
        info!("op_id={}: Initial spot price: {}, placing limit sell at {} for qty {}", original_hedge_op_id, current_spot_price, limit_price, initial_order_target_qty);

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
        let mut current_order_target_qty = initial_order_target_qty;
        let mut start = Instant::now();

        let unhedge_loop_result: Result<()> = async {
            loop {
                sleep(Duration::from_secs(1)).await;
                let now = Instant::now();

                let id_to_check_opt = current_spot_order_id.clone();
                let order_id_to_check = match id_to_check_opt {
                    Some(id) => id,
                    None => {
                        if cumulative_filled_qty >= spot_sell_qty - ORDER_FILL_TOLERANCE {
                            info!("op_id={}: No active unhedge order and target sell qty reached. Exiting loop.", original_hedge_op_id);
                            break Ok(());
                        } else if now.duration_since(start) > Duration::from_secs(2) {
                            warn!("op_id={}: No active unhedge order ID, but target {} not reached (filled {}). Aborting unhedge.", original_hedge_op_id, spot_sell_qty, cumulative_filled_qty);
                            return Err(anyhow!("No active spot order, but target sell qty not reached after fill/cancellation."));
                        }
                        debug!("op_id={}: No active order ID, continuing wait.", original_hedge_op_id);
                        continue;
                    }
                };

                 let status_result = self.exchange.get_order_status(&symbol, &order_id_to_check).await;

                 let status: OrderStatus = match status_result {
                    Ok(s) => s,
                    Err(e) => {
                        if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                            warn!("op_id={}: Unhedge order {} not found after delay, assuming it filled/cancelled for its target qty {}. Continuing...", original_hedge_op_id, order_id_to_check, current_order_target_qty);
                            let assumed_filled_in_this_order = current_order_target_qty;
                            cumulative_filled_qty += assumed_filled_in_this_order;
                            cumulative_filled_qty = cumulative_filled_qty.min(spot_sell_qty);
                            current_spot_order_id = None;
                            qty_filled_in_current_order = 0.0;
                            if cumulative_filled_qty >= spot_sell_qty - ORDER_FILL_TOLERANCE {
                                info!("op_id={}: Total filled spot quantity {} meets target {} after unhedge order not found assumption. Exiting loop.", original_hedge_op_id, cumulative_filled_qty, spot_sell_qty);
                                break Ok(());
                            } else {
                                warn!("op_id={}: Unhedge order filled assumption, but overall target {} not reached (filled {}). Triggering replacement logic.", original_hedge_op_id, spot_sell_qty, cumulative_filled_qty);
                                start = now - self.max_wait - Duration::from_secs(1);
                                continue;
                            }
                        } else {
                            warn!("op_id={}: Failed to get unhedge order status for {}: {}. Aborting unhedge.", original_hedge_op_id, order_id_to_check, e);
                            return Err(anyhow!("Unhedge failed during status check: {}", e));
                        }
                    }
                 };

                let previously_filled_in_current = qty_filled_in_current_order;
                qty_filled_in_current_order = status.filled_qty;
                let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current;

                if filled_since_last_check.abs() > ORDER_FILL_TOLERANCE {
                    cumulative_filled_qty += filled_since_last_check;
                    cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(spot_sell_qty * 1.00001);
                    debug!("op_id={}: Unhedge fill update. Filled in current: {:.8}, Cumulative: {:.8}", original_hedge_op_id, qty_filled_in_current_order, cumulative_filled_qty);
                }

                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                     info!("op_id={}: Unhedge spot order {} considered filled by exchange (remaining_qty: {}, current order filled: {}, total cumulative: {}). Exiting loop.",
                            original_hedge_op_id, order_id_to_check, status.remaining_qty, status.filled_qty, cumulative_filled_qty);
                     if cumulative_filled_qty > spot_sell_qty + ORDER_FILL_TOLERANCE {
                         warn!("op_id={}: Unhedge: Filled qty {:.8} slightly exceeds target {}. Using target.", original_hedge_op_id, cumulative_filled_qty, spot_sell_qty);
                         cumulative_filled_qty = spot_sell_qty;
                     }
                     current_spot_order_id = None;
                     break Ok(());
                }

                if start.elapsed() > self.max_wait && status.remaining_qty > ORDER_FILL_TOLERANCE {
                    warn!("op_id={}: Unhedge spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                          original_hedge_op_id, order_id_to_check, qty_filled_in_current_order, current_order_target_qty, self.max_wait);

                    let remaining_total_qty = spot_sell_qty - cumulative_filled_qty;
                    let remaining_total_qty_decimal = Decimal::from_f64(remaining_total_qty)
                        .unwrap_or_else(|| Decimal::ZERO);

                    if remaining_total_qty > ORDER_FILL_TOLERANCE && remaining_total_qty_decimal < min_spot_qty_decimal {
                        warn!(
                            "op_id={}: Remaining qty {:.8} is less than min order qty {}. IGNORING DUST and finishing spot sell.",
                            original_hedge_op_id, remaining_total_qty, min_spot_qty_decimal
                        );
                         info!("op_id={}: Cancelling order {} before ignoring dust.", original_hedge_op_id, order_id_to_check);
                         if let Err(e) = self.exchange.cancel_order(&symbol, &order_id_to_check).await {
                             warn!("op_id={}: Cancellation failed for order {} (likely inactive): {}", original_hedge_op_id, order_id_to_check, e);
                         } else {
                             info!("op_id={}: Cancel request sent for order {} before ignoring dust.", original_hedge_op_id, order_id_to_check);
                             sleep(Duration::from_millis(500)).await;
                         }
                         current_spot_order_id = None;
                         qty_filled_in_current_order = 0.0;

                         match self.exchange.get_order_status(&symbol, &order_id_to_check).await {
                            Ok(final_status) => {
                                let filled_after_cancel = final_status.filled_qty - previously_filled_in_current;
                                 if filled_after_cancel > ORDER_FILL_TOLERANCE {
                                    info!("op_id={}: Order {} filled further ({}) during/after final cancel.", original_hedge_op_id, order_id_to_check, filled_after_cancel);
                                    cumulative_filled_qty += filled_after_cancel;
                                    cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(spot_sell_qty * 1.00001);
                                 }
                            }
                            Err(e) => warn!("op_id:{}: Failed to get final status for {} after cancel: {}", original_hedge_op_id, order_id_to_check, e),
                         }

                         break Ok(());
                    }

                     info!("op_id={}: Cancelling order {} to replace it.", original_hedge_op_id, order_id_to_check);
                     if let Err(e) = self.exchange.cancel_order(&symbol, &order_id_to_check).await {
                         warn!("op_id={}: Cancellation failed for order {} (likely inactive): {}", original_hedge_op_id, order_id_to_check, e);
                         sleep(Duration::from_millis(200)).await;
                     } else {
                         info!("op_id={}: Successfully sent cancel request for unhedge order {}", original_hedge_op_id, order_id_to_check);
                         sleep(Duration::from_millis(500)).await;
                     }
                     current_spot_order_id = None;
                     qty_filled_in_current_order = 0.0;

                     match self.exchange.get_order_status(&symbol, &order_id_to_check).await {
                         Ok(final_status) => {
                             let filled_after_cancel = final_status.filled_qty - previously_filled_in_current;
                              if filled_after_cancel > ORDER_FILL_TOLERANCE {
                                 info!("op_id={}: Order {} filled further ({}) during/after cancel request.", original_hedge_op_id, order_id_to_check, filled_after_cancel);
                                 cumulative_filled_qty += filled_after_cancel;
                                 cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(spot_sell_qty * 1.00001);
                              }
                              if final_status.remaining_qty <= ORDER_FILL_TOLERANCE || cumulative_filled_qty >= spot_sell_qty - ORDER_FILL_TOLERANCE {
                                   info!("op_id={}: Target reached after final cancel check for order {}. Exiting loop.", original_hedge_op_id, order_id_to_check);
                                   break Ok(());
                              } else {
                                   info!("op_id={}: Order {} still has remaining qty {:.8} after cancel check.", original_hedge_op_id, order_id_to_check, final_status.remaining_qty);
                              }
                         }
                         Err(e) => { warn!("op_id:{}: Failed to get unhedge order status after cancel request: {}", original_hedge_op_id, e); }
                     }

                    let remaining_total_qty = spot_sell_qty - cumulative_filled_qty;
                    if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                        info!("op_id={}: Total filled quantity {} meets target {} during replacement process. Exiting loop.", original_hedge_op_id, cumulative_filled_qty, spot_sell_qty);
                        break Ok(());
                    }

                    current_spot_price = match self.exchange.get_spot_price(&symbol).await {
                         Ok(p) if p > 0.0 => p,
                         Ok(p) => {
                             error!("op_id={}: Received non-positive spot price during unhedge replacement: {}", original_hedge_op_id, p);
                             return Err(anyhow!("Received non-positive spot price during unhedge replacement: {}", p));
                         }
                         Err(e) => {
                             warn!("op_id={}: Failed to get spot price during unhedge replacement: {}. Aborting unhedge.", original_hedge_op_id, e);
                             return Err(anyhow!("Unhedge failed during replacement (get price step): {}", e));
                         }
                    };

                    limit_price = current_spot_price * (1.0 + self.slippage);
                    current_order_target_qty = remaining_total_qty;

                    info!("op_id={}: New spot price: {}, placing new limit sell at {} for remaining qty {}", original_hedge_op_id, current_spot_price, limit_price, current_order_target_qty);
                    let new_spot_order = match self
                        .exchange
                        .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
                        .await
                    {
                         Ok(o) => o,
                         Err(e) => {
                            error!("op_id={}: Failed to place replacement unhedge order (qty={:.8}): {}. Aborting.", original_hedge_op_id, current_order_target_qty, e);
                            return Err(anyhow!("Failed to place replacement order: {}", e));
                         }
                    };

                    info!("op_id={}: Placed replacement spot limit order: id={}", original_hedge_op_id, new_spot_order.id);
                    current_spot_order_id = Some(new_spot_order.id.clone());
                    start = Instant::now();
                }
            }
        }.await;


        if let Err(loop_err) = unhedge_loop_result {
            error!("op_id={}: Unhedge spot sell loop failed: {}", original_hedge_op_id, loop_err);
            return Err(loop_err);
        }

        let final_spot_sold_qty = cumulative_filled_qty;
        info!("op_id={}: Unhedge spot sell loop finished. Final actual spot sold quantity: {:.8}", original_hedge_op_id, final_spot_sold_qty);
        if (final_spot_sold_qty - spot_sell_qty).abs() > ORDER_FILL_TOLERANCE && final_spot_sold_qty < spot_sell_qty {
            warn!("op_id={}: Final spot sold qty {:.8} is less than initial available qty {:.8} (likely due to ignored dust).",
                   original_hedge_op_id, final_spot_sold_qty, spot_sell_qty);
        }


        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "op_id={}: Placing market buy order on futures ({}) for original hedge net quantity {}",
            original_hedge_op_id, futures_symbol, futures_buy_qty
        );
        let fut_order_result = self
            .exchange
            .place_futures_market_order(&futures_symbol, OrderSide::Buy, futures_buy_qty)
            .await;

        match fut_order_result {
            Ok(fut_order) => {
                info!("op_id={}: Placed futures market order: id={}", original_hedge_op_id, fut_order.id);
                info!("op_id={}: Unhedge completed successfully for original hedge op_id={}. Total spot sold: {:.8}, Futures bought back: {:.8}",
                       original_hedge_op_id, original_hedge_op_id, final_spot_sold_qty, futures_buy_qty);

                if let Err(e) = mark_hedge_as_unhedged(db, original_hedge_op_id).await {
                    error!("op_id={}: Failed to mark hedge operation {} as unhedged in DB: {}", original_hedge_op_id, original_hedge_op_id, e);
                }

                Ok((final_spot_sold_qty, futures_buy_qty))
            }
            Err(e) => {
                warn!("op_id={}: Failed to place futures buy order during unhedge: {}. Spot was already sold (qty={:.8})!",
                       original_hedge_op_id, e, final_spot_sold_qty);
                Err(anyhow!("Failed to place futures order after spot sell: {}", e))
            }
        }
    }
}