// src/hedger.rs

use anyhow::{anyhow, Result};
use tracing::{debug, error, info, warn};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use futures::future::BoxFuture;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

use crate::exchange::{Exchange, OrderStatus}; // Убрали FeeRate
use crate::exchange::bybit::{SPOT_CATEGORY, LINEAR_CATEGORY};
use crate::exchange::types::OrderSide;
use crate::models::{HedgeRequest, UnhedgeRequest};
use crate::storage::{Db, update_hedge_spot_order, update_hedge_final_status};


pub const ORDER_FILL_TOLERANCE: f64 = 1e-8;

#[derive(Clone)]
pub struct Hedger<E> {
    exchange:     E,
    slippage:     f64,
    max_wait:     Duration,
    quote_currency: String,
}

#[derive(Debug)]
pub struct HedgeParams {
    pub spot_order_qty: f64,
    pub fut_order_qty: f64,
    pub current_spot_price: f64,
    pub initial_limit_price: f64,
    pub symbol: String,
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
    pub fn new(exchange: E, slippage: f64, max_wait_secs: u64, quote_currency: String) -> Self {
        Self {
            exchange,
            slippage,
            max_wait: Duration::from_secs(max_wait_secs),
            quote_currency,
        }
    }

    pub async fn calculate_hedge_params(&self, req: &HedgeRequest) -> Result<HedgeParams> {
        let HedgeRequest { sum, symbol, volatility } = req;
        debug!(
            "Calculating hedge params for {} with sum={}, volatility={}",
            symbol, sum, volatility
        );

        match self.exchange.get_fee_rate(symbol, SPOT_CATEGORY).await {
            Ok(fee) => info!(symbol, category=SPOT_CATEGORY, maker_fee=fee.maker, taker_fee=fee.taker, "Current spot fee rate"),
            Err(e) => warn!("Could not get spot fee rate for {}: {}", symbol, e),
        }
        match self.exchange.get_fee_rate(symbol, LINEAR_CATEGORY).await {
             Ok(fee) => info!(symbol, category=LINEAR_CATEGORY, maker_fee=fee.maker, taker_fee=fee.taker, "Current futures fee rate"),
             Err(e) => warn!("Could not get futures fee rate for {}: {}", symbol, e),
        }

        let mmr = self.exchange.get_mmr(symbol).await?;
        let spot_value = sum / ((1.0 + volatility) * (1.0 + mmr));
        let fut_value  = sum - spot_value;
        debug!(
            "Calculated values: spot_value={}, fut_value={}, MMR={}",
            spot_value, fut_value, mmr
        );

        if spot_value <= 0.0 || fut_value <= 0.0 {
             error!("Calculated values are non-positive: spot={}, fut={}", spot_value, fut_value);
             return Err(anyhow::anyhow!("Calculated values are non-positive"));
        }

        let current_spot_price = self.exchange.get_spot_price(symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }
        let spot_order_qty = spot_value / current_spot_price;
        let fut_order_qty = fut_value / current_spot_price;
        let initial_limit_price = current_spot_price * (1.0 - self.slippage);
        debug!(
            "Calculated quantities based on price {}: spot_order_qty={}, fut_order_qty={}",
            current_spot_price, spot_order_qty, fut_order_qty
        );
        debug!("Initial limit price: {}", initial_limit_price);


        if spot_order_qty <= 0.0 || fut_order_qty <= 0.0 {
             error!("Calculated order quantities are non-positive.");
             return Err(anyhow::anyhow!("Calculated order quantities are non-positive"));
        }

        Ok(HedgeParams {
            spot_order_qty,
            fut_order_qty,
            current_spot_price,
            initial_limit_price,
            symbol: symbol.clone(),
        })
    }

    // --- ИЗМЕНЕНО: Принимаем operation_id и db ---
    pub async fn run_hedge(
        &self,
        params: HedgeParams,
        mut progress_callback: HedgeProgressCallback,
        current_order_id_storage: Arc<TokioMutex<Option<String>>>,
        total_filled_qty_storage: Arc<TokioMutex<f64>>,
        operation_id: i64, // <-- Добавлено
        db: &Db,           // <-- Добавлено
    ) -> Result<(f64, f64)>
    // --- Конец изменений ---
    {
        let HedgeParams {
            spot_order_qty: initial_spot_qty,
            fut_order_qty: initial_fut_qty,
            current_spot_price: mut current_spot_price,
            initial_limit_price: mut limit_price,
            symbol,
        } = params;

        let mut cumulative_filled_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty;
        let mut qty_filled_in_current_order = 0.0;
        let mut current_spot_order_id: Option<String> = None; // Локально храним ID текущего ордера

        info!(
            "Running hedge op_id:{} for {} with initial_spot_qty={}, initial_fut_qty={}, initial_limit_price={}",
            operation_id, symbol, initial_spot_qty, initial_fut_qty, limit_price
        );

        // --- ИЗМЕНЕНО: Клонируем Arc перед async move ---
        let update_current_order_id_local = |id: Option<String>| {
            // Клонируем Arc здесь, чтобы замыкание захватило клоны
            let storage_clone = current_order_id_storage.clone();
            let filled_storage_clone = total_filled_qty_storage.clone();
            let db_clone = db.clone(); // Клонируем пул для асинхронной задачи

            async move { // async move теперь захватывает клоны
                let id_clone_for_storage = id.clone();
                let id_clone_for_db = id.clone();
                // Используем клоны внутри async блока
                let filled_qty_for_db = *filled_storage_clone.lock().await;

                // Обновляем хранилище для отмены
                *storage_clone.lock().await = id_clone_for_storage;

                // Обновляем БД
                if let Err(e) = update_hedge_spot_order(
                    &db_clone,
                    operation_id,
                    id_clone_for_db.as_deref(), // Преобразуем Option<String> в Option<&str>
                    filled_qty_for_db,
                ).await {
                    error!("op_id:{}: Failed to update spot order info in DB: {}", operation_id, e);
                    // Не прерываем хедж, но логируем ошибку
                }
                id // Возвращаем ID для присвоения локальной переменной
            }
        };
        // --- Конец изменений ---

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
                    // --- ДОБАВЛЕНО: Обновляем статус в БД перед выходом ---
                    let _ = update_hedge_final_status(db, operation_id, "Failed", None, 0.0, Some(&e.to_string())).await;
                    // --- Конец добавления ---
                    return Err(e);
                }
            };
        info!("op_id:{}: Placed initial spot limit order: id={}", operation_id, spot_order.id);
        current_spot_order_id = update_current_order_id_local(Some(spot_order.id.clone())).await; // Обновляем всё
        *total_filled_qty_storage.lock().await = 0.0; // Общее исполненное пока 0

        let mut start = Instant::now();
        let mut last_update_sent = Instant::now();
        let update_interval = Duration::from_secs(5);

        // --- Обернем основной цикл в Result, чтобы использовать `?` и обработать ошибки в конце ---
        let hedge_loop_result: Result<()> = async {
            loop {
                sleep(Duration::from_secs(1)).await;
                let now = Instant::now();

                // Проверяем, есть ли активный ордер
                let order_id_to_check = match &current_spot_order_id {
                    Some(id) => id.clone(),
                    None => {
                        // Если ордера нет, но цель не достигнута, значит что-то пошло не так
                        if cumulative_filled_qty < initial_spot_qty - ORDER_FILL_TOLERANCE {
                            warn!("op_id:{}: No active order ID, but target not reached. Triggering replacement.", operation_id);
                            start = now - self.max_wait - Duration::from_secs(1); // Состариваем таймер
                            qty_filled_in_current_order = 0.0;
                        } else {
                            // Цель достигнута, выходим
                            break Ok(());
                        }
                        // Переходим к логике замены
                        continue;
                    }
                };


                tokio::task::yield_now().await;
                let status_result = self.exchange.get_order_status(&symbol, &order_id_to_check).await;

                let status: OrderStatus = match status_result {
                    Ok(s) => s,
                    Err(e) => {
                        if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                            warn!("op_id:{}: Order {} not found, assuming it filled for target qty {}. Continuing...", operation_id, order_id_to_check, current_order_target_qty);
                            cumulative_filled_qty += current_order_target_qty;
                            *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                            current_spot_order_id = update_current_order_id_local(None).await; // Обновляем всё
                            if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                info!("op_id:{}: Total filled spot quantity {} meets target {}. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                                break Ok(());
                            } else {
                                warn!("op_id:{}: Order filled assumption, but total target {} not reached. Triggering replacement logic.", operation_id, initial_spot_qty);
                                start = now - self.max_wait - Duration::from_secs(1);
                                qty_filled_in_current_order = 0.0;
                                continue;
                            }
                        } else {
                            warn!("op_id:{}: Failed to get order status for {}: {}. Aborting hedge.", operation_id, order_id_to_check, e);
                            // Ошибка получения статуса - выходим из цикла с ошибкой
                            return Err(anyhow!("Hedge failed during status check: {}", e));
                        }
                    }
                };

                let previously_filled_in_current = qty_filled_in_current_order;
                qty_filled_in_current_order = status.filled_qty;
                let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current;

                if filled_since_last_check > 0.0 {
                    cumulative_filled_qty += filled_since_last_check;
                    *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                    // --- ДОБАВЛЕНО: Обновляем БД при частичном исполнении ---
                    if let Err(e) = update_hedge_spot_order(
                        db,
                        operation_id,
                        current_spot_order_id.as_deref(),
                        cumulative_filled_qty, // Передаем общее исполненное
                    ).await {
                        error!("op_id:{}: Failed to update spot filled qty in DB: {}", operation_id, e);
                    }
                    // --- Конец добавления ---
                }

                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                    info!("op_id:{}: Current spot order {} filled (total cumulative: {}).", operation_id, order_id_to_check, cumulative_filled_qty);
                    current_spot_order_id = update_current_order_id_local(None).await; // Обновляем всё
                    if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                        info!("op_id:{}: Total filled spot quantity {} meets target {}. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                        break Ok(());
                    } else {
                        warn!("op_id:{}: Replacement order filled, but total target {} not reached. Triggering next replacement.", operation_id, initial_spot_qty);
                        start = now - self.max_wait - Duration::from_secs(1);
                        qty_filled_in_current_order = 0.0;
                    }
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

                    let remaining_total_qty = initial_spot_qty - cumulative_filled_qty;

                    tokio::task::yield_now().await;
                    if let Err(e) = self.exchange.cancel_order(&symbol, &order_id_to_check).await {
                        // Ошибки "не найден" игнорируются внутри cancel_order
                        warn!("op_id:{}: Attempt to cancel order {} failed or was ignored: {}", operation_id, order_id_to_check, e);
                    } else {
                        info!("op_id:{}: Successfully cancelled order {}", operation_id, order_id_to_check);
                    }
                    current_spot_order_id = update_current_order_id_local(None).await; // Обновляем всё

                    if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                        info!("op_id:{}: Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                        break Ok(());
                    }

                    tokio::task::yield_now().await;
                    current_spot_price = match self.exchange.get_spot_price(&symbol).await {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("op_id:{}: Failed to get spot price during replacement: {}. Aborting hedge.", operation_id, e);
                            return Err(anyhow!("Hedge failed during replacement (get price step): {}", e));
                        }
                    };
                    if current_spot_price <= 0.0 {
                        error!("op_id:{}: Invalid spot price received while replacing order: {}", operation_id, current_spot_price);
                        return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                    }
                    limit_price = current_spot_price * (1.0 - self.slippage);
                    price_for_update = current_spot_price;
                    current_order_target_qty = remaining_total_qty;
                    qty_filled_in_current_order = 0.0;

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
                    start = now;
                }

                // --- Логика обновления статуса в Telegram ---
                if is_replacement || elapsed_since_update > update_interval {
                    if !is_replacement {
                        tokio::task::yield_now().await;
                        match self.exchange.get_spot_price(&symbol).await {
                            Ok(p) if p > 0.0 => price_for_update = p,
                            Ok(_) => { /* Используем старую цену */ }
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
            } // Конец цикла loop
        }.await; // Завершаем async блок для hedge_loop_result

        // --- Обработка результата цикла и размещение фьючерсного ордера ---
        if let Err(loop_err) = hedge_loop_result {
            error!("op_id:{}: Hedge loop failed: {}", operation_id, loop_err);
            // Обновляем статус в БД как Failed
            let _ = update_hedge_final_status(db, operation_id, "Failed", None, cumulative_filled_qty, Some(&loop_err.to_string())).await;
            return Err(loop_err); // Возвращаем ошибку
        }

        // Если цикл завершился успешно (break Ok(())), размещаем фьючерсный ордер
        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        info!(
            "op_id:{}: Placing market sell order on futures ({}) for initial quantity {}",
            operation_id, futures_symbol, initial_fut_qty
        );
        tokio::task::yield_now().await;
        let fut_order_result = self
            .exchange
            .place_futures_market_order(&futures_symbol, OrderSide::Sell, initial_fut_qty)
            .await;

        match fut_order_result {
            Ok(fut_order) => {
                info!("op_id:{}: Placed futures market order: id={}", operation_id, fut_order.id);
                info!("op_id:{}: Hedge completed successfully. Total spot filled: {}", operation_id, cumulative_filled_qty);
                // Обновляем статус в БД как Completed
                let _ = update_hedge_final_status(db, operation_id, "Completed", Some(&fut_order.id), initial_fut_qty, None).await;
                Ok((cumulative_filled_qty, initial_fut_qty))
            }
            Err(e) => {
                warn!("op_id:{}: Failed to place futures order: {}. Spot was already bought!", operation_id, e);
                // Обновляем статус в БД как Failed (т.к. фьюч не разместился)
                let _ = update_hedge_final_status(db, operation_id, "Failed", None, cumulative_filled_qty, Some(&format!("Futures order failed: {}", e))).await;
                Err(anyhow!("Failed to place futures order after spot fill: {}", e))
            }
        }
    }

    // --- ИЗМЕНЕНО: Обновляем run_unhedge аналогично (пока без полной интеграции БД) ---
    pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
        // original_hedge_id: i64, // <-- Должен передаваться ID исходной операции
        // db: &Db,                // <-- Должен передаваться пул БД
        // TODO: Добавить поддержку отмены для unhedge по аналогии с run_hedge
        // TODO: Добавить колбэк прогресса для unhedge
        // TODO: Создавать запись в unhedge_operations и обновлять ее
        // TODO: Обновлять hedge_operations.unhedged_op_id при успехе
    ) -> Result<(f64, f64)> {
        let UnhedgeRequest { quantity: initial_spot_qty, symbol } = req;
        let initial_fut_qty = initial_spot_qty;

         info!(
            "Starting unhedge for {} with initial_quantity={}", // TODO: Добавить original_hedge_id в лог
            symbol, initial_spot_qty
        );

        let mut cumulative_filled_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty;
        let mut qty_filled_in_current_order = 0.0;

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
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);
        let mut start = Instant::now();

        // --- Обернем основной цикл в Result ---
        let unhedge_loop_result: Result<()> = async {
            loop {
                sleep(Duration::from_secs(1)).await;
                let now = Instant::now();

                let order_id_to_check = spot_order.id.clone(); // В unhedge пока не меняем ID

                let status: OrderStatus = match self.exchange.get_order_status(&symbol, &order_id_to_check).await {
                    Ok(s) => s,
                    Err(e) => {
                        if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                            warn!("Order {} not found, assuming it filled for target qty {}. Continuing...", order_id_to_check, current_order_target_qty);
                            cumulative_filled_qty += current_order_target_qty;
                            if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                info!("Total filled spot quantity {} meets target {}. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                                break Ok(());
                            } else {
                                warn!("Order filled assumption, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                                start = now - self.max_wait - Duration::from_secs(1);
                                qty_filled_in_current_order = 0.0;
                                continue;
                            }
                        } else {
                            warn!("Failed to get order status for {}: {}. Aborting unhedge.", order_id_to_check, e);
                            return Err(anyhow!("Unhedge failed during status check: {}", e));
                        }
                    }
                };

                let previously_filled_in_current = qty_filled_in_current_order;
                qty_filled_in_current_order = status.filled_qty;
                let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current;

                if filled_since_last_check > 0.0 {
                    cumulative_filled_qty += filled_since_last_check;
                    // TODO: Update unhedge_operations in DB
                }

                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                    info!("Current spot order {} filled (total cumulative: {}).", order_id_to_check, cumulative_filled_qty);
                    if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                        info!("Total filled spot quantity {} meets target {}. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                        break Ok(());
                    } else {
                        warn!("Replacement order filled, but total target {} not reached. Triggering next replacement.", initial_spot_qty);
                        start = now - self.max_wait - Duration::from_secs(1);
                        qty_filled_in_current_order = 0.0;
                    }
                }

                if start.elapsed() > self.max_wait && status.remaining_qty > ORDER_FILL_TOLERANCE {
                    warn!(
                        "Spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                        order_id_to_check, qty_filled_in_current_order, current_order_target_qty, self.max_wait
                    );

                    let remaining_total_qty = initial_spot_qty - cumulative_filled_qty;

                    if let Err(e) = self.exchange.cancel_order(&symbol, &order_id_to_check).await {
                         warn!("Order {} cancellation failed (likely already inactive): {}", order_id_to_check, e);
                         sleep(Duration::from_millis(500)).await;
                    } else {
                        info!("Successfully cancelled order {}", order_id_to_check);
                    }
                    // TODO: Update unhedge_operations in DB (order ID = None)

                    if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                        info!("Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", cumulative_filled_qty, initial_spot_qty);
                        break Ok(());
                    }

                    current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                    if current_spot_price <= 0.0 {
                        error!("Invalid spot price received while replacing order: {}", current_spot_price);
                        return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                    }
                    limit_price = current_spot_price * (1.0 + self.slippage);
                    current_order_target_qty = remaining_total_qty;
                    qty_filled_in_current_order = 0.0;

                    info!("New spot price: {}, placing new limit sell at {} for remaining qty {}", current_spot_price, limit_price, current_order_target_qty);
                    spot_order = self
                        .exchange
                        .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
                        .await?;
                    info!("Placed replacement spot limit order: id={}", spot_order.id);
                    // TODO: Update unhedge_operations in DB (new order ID)
                    start = Instant::now();
                }
                // TODO: Add progress callback call for unhedge here if needed
            } // Конец цикла loop
        }.await;

        if let Err(loop_err) = unhedge_loop_result {
             error!("Unhedge loop failed: {}", loop_err);
             // TODO: Update unhedge_operations status to Failed
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
                 // TODO: Update unhedge_operations status to Completed
                 // TODO: Update hedge_operations.unhedged_op_id
                 Ok((cumulative_filled_qty, initial_fut_qty))
             }
             Err(e) => {
                 warn!("Failed to place futures order during unhedge: {}. Spot was already sold!", e);
                 // TODO: Update unhedge_operations status to Failed
                 Err(anyhow!("Failed to place futures order after spot sell: {}", e))
             }
        }
    }
}