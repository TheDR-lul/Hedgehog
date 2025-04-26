// src/hedger_ws/state.rs

use rust_decimal::Decimal;
use std::collections::HashMap;
use crate::exchange::types::{OrderSide, OrderStatusText, OrderbookLevel, DetailedOrderStatus};

/// Статус конкретного ордера внутри чанка
#[derive(Debug, Clone)]
pub struct ChunkOrderState {
    pub order_id: String,
    pub symbol: String,
    pub side: OrderSide,
    pub limit_price: Decimal, // <-- ДОБАВЛЕНО: Цена, по которой ордер был выставлен
    pub target_quantity: Decimal,
    pub filled_quantity: Decimal,
    pub filled_value: Decimal,
    pub average_price: Decimal,
    pub status: OrderStatusText,
}

impl ChunkOrderState {
    // Обновляем конструктор
    pub fn new(order_id: String, symbol: String, side: OrderSide, limit_price: Decimal, target_quantity: Decimal) -> Self {
        Self {
            order_id,
            symbol,
            side,
            limit_price, // <-- ДОБАВЛЕНО
            target_quantity,
            filled_quantity: Decimal::ZERO,
            filled_value: Decimal::ZERO,
            average_price: Decimal::ZERO,
            status: OrderStatusText::New,
        }
    }

    // Метод update_from_details остается без изменений по сигнатуре
    pub fn update_from_details(&mut self, details: &DetailedOrderStatus) {
        if self.order_id != details.order_id {
            tracing::warn!(
                "Attempted to update ChunkOrderState for order {} with details from order {}",
                self.order_id, details.order_id
            );
            return;
        }
        // Используем try_from для безопасной конвертации f64 -> Decimal
        self.filled_quantity = Decimal::try_from(details.filled_qty).unwrap_or(self.filled_quantity);
        self.filled_value = Decimal::try_from(details.cumulative_executed_value).unwrap_or(self.filled_value);
        self.average_price = Decimal::try_from(details.average_price).unwrap_or(self.average_price);
        self.status = details.status_text.clone();
    }
}


/// Идентификатор "ноги" операции (Спот или Фьючерс)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    Spot,
    Futures,
}

/// Общий статус задачи хеджирования/расхеджирования
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HedgerWsStatus {
    Initializing,
    SettingLeverage,
    ConnectingWebSocket,
    CalculatingChunks,
    StartingChunk(u32),
    PlacingSpotOrder(u32),
    PlacingFuturesOrder(u32),
    RunningChunk(u32),
    WaitingImbalance {
        chunk_index: u32,
        leading_leg: Leg,
    },
    // --- ДОБАВЛЕНО: Статусы для перестановки ---
    CancellingOrder {
        chunk_index: u32,
        leg_to_cancel: Leg,
        order_id_to_cancel: String,
        reason: String, // "StalePrice" или "Timeout" (если добавим таймаут)
    },
    WaitingCancelConfirmation { // Ожидаем OrderUpdate после отправки cancel
        chunk_index: u32,
        cancelled_leg: Leg,
        cancelled_order_id: String,
    },
    // --- КОНЕЦ ДОБАВЛЕНИЙ ---
    Reconciling,
    Completed,
    Cancelling, // Отмена пользователем всей операции
    Cancelled,
    Failed(String),
}


/// Последние полученные рыночные данные
#[derive(Debug, Clone, Default)]
pub struct MarketUpdate {
    pub best_bid_price: Option<Decimal>,
    pub best_bid_quantity: Option<Decimal>,
    pub best_ask_price: Option<Decimal>,
    pub best_ask_quantity: Option<Decimal>,
    pub last_update_time_ms: Option<i64>, // Добавим время обновления
}

/// Основная структура состояния для задачи HedgerWsTask
#[derive(Debug, Clone)]
pub struct HedgerWsState {
    pub operation_id: i64,
    pub operation_type: OperationType,
    pub symbol_spot: String,
    pub symbol_futures: String,
    // Параметры инструментов (сохраняем при инициализации)
    pub spot_tick_size: Decimal,
    pub spot_quantity_step: Decimal,
    pub futures_tick_size: Decimal,
    pub futures_quantity_step: Decimal,
    pub min_spot_quantity: Decimal,
    pub min_futures_quantity: Decimal,
    pub min_spot_notional: Option<Decimal>,
    pub min_futures_notional: Option<Decimal>,
    // Параметры чанков
    pub total_chunks: u32,
    pub chunk_base_quantity_spot: Decimal,
    pub chunk_base_quantity_futures: Decimal,
    // Отслеживание прогресса
    pub current_chunk_index: u32,
    pub cumulative_spot_filled_quantity: Decimal,
    pub cumulative_spot_filled_value: Decimal,
    pub cumulative_futures_filled_quantity: Decimal,
    pub cumulative_futures_filled_value: Decimal,
    pub target_total_futures_value: Decimal,
    // Общая цель операции
    pub initial_target_spot_value: Decimal,
    pub initial_target_futures_qty: Decimal,
    // Состояние текущего чанка
    pub active_spot_order: Option<ChunkOrderState>,
    pub active_futures_order: Option<ChunkOrderState>,
    // Последние рыночные данные
    pub spot_market_data: MarketUpdate,
    pub futures_market_data: MarketUpdate,
    // Общий статус
    pub status: HedgerWsStatus,
}

/// Тип операции (для состояния)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationType {
    Hedge,
    Unhedge,
}

// Конструкторы new_hedge / new_unhedge нужно будет обновить,
// чтобы они сохраняли информацию об инструментах (tick_size и т.д.)
impl HedgerWsState {
     // ... (оставляем конструкторы new_hedge / new_unhedge) ...
     // Их нужно будет доработать в hedge_task.rs/unhedge_task.rs,
     // чтобы сохранять tick_size, qty_step и т.д.
     pub fn new_hedge(
        operation_id: i64,
        symbol_spot: String,
        symbol_futures: String,
        initial_target_spot_value: Decimal, // Начальная оценка
        initial_target_futures_qty: Decimal // Начальная оценка
    ) -> Self {
        // ЗАГЛУШКА: Нужно будет передавать и сохранять реальные лимиты
        let zero = Decimal::ZERO;
        Self {
            operation_id,
            operation_type: OperationType::Hedge,
            symbol_spot,
            symbol_futures,
            spot_tick_size: zero, futures_tick_size: zero, // Placeholder
            spot_quantity_step: zero, futures_quantity_step: zero, // Placeholder
            min_spot_quantity: zero, min_futures_quantity: zero, // Placeholder
            min_spot_notional: None, min_futures_notional: None, // Placeholder
            total_chunks: 0,
            chunk_base_quantity_spot: Decimal::ZERO,
            chunk_base_quantity_futures: Decimal::ZERO,
            current_chunk_index: 0,
            cumulative_spot_filled_quantity: Decimal::ZERO,
            cumulative_spot_filled_value: Decimal::ZERO,
            cumulative_futures_filled_quantity: Decimal::ZERO,
            cumulative_futures_filled_value: Decimal::ZERO,
            target_total_futures_value: initial_target_spot_value,
            initial_target_spot_value,
            initial_target_futures_qty,
            active_spot_order: None,
            active_futures_order: None,
            spot_market_data: MarketUpdate::default(),
            futures_market_data: MarketUpdate::default(),
            status: HedgerWsStatus::Initializing,
        }
    }

      pub fn new_unhedge(
         operation_id: i64,
         symbol_spot: String,
         symbol_futures: String,
         target_spot_sell_qty: Decimal,
         target_futures_buy_qty: Decimal
     ) -> Self {
          // ЗАГЛУШКА: Нужно будет передавать и сохранять реальные лимиты
         let zero = Decimal::ZERO;
         let initial_target_spot_value_estimate = target_spot_sell_qty; // Упрощенно

         Self {
             operation_id,
             operation_type: OperationType::Unhedge,
             symbol_spot,
             symbol_futures,
             spot_tick_size: zero, futures_tick_size: zero, // Placeholder
             spot_quantity_step: zero, futures_quantity_step: zero, // Placeholder
             min_spot_quantity: zero, min_futures_quantity: zero, // Placeholder
             min_spot_notional: None, min_futures_notional: None, // Placeholder
             total_chunks: 0,
             chunk_base_quantity_spot: Decimal::ZERO,
             chunk_base_quantity_futures: Decimal::ZERO,
             current_chunk_index: 0,
             cumulative_spot_filled_quantity: Decimal::ZERO,
             cumulative_spot_filled_value: Decimal::ZERO,
             cumulative_futures_filled_quantity: Decimal::ZERO,
             cumulative_futures_filled_value: Decimal::ZERO,
             target_total_futures_value: Decimal::ZERO,
             initial_target_spot_value: initial_target_spot_value_estimate,
             initial_target_futures_qty: target_futures_buy_qty,
             active_spot_order: None,
             active_futures_order: None,
             spot_market_data: MarketUpdate::default(),
             futures_market_data: MarketUpdate::default(),
             status: HedgerWsStatus::Initializing,
         }
     }
}