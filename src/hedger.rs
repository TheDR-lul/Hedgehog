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
    pub spot_order_qty: f64, // Округленное БРУТТО для спота
    pub fut_order_qty: f64,  // Округленное НЕТТО для фьюча (равно target_net_qty)
    pub current_spot_price: f64,
    pub initial_limit_price: f64,
    pub symbol: String,
    pub spot_value: f64, // Стоимость БРУТТО спот-ордера
    pub available_collateral: f64, // Остаток после покупки БРУТТО спота
}

#[derive(Debug, Clone)]
pub struct HedgeProgressUpdate {
    pub current_spot_price: f64,
    pub new_limit_price: f64,
    pub is_replacement: bool,
    pub filled_qty: f64, // Исполнено в ТЕКУЩЕМ ордере
    pub target_qty: f64, // Цель ТЕКУЩЕГО ордера
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

    /// Расчет параметров: БРУТТО спота для целевого НЕТТО фьюча
    pub async fn calculate_hedge_params(&self, req: &HedgeRequest) -> Result<HedgeParams> {
        let HedgeRequest { sum, symbol, volatility } = req;
        debug!("Calculating hedge params for {}...", symbol);

        // Шаг 1: Получаем инфо для обоих рынков
        let spot_info = self.exchange.get_spot_instrument_info(symbol).await
            .map_err(|e| anyhow!("Failed to get SPOT instrument info: {}", e))?;
        let linear_info = self.exchange.get_linear_instrument_info(symbol).await
             .map_err(|e| anyhow!("Failed to get LINEAR instrument info: {}", e))?;

        // Шаг 4: Получаем комиссию спота
        let spot_fee = match self.exchange.get_fee_rate(symbol, SPOT_CATEGORY).await {
            Ok(fee) => { info!("Spot fee rate: Taker={}", fee.taker); fee.taker }
            Err(e) => { warn!("Could not get spot fee rate: {}. Using fallback 0.001", e); 0.001 }
        };

        // Начальный расчет SpotValue
        let mmr = self.exchange.get_mmr(symbol).await?;
        let initial_spot_value = sum / ((1.0 + volatility) * (1.0 + mmr));

        if initial_spot_value <= 0.0 { return Err(anyhow!("Initial spot value is non-positive")); }

        let current_spot_price = self.exchange.get_spot_price(symbol).await?;
        if current_spot_price <= 0.0 { return Err(anyhow!("Invalid spot price")); }

        // Идеальное брутто-количество
        let ideal_gross_qty = initial_spot_value / current_spot_price;
        debug!("Ideal gross quantity (before fees/rounding): {}", ideal_gross_qty);

        // Шаг 2 и 3: Определяем точность и целевое ЧИСТОЕ количество
        let fut_qty_step_str = linear_info.lot_size_filter.qty_step.as_deref().ok_or_else(|| anyhow!("Missing qtyStep"))?;
        let fut_decimals = fut_qty_step_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        debug!("Futures precision: {} decimals", fut_decimals);

        let target_net_qty_decimal = Decimal::from_f64(ideal_gross_qty)
            .ok_or_else(|| anyhow!("Failed to convert ideal qty"))?
            .trunc_with_scale(fut_decimals); // Округляем идеальное брутто до точности фьюча = наша цель по нетто
        let target_net_qty = target_net_qty_decimal.to_f64().ok_or_else(|| anyhow!("Failed to convert target net qty"))?;
        debug!("Target NET quantity (rounded to fut_decimals): {}", target_net_qty);

        // Шаг 5: Рассчитываем требуемое БРУТТО количество спота
        if (1.0 - spot_fee).abs() < f64::EPSILON { return Err(anyhow!("Fee rate is 100% or invalid")); }
        let required_gross_qty = target_net_qty / (1.0 - spot_fee);
        debug!("Required GROSS quantity (to get target net after fee): {}", required_gross_qty);

        // Шаг 6: Округляем требуемое БРУТТО до точности СПОТА
        let spot_precision_str = spot_info.lot_size_filter.base_precision.as_deref().ok_or_else(|| anyhow!("Missing basePrecision"))?;
        let spot_decimals = spot_precision_str.split('.').nth(1).map_or(0, |s| s.trim_end_matches('0').len()) as u32;
        debug!("Spot precision: {} decimals", spot_decimals);

        let final_spot_gross_qty_decimal = Decimal::from_f64(required_gross_qty)
             .ok_or_else(|| anyhow!("Failed to convert required gross qty"))?
             .trunc_with_scale(spot_decimals); // Округляем требуемое брутто до точности спота
        let final_spot_gross_qty = final_spot_gross_qty_decimal.to_f64().ok_or_else(|| anyhow!("Failed to convert final gross qty"))?;
        debug!("Final SPOT GROSS quantity (rounded to spot_decimals): {}", final_spot_gross_qty);

        // Шаг 7: Финальные количества для ордеров
        let spot_order_qty = final_spot_gross_qty; // Брутто для спота
        let fut_order_qty = target_net_qty;      // Нетто для фьючерса

        // Шаг 8: Пересчет значений для проверок
        let adjusted_spot_value = spot_order_qty * current_spot_price; // Стоимость реального спот ордера
        let available_collateral = sum - adjusted_spot_value; // Остаток USDT после покупки брутто-количества
        let futures_position_value = fut_order_qty * current_spot_price; // Стоимость фьюч. позиции (нетто кол-во)

        debug!("Adjusted spot value (cost): {}", adjusted_spot_value);
        debug!("Available collateral after spot buy: {}", available_collateral);
        debug!("Futures position value (based on net qty): {}", futures_position_value);

        let initial_limit_price = current_spot_price * (1.0 - self.slippage);
        debug!("Initial limit price: {}", initial_limit_price);

        // Шаг 9: Проверки
        // Мин. кол-во спота
        let min_spot_qty_str = &spot_info.lot_size_filter.min_order_qty;
        let min_spot_qty = Decimal::from_str(min_spot_qty_str).unwrap_or_default();
        if final_spot_gross_qty_decimal < min_spot_qty {
            return Err(anyhow!("Calculated spot quantity {:.8} < min spot quantity {}", final_spot_gross_qty, min_spot_qty_str));
        }
         // Мин. кол-во фьюча
        let min_fut_qty_str = &linear_info.lot_size_filter.min_order_qty;
        let min_fut_qty = Decimal::from_str(min_fut_qty_str).unwrap_or_default();
         if target_net_qty_decimal < min_fut_qty {
            return Err(anyhow!("Target net quantity {:.8} < min futures quantity {}", target_net_qty, min_fut_qty_str));
        }
        // Положительные значения
        if spot_order_qty <= 0.0 || fut_order_qty <= 0.0 {
             return Err(anyhow!("Final order quantities are non-positive"));
        }

        // Плечо
        if available_collateral <= 0.0 { return Err(anyhow!("Available collateral non-positive")); }
        let required_leverage = futures_position_value / available_collateral; // Плечо для нетто-позиции
        debug!("Calculated required leverage: {}", required_leverage);
        if required_leverage.is_nan() || required_leverage.is_infinite() { return Err(anyhow!("Invalid leverage calculation")); }
        if required_leverage > self.config.max_allowed_leverage {
            return Err(anyhow!("Required leverage {:.2}x > max allowed {:.2}x", required_leverage, self.config.max_allowed_leverage));
        }
        info!("Required leverage {:.2}x is within max allowed {:.2}x", required_leverage, self.config.max_allowed_leverage);


        Ok(HedgeParams {
            spot_order_qty, // Финальное БРУТТО для спота
            fut_order_qty,  // Финальное НЕТТО для фьюча
            current_spot_price,
            initial_limit_price,
            symbol: symbol.clone(),
            spot_value: adjusted_spot_value, // Стоимость БРУТТО кол-ва спота
            available_collateral, // Остаток после покупки БРУТТО кол-ва
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
        // Возвращаем: (Фактическое БРУТТО спота, Целевое НЕТТО фьюча, Стоимость БРУТТО спота)
    ) -> Result<(f64, f64, f64)>
    {
        let HedgeParams {
            spot_order_qty: initial_spot_qty, // Целевое БРУТТО для спота
            fut_order_qty: initial_fut_qty,   // Целевое НЕТТО для фьюча
            current_spot_price: mut current_spot_price, // Используется для limit_price и обновлений
            initial_limit_price: mut limit_price,
            symbol,
            spot_value, // Стоимость целевого БРУТТО спота (для расчета плеча)
            available_collateral, // Остаток после покупки целевого БРУТТО спота (для расчета плеча)
        } = params;

        info!(
            "Running hedge op_id:{} for {} with spot_qty(gross_target)={}, fut_qty(net_target)={}, spot_value(gross_target)={}, avail_collateral={}",
            operation_id, symbol, initial_spot_qty, initial_fut_qty, spot_value, available_collateral
        );

        // --- Проверка и установка плеча ---
        let futures_position_value = initial_fut_qty * current_spot_price; // Приблизительная стоимость фьюча
        if available_collateral <= 0.0 {
            error!("op_id:{}: Available collateral is non-positive. Aborting.", operation_id);
            let msg = "Available collateral is non-positive".to_string();
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
            return Err(anyhow!(msg));
        }
        let required_leverage = futures_position_value / available_collateral; // Используем стоимость фьюча
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
                 return Err(anyhow!(msg));
             }
        };

        // --- Логика установки плеча ---
        let target_leverage_set = (required_leverage * 100.0).round() / 100.0; // Округляем до 2 знаков
        // Сравниваем с небольшим допуском
        if (target_leverage_set - current_leverage).abs() > 0.01 {
             info!("op_id:{}: Current leverage {:.2}x differs from target {:.2}x. Attempting to set leverage.", operation_id, current_leverage, target_leverage_set);
             if let Err(e) = self.exchange.set_leverage(&symbol, target_leverage_set).await {
                 error!("op_id:{}: Failed to set leverage to {:.2}x: {}. Aborting.", operation_id, target_leverage_set, e);
                 let msg = format!("Failed to set leverage: {}", e);
                 let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&msg)).await;
                 return Err(anyhow!(msg));
             }
             info!("op_id:{}: Leverage set/confirmed to ~{:.2}x successfully.", operation_id, target_leverage_set);
             sleep(Duration::from_millis(500)).await; // Небольшая пауза после установки
        } else {
             info!("op_id:{}: Current exchange leverage {:.2}x is already close enough to target {:.2}x. No need to set.", operation_id, current_leverage, target_leverage_set);
        }
        // --- Конец логики установки плеча ---


        // --- Размещение ордеров и цикл управления ---
        let mut cumulative_filled_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty; // Цель цикла - купить БРУТТО
        let mut qty_filled_in_current_order = 0.0;
        let mut current_spot_order_id: Option<String> = None;

        let update_current_order_id_local = |id: Option<String>| { /* ... Код без изменений ... */
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
        let spot_order = match self.exchange.place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price).await {
            Ok(o) => o,
            Err(e) => {
                error!("op_id:{}: Failed to place initial order: {}", operation_id, e);
                let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&e.to_string())).await;
                return Err(e);
            }
        };
        info!("op_id:{}: Placed initial spot limit order: id={}", operation_id, spot_order.id);
        current_spot_order_id = update_current_order_id_local(Some(spot_order.id.clone())).await;
        *total_filled_qty_storage.lock().await = 0.0; // Начинаем с нуля

        let mut start = Instant::now();
        let mut last_update_sent = Instant::now();
        let update_interval = Duration::from_secs(5);

        // --- Основной цикл управления спот-ордером ---
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
                                start = now - self.max_wait - Duration::from_secs(1); // Форсируем замену
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
                    cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_spot_qty * 1.01); // Ограничение сверху
                    *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                    if let Err(e) = update_hedge_spot_order(db, operation_id, current_spot_order_id.as_deref(), cumulative_filled_qty).await {
                        error!("op_id:{}: Failed to update spot filled qty in DB: {}", operation_id, e);
                    }
                }

                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                    info!("op_id:{}: Spot order {} considered filled by exchange (remaining_qty: {}, current order filled: {}, total cumulative: {}). Exiting loop.",
                           operation_id, order_id_to_check, status.remaining_qty, status.filled_qty, cumulative_filled_qty);
                    // Корректируем cumulative_filled_qty до initial_spot_qty, если исполнено
                    if (cumulative_filled_qty - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE {
                        warn!("op_id:{}: Cumulative filled qty {} differs slightly from target {}. Using target for final value.", operation_id, cumulative_filled_qty, initial_spot_qty);
                        cumulative_filled_qty = initial_spot_qty;
                         *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                         if let Err(e) = update_hedge_spot_order(db, operation_id, current_spot_order_id.as_deref(), cumulative_filled_qty).await {
                            error!("op_id:{}: Failed to update final spot filled qty in DB: {}", operation_id, e);
                        }
                    }
                    current_spot_order_id = update_current_order_id_local(None).await;
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

                    let remaining_total_qty = initial_spot_qty - cumulative_filled_qty; // Сколько еще нужно КУПИТЬ до ОБЩЕЙ цели

                    // Отменяем старый ордер
                    tokio::task::yield_now().await;
                    if let Err(e) = self.exchange.cancel_order(&symbol, &order_id_to_check).await {
                        warn!("op_id:{}: Attempt to cancel order {} failed or was ignored: {}", operation_id, order_id_to_check, e);
                        sleep(Duration::from_millis(500)).await;
                    } else {
                        info!("op_id:{}: Successfully sent cancel request for order {}", operation_id, order_id_to_check);
                        sleep(Duration::from_millis(500)).await;
                    }
                    current_spot_order_id = update_current_order_id_local(None).await; // Обновляем всё (ID = None)
                    qty_filled_in_current_order = 0.0; // Сброс для нового ордера

                    // Перепроверяем статус после отмены
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
                            // Корректируем до цели, если нужно
                            if (cumulative_filled_qty - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE && cumulative_filled_qty < initial_spot_qty {
                                warn!("op_id:{}: Correcting cumulative qty to target after fill during cancel.", operation_id);
                                cumulative_filled_qty = initial_spot_qty;
                                *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                                if let Err(e) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                    error!("op_id:{}: Failed to update corrected spot filled qty in DB: {}", operation_id, e);
                                }
                            }

                             if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                 info!("op_id:{}: Total filled quantity {} meets target {} after fill during cancel. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                                 break Ok(());
                             }
                         }
                         Ok(_) => { /* Order still active or partially filled */ }
                         Err(e) => { warn!("op_id:{}: Failed to get order status after cancel request: {}", operation_id, e); }
                     }


                    // Если после всех проверок цель все еще не достигнута
                    if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                         info!("op_id:{}: Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                         break Ok(());
                    }


                    // Получаем новую цену и ставим новый ордер
                    tokio::task::yield_now().await;
                    current_spot_price = match self.exchange.get_spot_price(&symbol).await {
                        Ok(p) if p > 0.0 => p, // Проверяем, что цена положительная
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
                    current_order_target_qty = remaining_total_qty; // Новый ордер на оставшийся объем

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
                    current_spot_order_id = update_current_order_id_local(Some(new_spot_order.id.clone())).await; // Обновляем всё
                    start = now; // Сбрасываем таймер ожидания для нового ордера
                }

                // --- Обновление статуса в Telegram ---
                if is_replacement || elapsed_since_update > update_interval {
                    if !is_replacement {
                         match self.exchange.get_spot_price(&symbol).await {
                            Ok(p) if p > 0.0 => price_for_update = p,
                            _ => {} // Игнорируем ошибки получения цены для апдейта
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
            // Обновляем статус в БД как Failed
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, cumulative_filled_qty, Some(&loop_err.to_string())).await;
            return Err(loop_err); // Возвращаем ошибку
        }

        // --- Размещение фьючерсного ордера ---
        // Убедимся, что final_spot_qty_gross точно равен initial_spot_qty (целевому брутто) перед возвратом
         let final_spot_qty_gross = if (cumulative_filled_qty - initial_spot_qty).abs() < ORDER_FILL_TOLERANCE {
             initial_spot_qty // Возвращаем целевое брутто
         } else {
             warn!("op_id:{}: Final GROSS filled {} differs significantly from target {}. Using actual filled.", operation_id, cumulative_filled_qty, initial_spot_qty);
             cumulative_filled_qty // Возвращаем фактическое брутто
         };
        // Пересчитываем финальную стоимость на основе фактического/целевого брутто
        let final_spot_value_gross = final_spot_qty_gross * current_spot_price;

        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "op_id:{}: Placing market sell order on futures ({}) for final NET quantity {}", // Используем целевое НЕТТО
            operation_id, futures_symbol, initial_fut_qty
        );
        tokio::task::yield_now().await;
        let fut_order_result = self.exchange.place_futures_market_order(&futures_symbol, OrderSide::Sell, initial_fut_qty).await; // Передаем НЕТТО кол-во

        match fut_order_result {
            Ok(fut_order) => {
                info!("op_id:{}: Placed futures market order: id={}", operation_id, fut_order.id);
                info!("op_id:{}: Hedge completed successfully. Total spot (gross) filled: {}", operation_id, final_spot_qty_gross);
                // В БД пишем целевое (оно же реальное) кол-во фьюча
                let _ = update_hedge_final_status(db, operation_id, "Completed", Some(&fut_order.id), initial_fut_qty, None).await;
                // Возвращаем: (Фактическое БРУТТО спота, Целевое/Фактическое НЕТТО фьюча, Стоимость БРУТТО спота)
                Ok((final_spot_qty_gross, initial_fut_qty, final_spot_value_gross))
            }
            Err(e) => {
                 warn!("op_id:{}: Failed to place futures order: {}. Spot was already bought!", operation_id, e);
                let _ = update_hedge_final_status(db, operation_id, "Failed", None, final_spot_qty_gross, Some(&format!("Futures order failed: {}", e))).await; // В БД пишем БРУТТО спота при ошибке
                Err(anyhow!("Failed to place futures order after spot fill: {}", e))
            }
        }
    }


    /// Запуск расхеджирования (с проверкой баланса и автоматической корректировкой количества)
    pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
        // TODO: Принимать и использовать ID операции хеджирования для связи и обновления БД
        // original_hedge_id: i64,
        // db: &Db,
    ) -> Result<(f64, f64)> {
         let UnhedgeRequest { quantity: requested_quantity, symbol } = req; // Используем requested_quantity
         // Корректируем initial_spot_qty и initial_fut_qty после проверки баланса
         let mut initial_spot_qty = requested_quantity;
         let mut initial_fut_qty = requested_quantity;

         info!(
            "Starting unhedge for {} with requested_quantity={}",
            symbol, requested_quantity
        );

        let mut cumulative_filled_qty = 0.0;
        // current_order_target_qty будет установлен после проверки баланса
        let mut current_order_target_qty: f64;
        let mut qty_filled_in_current_order = 0.0;
         let mut current_spot_order_id: Option<String> = None; // Для unhedge тоже нужен ID

        if requested_quantity <= 0.0 {
             error!("Requested quantity must be positive: {}. Aborting unhedge.", requested_quantity);
             return Err(anyhow::anyhow!("Requested quantity must be positive"));
        }

        // --- ИЗМЕНЕННАЯ ПРОВЕРКА БАЛАНСА ---
        match self.exchange.get_balance(&symbol).await {
            Ok(balance) => {
                 info!("Checked available balance {} for {}", balance.free, symbol);
                 if balance.free < requested_quantity - ORDER_FILL_TOLERANCE {
                     // Если доступно МЕНЬШЕ, чем запрошено (с учетом допуска)
                     if balance.free > ORDER_FILL_TOLERANCE {
                         // Но доступно больше нуля - используем доступное количество
                         warn!(
                             "Available balance {} is less than requested {}. Unhedging available amount.",
                             balance.free, requested_quantity
                         );
                         initial_spot_qty = balance.free; // Корректируем количество для операции
                         initial_fut_qty = balance.free;  // И фьючерс тоже на это количество
                     } else {
                         // Если доступно почти ноль - ошибка
                         error!("Insufficient available balance {} for {} to unhedge requested {}", balance.free, symbol, requested_quantity);
                         return Err(anyhow!("Insufficient balance: available {:.8}, needed {:.8}", balance.free, requested_quantity));
                     }
                 } else {
                      // Доступно достаточно или равно запрошенному - используем запрошенное
                      info!("Available balance {} is sufficient for requested {}", balance.free, requested_quantity);
                      // initial_spot_qty и initial_fut_qty остаются равными requested_quantity
                 }
            },
            Err(e) => {
                 // Если не удалось получить баланс, лучше не продолжать продажу
                 error!("Failed to get balance for {} before unhedge: {}. Aborting.", symbol, e);
                 return Err(anyhow!("Failed to get balance before unhedge: {}", e));
            }
        }
        // --- КОНЕЦ ИЗМЕНЕННОЙ ПРОВЕРКИ БАЛАНСА ---

        // Устанавливаем цель для первого ордера и для цикла
        current_order_target_qty = initial_spot_qty; // Это уже скорректированное количество
        info!("Proceeding unhedge with actual quantity: {}", initial_spot_qty);


        // --- Логика цикла для unhedge (полная версия) ---
        let mut current_spot_price = self.exchange.get_spot_price(&symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }

        let mut limit_price = current_spot_price * (1.0 + self.slippage); // Цена ВЫШЕ рынка
        info!("Initial spot price: {}, placing limit sell at {} for qty {}", current_spot_price, limit_price, current_order_target_qty);
        let spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);
        current_spot_order_id = Some(spot_order.id.clone()); // Сохраняем ID
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
                        continue; // Ждем
                    }
                };


                let status: OrderStatus = match self.exchange.get_order_status(&symbol, &order_id_to_check).await {
                    Ok(s) => s,
                    Err(e) => {
                        if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                            warn!("Unhedge order {} not found after delay, assuming it filled for its target qty {}. Continuing...", order_id_to_check, current_order_target_qty);
                            cumulative_filled_qty += current_order_target_qty;
                             current_spot_order_id = None; // Обнуляем ID
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
                    // TODO: Update unhedge_operations in DB
                }

                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                     info!("Unhedge spot order {} considered filled by exchange (remaining_qty: {}, current order filled: {}, total cumulative: {}). Exiting loop.",
                           order_id_to_check, status.remaining_qty, status.filled_qty, cumulative_filled_qty);
                    // Корректируем до цели (initial_spot_qty уже скорректирован, если нужно)
                    if (cumulative_filled_qty - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE {
                         warn!("Unhedge: Correcting final filled qty to target {}.", initial_spot_qty);
                         cumulative_filled_qty = initial_spot_qty;
                    }
                     current_spot_order_id = None; // Обнуляем ID
                     break Ok(()); // Выходим из цикла
                }

                if start.elapsed() > self.max_wait && status.remaining_qty > ORDER_FILL_TOLERANCE {
                     warn!("Unhedge spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...", order_id_to_check, qty_filled_in_current_order, current_order_target_qty, self.max_wait);

                    let remaining_total_qty = initial_spot_qty - cumulative_filled_qty; // Общая цель уже скорректирована

                    if let Err(e) = self.exchange.cancel_order(&symbol, &order_id_to_check).await {
                         warn!("Unhedge order {} cancellation failed (likely already inactive): {}", order_id_to_check, e);
                         sleep(Duration::from_millis(500)).await;
                    } else {
                        info!("Successfully sent cancel request for unhedge order {}", order_id_to_check);
                         sleep(Duration::from_millis(500)).await;
                    }
                    current_spot_order_id = None; // Обнуляем ID
                    qty_filled_in_current_order = 0.0;

                     match self.exchange.get_order_status(&symbol, &order_id_to_check).await {
                         Ok(final_status) if final_status.remaining_qty <= ORDER_FILL_TOLERANCE => {
                              info!("Unhedge order {} filled completely after cancel request. Adjusting cumulative qty.", order_id_to_check);
                              let filled_after_cancel = final_status.filled_qty - previously_filled_in_current;
                              if filled_after_cancel > 0.0 { cumulative_filled_qty += filled_after_cancel; }
                              // Корректируем до цели
                              if (cumulative_filled_qty - initial_spot_qty).abs() > ORDER_FILL_TOLERANCE && cumulative_filled_qty < initial_spot_qty {
                                  cumulative_filled_qty = initial_spot_qty;
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

                    limit_price = current_spot_price * (1.0 + self.slippage); // Цена ВЫШЕ рынка
                    current_order_target_qty = remaining_total_qty; // Цель нового ордера - оставшийся объем

                    info!("New spot price: {}, placing new limit sell at {} for remaining qty {}", current_spot_price, limit_price, current_order_target_qty);
                    let new_spot_order = self
                        .exchange
                        .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
                        .await?;
                    info!("Placed replacement spot limit order: id={}", new_spot_order.id);
                    current_spot_order_id = Some(new_spot_order.id.clone()); // Сохраняем новый ID
                    // TODO: Update unhedge_operations in DB (new order ID)
                    start = Instant::now(); // Сбрасываем таймер для нового ордера
                }
                // TODO: Add progress callback call for unhedge here if needed
            } // Конец цикла loop
        }.await; // Конец async блока

        if let Err(loop_err) = unhedge_loop_result {
             error!("Unhedge loop failed: {}", loop_err);
             // TODO: Update unhedge_operations status to Failed
             return Err(loop_err);
        }

         // Убедимся, что cumulative_filled_qty точно равен initial_spot_qty (скорректированному)
         let final_spot_qty = if (cumulative_filled_qty - initial_spot_qty).abs() < ORDER_FILL_TOLERANCE {
             initial_spot_qty
         } else {
             warn!("Unhedge: Final filled qty {} differs from target {}. Using actual filled.", cumulative_filled_qty, initial_spot_qty);
             cumulative_filled_qty
         };


        // Если цикл завершился успешно, покупаем фьючерс
        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "Placing market buy order on futures ({}) for final quantity {}", // Используем скорректированное initial_fut_qty
            futures_symbol, initial_fut_qty
        );
        let fut_order_result = self
            .exchange
            .place_futures_market_order(&futures_symbol, OrderSide::Buy, initial_fut_qty) // Используем скорректированный initial_fut_qty
            .await;

        match fut_order_result {
             Ok(fut_order) => {
                 info!("Placed futures market order: id={}", fut_order.id);
                 info!("Unhedge completed successfully for {}. Total spot filled: {}", symbol, final_spot_qty);
                 // TODO: Добавить обновление БД для unhedge
                 Ok((final_spot_qty, initial_fut_qty)) // Возвращаем скорректированные кол-ва
             }
             Err(e) => {
                 warn!("Failed to place futures order during unhedge: {}. Spot was already sold!", e);
                 // TODO: Добавить обновление БД для unhedge (статус Failed)
                 Err(anyhow!("Failed to place futures order after spot sell: {}", e))
             }
        }
    }

} // Конец impl Hedger