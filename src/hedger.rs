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
// --- ДОБАВЛЕНО: Импортируем Db и функции storage ---
use crate::storage::{Db, update_hedge_spot_order, update_hedge_final_status};
// --- Конец добавления ---


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

        tokio::task::yield_now().await; // Даем шанс другим задачам перед размещением ордера
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
                        // Если ордера нет, но цель *теоретически* не достигнута (из-за float),
                        // И прошло достаточно времени с момента обнуления ID (чтобы избежать мгновенной перестановки),
                        // то это ошибка логики, которая не должна происходить после исправления.
                        // Но на всякий случай добавим проверку и выход.
                        // Сравнение с initial_spot_qty здесь все еще нужно, чтобы понять, не завершился ли хедж с ошибкой ранее.
                        if cumulative_filled_qty < initial_spot_qty - ORDER_FILL_TOLERANCE && now.duration_since(start) > Duration::from_secs(2) {
                            warn!("op_id:{}: No active order ID, but target not reached and time elapsed. Aborting hedge.", operation_id);
                            return Err(anyhow!("No active spot order, but target not reached after fill/cancellation."));
                        } else {
                            // Если ордера нет и цель достигнута ИЛИ прошло мало времени - ждем дальше или выходим нормально.
                            // Эта ветка не должна вести к бесконечному циклу замен.
                            // Если cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE, то хедж успешно завершен.
                            // Но мы должны были выйти через break Ok(()) в блоке "if status.remaining_qty <= ORDER_FILL_TOLERANCE"
                            // Поэтому эта ветка 'else' скорее всего не нужна, но оставим для безопасности.
                             if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                info!("op_id:{}: No active order and target reached. Exiting loop.", operation_id);
                                break Ok(());
                             }
                            // Если target не достигнут, но ID=None, ждем (мало ли, ордер отменился и еще не заменился)
                            continue;
                        }
                    }
                };

                tokio::task::yield_now().await;
                let status_result = self.exchange.get_order_status(&symbol, &order_id_to_check).await;

                let status: OrderStatus = match status_result {
                    Ok(s) => s,
                    Err(e) => {
                        // Если ордер не найден *сразу* после постановки - это странно.
                        // Если прошло время - возможно, он быстро исполнился и исчез из API / был отменен.
                        if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                            // Предполагаем, что ордер исполнился на ВЕСЬ свой объем (current_order_target_qty)
                            // Это рискованно, но лучше, чем зависание.
                            warn!("op_id:{}: Order {} not found after delay, assuming it filled for its target qty {}. Continuing...", operation_id, order_id_to_check, current_order_target_qty);
                            cumulative_filled_qty += current_order_target_qty; // Добавляем ЦЕЛЕВОЙ объем этого ордера
                            *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                            current_spot_order_id = update_current_order_id_local(None).await; // Обнуляем ID
                             // Обновляем БД с новым cumulative_filled_qty
                            if let Err(db_err) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                error!("op_id:{}: Failed to update spot filled qty in DB after order not found: {}", operation_id, db_err);
                            }
                            // Проверяем, достигнута ли ОБЩАЯ цель
                            if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                info!("op_id:{}: Total filled spot quantity {} meets overall target {} after order not found assumption. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                                break Ok(());
                            } else {
                                warn!("op_id:{}: Assumed order filled, but overall target {} not reached. Triggering replacement logic.", operation_id, initial_spot_qty);
                                start = now - self.max_wait - Duration::from_secs(1); // Состариваем таймер для немедленной замены
                                qty_filled_in_current_order = 0.0;
                                continue; // Переходим к следующей итерации для логики замены
                            }
                        } else {
                            // Другая ошибка получения статуса - выходим из цикла с ошибкой
                            warn!("op_id:{}: Failed to get order status for {}: {}. Aborting hedge.", operation_id, order_id_to_check, e);
                            return Err(anyhow!("Hedge failed during status check: {}", e));
                        }
                    }
                };

                let previously_filled_in_current = qty_filled_in_current_order;
                qty_filled_in_current_order = status.filled_qty;
                let filled_since_last_check = qty_filled_in_current_order - previously_filled_in_current;

                if filled_since_last_check.abs() > ORDER_FILL_TOLERANCE { // Используем abs на случай отмены и возврата средств
                    cumulative_filled_qty += filled_since_last_check;
                    // Ограничиваем снизу нулем и сверху целью на всякий случай
                    cumulative_filled_qty = cumulative_filled_qty.max(0.0).min(initial_spot_qty * 1.01); // Добавляем небольшой буфер сверху
                    *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                    // Обновляем БД при любом изменении исполненного объема
                    if let Err(e) = update_hedge_spot_order(
                        db,
                        operation_id,
                        current_spot_order_id.as_deref(),
                        cumulative_filled_qty, // Передаем общее исполненное
                    ).await {
                        error!("op_id:{}: Failed to update spot filled qty in DB: {}", operation_id, e);
                    }
                }

                // ---> ИЗМЕНЕНО ЗДЕСЬ: Логика выхода из цикла при исполнении ордера <---
                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                    // Ордер считается исполненным биржей
                    info!("op_id:{}: Spot order {} considered filled by exchange (remaining_qty: {}, current order filled: {}, total cumulative: {}). Exiting loop.",
                           operation_id, order_id_to_check, status.remaining_qty, status.filled_qty, cumulative_filled_qty);
                    current_spot_order_id = update_current_order_id_local(None).await; // Обнуляем ID

                    // Доверяем статусу биржи и выходим из цикла ожидания спота
                    break Ok(());
                }
                // ---> Конец изменений <---


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

                    // Рассчитываем, сколько ЕЩЕ нужно купить для достижения ОБЩЕЙ цели
                    let remaining_total_qty = initial_spot_qty - cumulative_filled_qty;

                    tokio::task::yield_now().await;
                    if let Err(e) = self.exchange.cancel_order(&symbol, &order_id_to_check).await {
                        // Ошибки "не найден" игнорируются внутри cancel_order
                        warn!("op_id:{}: Attempt to cancel order {} failed or was ignored: {}", operation_id, order_id_to_check, e);
                        // Даем время на обработку отмены перед проверкой баланса/размещением
                        sleep(Duration::from_millis(500)).await;
                    } else {
                        info!("op_id:{}: Successfully sent cancel request for order {}", operation_id, order_id_to_check);
                         // Даем время на обработку отмены
                        sleep(Duration::from_millis(500)).await;
                    }
                    current_spot_order_id = update_current_order_id_local(None).await; // Обновляем всё (ID = None)
                    qty_filled_in_current_order = 0.0; // Сбрасываем счетчик для нового ордера

                    // Перепроверяем, не исполнился ли ордер полностью *после* отправки отмены
                     tokio::task::yield_now().await;
                     match self.exchange.get_order_status(&symbol, &order_id_to_check).await {
                         Ok(final_status) if final_status.remaining_qty <= ORDER_FILL_TOLERANCE => {
                             info!("op_id:{}: Order {} filled completely after cancel request. Adjusting cumulative qty.", operation_id, order_id_to_check);
                             // Корректируем cumulative_filled_qty до initial_spot_qty, так как ордер исполнился полностью
                             // Это может быть не совсем точно, если ордер был частичным, но биржа его закрыла
                             let filled_after_cancel = final_status.filled_qty - previously_filled_in_current;
                             if filled_after_cancel > 0.0 {
                                 cumulative_filled_qty += filled_after_cancel;
                                 *total_filled_qty_storage.lock().await = cumulative_filled_qty;
                                 if let Err(e) = update_hedge_spot_order(db, operation_id, None, cumulative_filled_qty).await {
                                     error!("op_id:{}: Failed to update spot filled qty in DB after fill during cancel: {}", operation_id, e);
                                 }
                             }
                             // Проверяем ОБЩУЮ цель снова
                             if cumulative_filled_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                                 info!("op_id:{}: Total filled quantity {} meets target {} after fill during cancel. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                                 break Ok(());
                             }
                         }
                         Ok(_) => { /* Ордер все еще активен или частично исполнен */ }
                         Err(e) => { warn!("op_id:{}: Failed to get order status after cancel request: {}", operation_id, e); }
                     }


                    // Если после всех проверок цель все еще не достигнута
                    if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                         info!("op_id:{}: Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", operation_id, cumulative_filled_qty, initial_spot_qty);
                         break Ok(());
                    }


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

                    // В колбэк передаем исполненное в *текущем* ордере и его цель
                    let update = HedgeProgressUpdate {
                        current_spot_price: price_for_update,
                        new_limit_price: limit_price,
                        is_replacement,
                        filled_qty: qty_filled_in_current_order, // Исполнено в этом ордере
                        target_qty: current_order_target_qty, // Цель этого ордера
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
                // Используем фактический cumulative_filled_qty для финального сообщения
                info!("op_id:{}: Hedge completed successfully. Total spot filled: {}", operation_id, cumulative_filled_qty);
                // Обновляем статус в БД как Completed
                let _ = update_hedge_final_status(db, operation_id, "Completed", Some(&fut_order.id), initial_fut_qty, None).await;
                // Возвращаем фактический исполненный объем спота и целевой фьюча
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
        // В unhedge объем фьючерса равен объему спота
        let initial_fut_qty = initial_spot_qty;

         info!(
            "Starting unhedge for {} with initial_quantity={}", // TODO: Добавить original_hedge_id в лог
            symbol, initial_spot_qty
        );

        let mut cumulative_filled_qty = 0.0;
        let mut current_order_target_qty = initial_spot_qty;
        let mut qty_filled_in_current_order = 0.0;
         let mut current_spot_order_id: Option<String> = None; // Для unhedge тоже нужен ID

        if initial_spot_qty <= 0.0 {
             error!("Initial quantity is non-positive: {}. Aborting unhedge.", initial_spot_qty);
             return Err(anyhow::anyhow!("Initial quantity is non-positive"));
        }

        let mut current_spot_price = self.exchange.get_spot_price(&symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }

        // В unhedge ставим ордер чуть ВЫШЕ рынка
        let mut limit_price = current_spot_price * (1.0 + self.slippage);
        info!("Initial spot price: {}, placing limit sell at {} for qty {}", current_spot_price, limit_price, current_order_target_qty);
        let spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);
        current_spot_order_id = Some(spot_order.id.clone()); // Сохраняем ID
        let mut start = Instant::now();

        // --- Обернем основной цикл в Result ---
        let unhedge_loop_result: Result<()> = async {
            loop {
                sleep(Duration::from_secs(1)).await;
                let now = Instant::now();

                 // Проверяем, есть ли активный ордер
                 let order_id_to_check = match &current_spot_order_id {
                    Some(id) => id.clone(),
                    None => {
                        // Аналогично hedge, если ID нет, но цель не достигнута - ошибка
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

                // ---> ИЗМЕНЕНО ЗДЕСЬ: Логика выхода из цикла при исполнении ордера <---
                if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                     info!("Unhedge spot order {} considered filled by exchange (remaining_qty: {}, current order filled: {}, total cumulative: {}). Exiting loop.",
                           order_id_to_check, status.remaining_qty, status.filled_qty, cumulative_filled_qty);
                     current_spot_order_id = None; // Обнуляем ID
                     break Ok(()); // Выходим из цикла
                }
                // ---> Конец изменений <---

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
                    current_spot_order_id = None; // Обнуляем ID
                    qty_filled_in_current_order = 0.0;

                     // Перепроверка статуса после отмены
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

                    limit_price = current_spot_price * (1.0 + self.slippage); // Цена ВЫШЕ рынка
                    current_order_target_qty = remaining_total_qty;

                    info!("New spot price: {}, placing new limit sell at {} for remaining qty {}", current_spot_price, limit_price, current_order_target_qty);
                    let new_spot_order = self
                        .exchange
                        .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
                        .await?;
                    info!("Placed replacement spot limit order: id={}", new_spot_order.id);
                    current_spot_order_id = Some(new_spot_order.id.clone()); // Сохраняем новый ID
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

        // Если цикл завершился успешно, покупаем фьючерс
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