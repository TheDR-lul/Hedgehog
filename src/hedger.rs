// src/hedger.rs

use anyhow::{anyhow, Result};
use tracing::{debug, error, info, warn};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use futures::future::BoxFuture;

// --- Используемые типажи и структуры ---
use crate::exchange::{Exchange, OrderStatus, FeeRate}; // Добавляем FeeRate
use crate::exchange::bybit::{SPOT_CATEGORY, LINEAR_CATEGORY}; // Импортируем константы
use crate::exchange::types::OrderSide;
use crate::models::{HedgeRequest, UnhedgeRequest};

const ORDER_FILL_TOLERANCE: f64 = 1e-8; // Допуск для проверки полного исполнения

// --- ИЗМЕНЕНО: Добавляем Clone ---
#[derive(Clone)] // <-- Добавлено
pub struct Hedger<E> {
    exchange:     E,
    slippage:     f64,
    max_wait:     Duration,
    quote_currency: String,
}

#[derive(Debug)]
pub struct HedgeParams {
    pub spot_order_qty: f64, // Изначальное количество для спота
    pub fut_order_qty: f64,  // Изначальное количество для фьючерса
    pub current_spot_price: f64,
    pub initial_limit_price: f64,
    pub symbol: String,
}

#[derive(Debug, Clone)]
pub struct HedgeProgressUpdate {
    pub current_spot_price: f64,
    pub new_limit_price: f64,
    pub is_replacement: bool,
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

        // --- Опционально: Получаем и логируем комиссию ---
        match self.exchange.get_fee_rate(symbol, SPOT_CATEGORY).await {
            Ok(fee) => info!(symbol, category=SPOT_CATEGORY, maker_fee=fee.maker, taker_fee=fee.taker, "Current spot fee rate"),
            Err(e) => warn!("Could not get spot fee rate for {}: {}", symbol, e),
        }
        match self.exchange.get_fee_rate(symbol, LINEAR_CATEGORY).await {
             Ok(fee) => info!(symbol, category=LINEAR_CATEGORY, maker_fee=fee.maker, taker_fee=fee.taker, "Current futures fee rate"),
             Err(e) => warn!("Could not get futures fee rate for {}: {}", symbol, e),
        }
        // --- Конец опциональной части ---


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


    pub async fn run_hedge(
        &self,
        params: HedgeParams,
        mut progress_callback: HedgeProgressCallback,
    ) -> Result<(f64, f64)> { // Возвращает (итоговый_спот_объем, итоговый_фьюч_объем)

        let HedgeParams {
            spot_order_qty: initial_spot_qty, // Переименовываем для ясности
            fut_order_qty: initial_fut_qty,   // Переименовываем для ясности
            current_spot_price: mut current_spot_price,
            initial_limit_price: mut limit_price,
            symbol,
        } = params;

        // --- ИЗМЕНЕНО: Переменные для отслеживания исполнения ---
        let mut total_filled_spot_qty = 0.0; // Сколько всего исполнено на споте
        let mut current_order_target_qty = initial_spot_qty; // Сколько должен исполнить ТЕКУЩИЙ ордер
        // --- Конец изменений ---

        info!(
            "Running hedge for {} with initial_spot_qty={}, initial_fut_qty={}, initial_limit_price={}",
            symbol, initial_spot_qty, initial_fut_qty, limit_price
        );

        info!("Placing initial limit buy at {} for qty {}", limit_price, current_order_target_qty);
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);

        let mut start = Instant::now();
        let mut last_update_sent = Instant::now();
        let update_interval = Duration::from_secs(5);

        loop {
            sleep(Duration::from_secs(1)).await;
            let now = Instant::now();

            // --- ИЗМЕНЕНО: Получаем статус текущего ордера ---
            let status: OrderStatus = match self.exchange.get_order_status(&symbol, &spot_order.id).await {
                Ok(s) => s,
                Err(e) => {
                    // Если ордер не найден (возможно, уже исполнен и удален из realtime),
                    // попробуем считать его исполненным на target_qty, если прошло время
                    if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) { // Небольшая задержка
                         warn!("Order {} not found, assuming it filled for target qty {}. Continuing...", spot_order.id, current_order_target_qty);
                         total_filled_spot_qty += current_order_target_qty; // Добавляем целевое кол-во
                         // Проверяем, достигли ли общего целевого объема
                         if total_filled_spot_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                             info!("Total filled spot quantity {} meets target {}. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                             break; // Выходим из цикла, если общий объем достигнут
                         } else {
                             // Если общий объем не достигнут, нужно переставить ордер на остаток
                             warn!("Order filled assumption, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                             start = now - self.max_wait - Duration::from_secs(1); // Искусственно "состариваем" таймер, чтобы сработала логика замены
                             continue; // Переходим к следующей итерации, где сработает замена
                         }
                    } else {
                        warn!("Failed to get order status for {}: {}. Retrying...", spot_order.id, e);
                        continue; // Повторяем попытку получить статус
                    }
                }
            };
            // --- Конец изменений ---

            // --- ИЗМЕНЕНО: Проверяем исполнение ТЕКУЩЕГО ордера ---
            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                info!("Current spot order {} filled for qty {}.", spot_order.id, status.filled_qty);
                total_filled_spot_qty += status.filled_qty; // Добавляем фактически исполненное
                // Проверяем, достигли ли общего целевого объема
                if total_filled_spot_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                    info!("Total filled spot quantity {} meets target {}. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                    break; // Выходим из цикла
                } else {
                    // Если текущий ордер исполнился, но общий объем не достигнут (маловероятно при текущей логике, но на всякий случай)
                    warn!("Current order filled, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                    start = now - self.max_wait - Duration::from_secs(1); // Искусственно "состариваем" таймер
                    // Переходим к следующей итерации, где сработает замена на остаток
                }
            }
            // --- Конец изменений ---

            let elapsed_since_start = now.duration_since(start);
            let elapsed_since_update = now.duration_since(last_update_sent);

            let mut is_replacement = false;
            let mut price_for_update = current_spot_price;

            // --- Логика перестановки ордера ---
            if elapsed_since_start > self.max_wait {
                is_replacement = true;
                warn!(
                    "Spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                    spot_order.id, status.filled_qty, current_order_target_qty, self.max_wait
                );

                // --- ИЗМЕНЕНО: Учитываем уже исполненное ---
                total_filled_spot_qty += status.filled_qty; // Добавляем то, что успело исполниться в этом ордере
                let remaining_total_qty = initial_spot_qty - total_filled_spot_qty; // Сколько ЕЩЕ НУЖНО купить ВСЕГО
                // --- Конец изменений ---

                // Отменяем старый ордер
                if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                    if !e.to_string().contains("110025") && !e.to_string().contains("10001") && !e.to_string().contains("170106") {
                        error!("Unexpected error cancelling order {}: {}", spot_order.id, e);
                    }
                    sleep(Duration::from_millis(500)).await;
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                // --- ИЗМЕНЕНО: Проверяем, нужно ли еще покупать ---
                if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                    info!("Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                    break; // Выходим, если уже всё купили
                }
                // --- Конец изменений ---

                // Получаем новую цену и ставим ордер на ОСТАТОК
                current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 - self.slippage);
                price_for_update = current_spot_price;

                // --- ИЗМЕНЕНО: Обновляем целевое кол-во для нового ордера ---
                current_order_target_qty = remaining_total_qty;
                // --- Конец изменений ---

                info!("New spot price: {}, placing new limit buy at {} for remaining qty {}", current_spot_price, limit_price, current_order_target_qty);
                spot_order = self.exchange.place_limit_order(&symbol, OrderSide::Buy, current_order_target_qty, limit_price).await?;
                info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = now; // Сбрасываем таймер ожидания для НОВОГО ордера
            }

            // --- Логика обновления статуса в Telegram ---
            if is_replacement || elapsed_since_update > update_interval {
                if !is_replacement {
                    match self.exchange.get_spot_price(&symbol).await {
                        Ok(p) if p > 0.0 => price_for_update = p,
                        _ => { /* Используем последнюю известную */ }
                    }
                }

                let update = HedgeProgressUpdate {
                    current_spot_price: price_for_update,
                    new_limit_price: limit_price, // Цена текущего активного ордера
                    is_replacement,
                };

                if let Err(e) = progress_callback(update).await {
                    // Игнорируем ошибку "message is not modified"
                    if !e.to_string().contains("message is not modified") {
                        warn!("Progress callback failed: {}. Continuing hedge...", e);
                    }
                }
                last_update_sent = now;
            }
        } // Конец цикла loop

        // --- Размещение фьючерсного ордера ---
        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        // --- ИЗМЕНЕНО: Используем initial_fut_qty ---
        info!(
            "Placing market sell order on futures ({}) for initial quantity {}",
            futures_symbol, initial_fut_qty
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Sell, initial_fut_qty)
            .await?;
        info!("Placed futures market order: id={}", fut_order.id);
        // --- Конец изменений ---

        info!("Hedge completed successfully for {}. Total spot filled: {}", symbol, total_filled_spot_qty);
        // --- ИЗМЕНЕНО: Возвращаем фактически исполненный объем спота и изначальный объем фьюча ---
        Ok((total_filled_spot_qty, initial_fut_qty))
        // --- Конец изменений ---
    }


    pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
        // mut progress_callback: Option<HedgeProgressCallback>, // Пока без колбэка
    ) -> Result<(f64, f64)> { // Возвращает (итоговый_спот_объем, итоговый_фьюч_объем)
        let UnhedgeRequest { quantity: initial_spot_qty, symbol } = req; // Переименовываем quantity
        let initial_fut_qty = initial_spot_qty; // Объем для фьюча равен объему для спота

         info!(
            "Starting unhedge for {} with initial_quantity={}",
            symbol, initial_spot_qty
        );

        // --- ИЗМЕНЕНО: Переменные для отслеживания исполнения ---
        let mut total_filled_spot_qty = 0.0; // Сколько всего исполнено на споте
        let mut current_order_target_qty = initial_spot_qty; // Сколько должен исполнить ТЕКУЩИЙ ордер
        // --- Конец изменений ---

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

        loop {
            sleep(Duration::from_secs(1)).await;
            let now = Instant::now();

             // --- ИЗМЕНЕНО: Получаем статус текущего ордера ---
             let status: OrderStatus = match self.exchange.get_order_status(&symbol, &spot_order.id).await {
                Ok(s) => s,
                Err(e) => {
                    if e.to_string().contains("Order not found") && now.duration_since(start) > Duration::from_secs(5) {
                         warn!("Order {} not found, assuming it filled for target qty {}. Continuing...", spot_order.id, current_order_target_qty);
                         total_filled_spot_qty += current_order_target_qty;
                         if total_filled_spot_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                             info!("Total filled spot quantity {} meets target {}. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                             break;
                         } else {
                             warn!("Order filled assumption, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                             start = now - self.max_wait - Duration::from_secs(1);
                             continue;
                         }
                    } else {
                        warn!("Failed to get order status for {}: {}. Retrying...", spot_order.id, e);
                        continue;
                    }
                }
            };
            // --- Конец изменений ---

            // --- ИЗМЕНЕНО: Проверяем исполнение ТЕКУЩЕГО ордера ---
            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                 info!("Current spot order {} filled for qty {}.", spot_order.id, status.filled_qty);
                 total_filled_spot_qty += status.filled_qty;
                 if total_filled_spot_qty >= initial_spot_qty - ORDER_FILL_TOLERANCE {
                     info!("Total filled spot quantity {} meets target {}. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                     break;
                 } else {
                     warn!("Current order filled, but total target {} not reached. Triggering replacement logic.", initial_spot_qty);
                     start = now - self.max_wait - Duration::from_secs(1);
                 }
            }
            // --- Конец изменений ---

            // --- Логика перестановки ордера ---
            if start.elapsed() > self.max_wait { // Используем start, а не now.duration_since(start)
                 warn!(
                    "Spot order {} partially filled ({}/{}) or not filled within {:?}. Replacing...",
                    spot_order.id, status.filled_qty, current_order_target_qty, self.max_wait
                );

                 // --- ИЗМЕНЕНО: Учитываем уже исполненное ---
                 total_filled_spot_qty += status.filled_qty;
                 let remaining_total_qty = initial_spot_qty - total_filled_spot_qty;
                 // --- Конец изменений ---

                 // Отменяем старый ордер
                 if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                    if !e.to_string().contains("110025") && !e.to_string().contains("10001") && !e.to_string().contains("170106") {
                        error!("Unexpected error cancelling order {}: {}", spot_order.id, e);
                    }
                     sleep(Duration::from_millis(500)).await;
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                 // --- ИЗМЕНЕНО: Проверяем, нужно ли еще продавать ---
                 if remaining_total_qty <= ORDER_FILL_TOLERANCE {
                    info!("Total filled quantity {} meets target {}. No need to replace order. Exiting loop.", total_filled_spot_qty, initial_spot_qty);
                    break;
                 }
                 // --- Конец изменений ---

                // Получаем новую цену и ставим ордер на ОСТАТОК
                current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 + self.slippage); // Для продажи + slippage

                // --- ИЗМЕНЕНО: Обновляем целевое кол-во для нового ордера ---
                current_order_target_qty = remaining_total_qty;
                // --- Конец изменений ---

                info!("New spot price: {}, placing new limit sell at {} for remaining qty {}", current_spot_price, limit_price, current_order_target_qty);
                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Sell, current_order_target_qty, limit_price)
                    .await?;
                 info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = Instant::now(); // Сбрасываем таймер для НОВОГО ордера
            }
        } // Конец цикла loop

        // --- Размещение фьючерсного ордера ---
        let futures_symbol = format!("{}{}", symbol, self.quote_currency);
        // --- ИЗМЕНЕНО: Используем initial_fut_qty ---
        info!(
            "Placing market buy order on futures ({}) for initial quantity {}",
            futures_symbol, initial_fut_qty
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Buy, initial_fut_qty)
            .await?;
        info!("Placed futures market order: id={}", fut_order.id);
        // --- Конец изменений ---

        info!("Unhedge completed successfully for {}. Total spot filled: {}", symbol, total_filled_spot_qty);
        // --- ИЗМЕНЕНО: Возвращаем фактически исполненный объем спота и изначальный объем фьюча ---
        Ok((total_filled_spot_qty, initial_fut_qty))
        // --- Конец изменений ---
    }
}
