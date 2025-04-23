// src/hedger.rs

use anyhow::{anyhow, Result};
use tracing::{debug, error, info, warn};
use std::time::{Duration, Instant};
use tokio::time::sleep;
// --- ИМПОРТЫ для колбэка ---
use futures::future::BoxFuture; // Для асинхронного колбэка
// --- Конец импортов ---

use crate::exchange::{Exchange, OrderStatus};
use crate::exchange::types::OrderSide;
use crate::models::{HedgeRequest, UnhedgeRequest};

// Константы
const ORDER_FILL_TOLERANCE: f64 = 1e-8;
const FUTURES_SYMBOL_SUFFIX: &str = "USDT";

// Структура Hedger
pub struct Hedger<E> {
    exchange:     E,
    slippage:     f64,
    commission:   f64,
    max_wait:     Duration,
}

// Структура для возврата рассчитанных параметров
#[derive(Debug)]
pub struct HedgeParams {
    pub spot_order_qty: f64,
    pub fut_order_qty: f64,
    pub current_spot_price: f64,
    pub initial_limit_price: f64,
    pub symbol: String,
}

// Структура для обновления прогресса
#[derive(Debug, Clone)]
pub struct HedgeProgressUpdate {
    pub current_spot_price: f64,
    pub new_limit_price: f64,
}

// Тип для асинхронного колбэка
// Колбэк принимает обновление и возвращает Future, который может завершиться ошибкой
pub type HedgeProgressCallback = Box<dyn FnMut(HedgeProgressUpdate) -> BoxFuture<'static, Result<()>> + Send + Sync>;


impl<E> Hedger<E>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    /// slippage и commission — доли (0.005 = 0.5%, 0.001 = 0.1%)
    /// max_wait_secs - время в секундах, после которого переставляем лимитный ордер
    pub fn new(exchange: E, slippage: f64, commission: f64, max_wait_secs: u64) -> Self {
        Self {
            exchange,
            slippage,
            commission,
            max_wait: Duration::from_secs(max_wait_secs),
        }
    }

    /// Расчет параметров хеджирования
    pub async fn calculate_hedge_params(&self, req: &HedgeRequest) -> Result<HedgeParams> {
        let HedgeRequest { sum, symbol, volatility } = req;
        debug!(
            "Calculating hedge params for {} with sum={}, volatility={}",
            symbol, sum, volatility
        );

        // 1) Корректируем сумму
        let adj_sum = sum * (1.0 - self.commission);
        debug!("Adjusted sum: {}", adj_sum);

        // 2) Считаем VALUE
        let mmr = self.exchange.get_mmr(symbol).await?;
        let spot_value = adj_sum / ((1.0 + volatility) * (1.0 + mmr));
        let fut_value  = adj_sum - spot_value;
        debug!(
            "Calculated values: spot_value={}, fut_value={}, MMR={}",
            spot_value, fut_value, mmr
        );

        if spot_value <= 0.0 || fut_value <= 0.0 {
             error!("Calculated values are non-positive: spot={}, fut={}", spot_value, fut_value);
             return Err(anyhow::anyhow!("Calculated values are non-positive"));
        }

        // 3) Получаем цену, считаем QUANTITY и лимитную цену
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


    /// Цикл хеджирования:
    /// Принимает рассчитанные параметры и колбэк для обновлений
    pub async fn run_hedge(
        &self,
        params: HedgeParams,
        mut progress_callback: HedgeProgressCallback, // <-- Принимаем колбэк
    ) -> Result<(f64, f64)> {

        let HedgeParams {
            spot_order_qty,
            fut_order_qty,
            current_spot_price: mut current_spot_price,
            initial_limit_price: mut limit_price,
            symbol,
        } = params;

        info!(
            "Running hedge for {} with spot_qty={}, fut_qty={}, initial_limit_price={}",
            symbol, spot_order_qty, fut_order_qty, limit_price
        );

        info!("Placing initial limit buy at {} for qty {}", limit_price, spot_order_qty);
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Buy, spot_order_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);
        let mut start = Instant::now();

        loop {
            sleep(Duration::from_secs(1)).await; // Пауза между проверками статуса
            let status: OrderStatus = match self.exchange.get_order_status(&symbol, &spot_order.id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to get order status for {}: {}. Retrying...", spot_order.id, e);
                    continue; // Пропускаем итерацию, попробуем снова
                }
            };

            // Проверяем, исполнен ли ордер
            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                info!("Spot order {} filled.", spot_order.id);
                break; // Выходим из цикла, если ордер исполнен
            }

            // Проверяем, не пора ли переставить ордер
            if start.elapsed() > self.max_wait {
                warn!(
                    "Spot order {} not filled within {:?}. Replacing...",
                    spot_order.id, self.max_wait
                );
                // Пытаемся отменить старый ордер
                if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                    // Логируем ошибку, но продолжаем, пытаясь разместить новый
                    error!(
                        "Failed to cancel order {} before replacing: {}. Continuing, but risk of double order exists!",
                        spot_order.id, e
                    );
                    // Добавляем небольшую паузу на всякий случай перед размещением нового
                    sleep(Duration::from_millis(500)).await;
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                // Получаем новую цену и рассчитываем новую лимитную цену
                current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    // Возвращаем ошибку, чтобы уведомить пользователя и прервать процесс
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 - self.slippage);
                info!("New spot price: {}, placing new limit buy at {} for qty {}", current_spot_price, limit_price, spot_order_qty);

                // --- ВЫЗОВ КОЛБЭКА ---
                let update = HedgeProgressUpdate {
                    current_spot_price,
                    new_limit_price: limit_price,
                };
                // Вызываем колбэк и логируем ошибку, если она есть, но не прерываем хедж
                if let Err(e) = progress_callback(update).await {
                    warn!("Progress callback failed: {}. Continuing hedge...", e);
                }
                // --- Конец вызова колбэка ---

                // Размещаем новый ордер
                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Buy, spot_order_qty, limit_price)
                    .await?;
                info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = Instant::now(); // Сбрасываем таймер ожидания
            }
        } // Конец цикла ожидания/замены спот-ордера

        // 5) рыночный SELL на фьюче (после успешного исполнения спот-ордера)
        let futures_symbol = format!("{}{}", symbol, FUTURES_SYMBOL_SUFFIX);
        info!(
            "Placing market sell order on futures ({}) for quantity {}",
            futures_symbol, fut_order_qty
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Sell, fut_order_qty)
            .await?;
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Hedge completed successfully for {}", symbol);
        // Возвращаем реальные объемы ордеров
        Ok((spot_order_qty, fut_order_qty))
    }

    /// Цикл расхеджирования:
    /// Принимает запрос и, опционально, колбэк для обновлений
    pub async fn run_unhedge(
        &self,
        req: UnhedgeRequest,
        // Можно добавить: mut progress_callback: Option<HedgeProgressCallback>,
    ) -> Result<(f64, f64)> {
        let UnhedgeRequest { sum: quantity, symbol } = req;
         info!(
            "Starting unhedge for {} with quantity={}",
            symbol, quantity
        );

        let spot_order_qty = quantity;
        let fut_order_qty = quantity;
        info!("Using order quantity: {}", spot_order_qty);

        if spot_order_qty <= 0.0 {
             error!("Order quantity is non-positive: {}. Aborting unhedge.", spot_order_qty);
             return Err(anyhow::anyhow!("Order quantity is non-positive"));
        }

        // Получаем цену для установки лимитного ордера
        let mut current_spot_price = self.exchange.get_spot_price(&symbol).await?;
        if current_spot_price <= 0.0 {
            error!("Invalid spot price received: {}", current_spot_price);
            return Err(anyhow!("Invalid spot price: {}", current_spot_price));
        }

        // limit SELL на споте с реплейсом
        let mut limit_price = current_spot_price * (1.0 + self.slippage);
        info!("Initial spot price: {}, placing limit sell at {} for qty {}", current_spot_price, limit_price, spot_order_qty);
        let mut spot_order = self
            .exchange
            .place_limit_order(&symbol, OrderSide::Sell, spot_order_qty, limit_price)
            .await?;
        info!("Placed initial spot limit order: id={}", spot_order.id);
        let mut start = Instant::now();

        loop {
            sleep(Duration::from_secs(1)).await;
             let status: OrderStatus = match self.exchange.get_order_status(&symbol, &spot_order.id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to get order status for {}: {}. Retrying...", spot_order.id, e);
                    continue;
                }
            };

            if status.remaining_qty <= ORDER_FILL_TOLERANCE {
                 info!("Spot order {} filled.", spot_order.id);
                break;
            }

            if start.elapsed() > self.max_wait {
                 warn!(
                    "Spot order {} not filled within {:?}. Replacing...",
                    spot_order.id, self.max_wait
                );
                if let Err(e) = self.exchange.cancel_order(&symbol, &spot_order.id).await {
                     error!(
                        "Failed to cancel order {} before replacing: {}. Continuing, but risk of double order exists!",
                        spot_order.id, e
                    );
                     sleep(Duration::from_millis(500)).await;
                } else {
                    info!("Successfully cancelled order {}", spot_order.id);
                }

                current_spot_price = self.exchange.get_spot_price(&symbol).await?;
                 if current_spot_price <= 0.0 {
                    error!("Invalid spot price received while replacing order: {}", current_spot_price);
                    return Err(anyhow!("Invalid spot price while replacing order: {}", current_spot_price));
                }
                limit_price = current_spot_price * (1.0 + self.slippage);
                info!("New spot price: {}, placing new limit sell at {} for qty {}", current_spot_price, limit_price, spot_order_qty);

                // --- МЕСТО ДЛЯ ВЫЗОВА КОЛБЭКА (если нужно для unhedge) ---
                // if let Some(ref mut cb) = progress_callback {
                //     let update = HedgeProgressUpdate { current_spot_price, new_limit_price: limit_price };
                //     if let Err(e) = cb(update).await {
                //         warn!("Unhedge progress callback failed: {}. Continuing...", e);
                //     }
                // }
                // --- Конец места для вызова колбэка ---

                spot_order = self
                    .exchange
                    .place_limit_order(&symbol, OrderSide::Sell, spot_order_qty, limit_price)
                    .await?;
                 info!("Placed replacement spot limit order: id={}", spot_order.id);
                start = Instant::now();
            }
        }

        // market BUY на фьюче
        let futures_symbol = format!("{}{}", symbol, FUTURES_SYMBOL_SUFFIX);
        info!(
            "Placing market buy order on futures ({}) for quantity {}",
            futures_symbol, fut_order_qty
        );
        let fut_order = self
            .exchange
            .place_market_order(&futures_symbol, OrderSide::Buy, fut_order_qty)
            .await?;
        info!("Placed futures market order: id={}", fut_order.id);

        info!("Unhedge completed successfully for {}", symbol);
        // Возвращаем объем, который был обработан на споте и фьюче
        Ok((spot_order_qty, fut_order_qty))
    }
}