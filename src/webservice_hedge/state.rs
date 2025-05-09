// src/webservice_hedge/state.rs

use rust_decimal::Decimal;
use crate::exchange::types::{OrderSide, OrderStatusText, DetailedOrderStatus};

#[derive(Debug, Clone)]
pub struct ChunkOrderState {
    pub order_id: String,
    pub symbol: String,
    pub side: OrderSide,
    pub limit_price: Decimal, 
    pub target_quantity: Decimal,
    pub filled_quantity: Decimal,
    pub filled_value: Decimal,
    pub average_price: Decimal,
    pub status: OrderStatusText,
}

impl ChunkOrderState {
    pub fn new(order_id: String, symbol: String, side: OrderSide, limit_price: Decimal, target_quantity: Decimal) -> Self {
        Self {
            order_id,
            symbol,
            side,
            limit_price, 
            target_quantity,
            filled_quantity: Decimal::ZERO,
            filled_value: Decimal::ZERO,
            average_price: Decimal::ZERO,
            status: OrderStatusText::New,
        }
    }

    pub fn update_from_details(&mut self, details: &DetailedOrderStatus) {
        if self.order_id != details.order_id {
            tracing::warn!(
                "Attempted to update ChunkOrderState for order {} with details from order {}",
                self.order_id, details.order_id
            );
            return;
        }
        self.filled_quantity = Decimal::try_from(details.filled_qty).unwrap_or(self.filled_quantity);
        self.filled_value = Decimal::try_from(details.cumulative_executed_value).unwrap_or(self.filled_value);
        self.average_price = Decimal::try_from(details.average_price).unwrap_or(self.average_price);
        self.status = details.status_text.clone();
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    Spot,
    Futures,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HedgerWsStatus {
    Initializing,
    SettingLeverage,
    ConnectingWebSocket, // Этот статус может быть уже не так актуален, если соединения создаются раньше
    CalculatingChunks,   // Этот статус тоже может быть установлен до создания полной задачи
    StartingChunk(u32),
    PlacingSpotOrder(u32),
    PlacingFuturesOrder(u32),
    RunningChunk(u32),
    WaitingImbalance {
        chunk_index: u32,
        leading_leg: Leg,
    },
    CancellingOrder {
        chunk_index: u32,
        leg_to_cancel: Leg,
        order_id_to_cancel: String,
        reason: String, 
    },
    WaitingCancelConfirmation { 
        chunk_index: u32,
        cancelled_leg: Leg,
        cancelled_order_id: String,
    },
    Reconciling,
    Completed,
    Cancelling, 
    Cancelled,
    Failed(String),
}


#[derive(Debug, Clone, Default)]
pub struct MarketUpdate {
    pub best_bid_price: Option<Decimal>,
    pub best_bid_quantity: Option<Decimal>,
    pub best_ask_price: Option<Decimal>,
    pub best_ask_quantity: Option<Decimal>,
    pub last_update_time_ms: Option<i64>, 
}

#[derive(Debug, Clone)]
pub struct HedgerWsState {
    pub operation_id: i64,
    pub operation_type: OperationType,
    pub symbol_spot: String,
    pub symbol_futures: String,
    
    pub spot_tick_size: Decimal,
    pub spot_quantity_step: Decimal,
    pub futures_tick_size: Decimal,
    pub futures_quantity_step: Decimal,
    pub min_spot_quantity: Decimal,
    pub min_futures_quantity: Decimal,
    pub min_spot_notional: Option<Decimal>,
    pub min_futures_notional: Option<Decimal>,
    
    pub total_chunks: u32,
    pub chunk_base_quantity_spot: Decimal,
    pub chunk_base_quantity_futures: Decimal,
    
    pub current_chunk_index: u32, // Начинается с 1 для первого чанка
    pub cumulative_spot_filled_quantity: Decimal,
    pub cumulative_spot_filled_value: Decimal,
    pub cumulative_futures_filled_quantity: Decimal,
    pub cumulative_futures_filled_value: Decimal,
    
    pub target_total_futures_value: Decimal, // Динамически обновляется на основе стоимости спота (для хеджа)
    
    pub initial_target_spot_value: Decimal, // Рассчитанная начальная целевая стоимость спота
    pub initial_target_futures_qty: Decimal, // Рассчитанное начальное целевое количество фьючерсов
    // ИЗМЕНЕНО: Добавлено поле для хранения начальной суммы пользователя
    pub initial_user_sum: Decimal,          // Сумма, введенная пользователем, для расчета плеча

    pub active_spot_order: Option<ChunkOrderState>,
    pub active_futures_order: Option<ChunkOrderState>,
    
    pub spot_market_data: MarketUpdate,
    pub futures_market_data: MarketUpdate,
    
    pub status: HedgerWsStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationType {
    Hedge,
    Unhedge,
}

impl HedgerWsState {
     pub fn new_hedge(
        operation_id: i64,
        symbol_spot: String,
        symbol_futures: String,
        initial_target_spot_value: Decimal, 
        initial_target_futures_qty: Decimal,
        // ИЗМЕНЕНО: Добавлен параметр initial_user_sum
        initial_user_sum: Decimal,
    ) -> Self {
        let zero = Decimal::ZERO;
        Self {
            operation_id,
            operation_type: OperationType::Hedge,
            symbol_spot,
            symbol_futures,
            spot_tick_size: zero, futures_tick_size: zero, 
            spot_quantity_step: zero, futures_quantity_step: zero, 
            min_spot_quantity: zero, min_futures_quantity: zero, 
            min_spot_notional: None, min_futures_notional: None, 
            total_chunks: 0, // Будет установлено позже
            chunk_base_quantity_spot: Decimal::ZERO,
            chunk_base_quantity_futures: Decimal::ZERO,
            current_chunk_index: 1, // Начинаем с первого чанка
            cumulative_spot_filled_quantity: Decimal::ZERO,
            cumulative_spot_filled_value: Decimal::ZERO,
            cumulative_futures_filled_quantity: Decimal::ZERO,
            cumulative_futures_filled_value: Decimal::ZERO,
            target_total_futures_value: initial_target_spot_value, // Начальное значение
            initial_target_spot_value,
            initial_target_futures_qty,
            // ИЗМЕНЕНО: Инициализация нового поля
            initial_user_sum, 
            active_spot_order: None,
            active_futures_order: None,
            spot_market_data: MarketUpdate::default(),
            futures_market_data: MarketUpdate::default(),
            status: HedgerWsStatus::Initializing, // Начальный статус
        }
    }

      pub fn new_unhedge(
         operation_id: i64,
         symbol_spot: String,
         symbol_futures: String,
         target_spot_sell_qty: Decimal, // Это уже кол-во, а не стоимость
         target_futures_buy_qty: Decimal,
         // Для unhedge initial_user_sum может быть не так актуален или равен стоимости продаваемого спота
         initial_user_sum_equivalent: Decimal, 
     ) -> Self {
         let zero = Decimal::ZERO;
         Self {
             operation_id,
             operation_type: OperationType::Unhedge,
             symbol_spot,
             symbol_futures,
             spot_tick_size: zero, futures_tick_size: zero, 
             spot_quantity_step: zero, futures_quantity_step: zero, 
             min_spot_quantity: zero, min_futures_quantity: zero, 
             min_spot_notional: None, min_futures_notional: None, 
             total_chunks: 0,
             chunk_base_quantity_spot: Decimal::ZERO,
             chunk_base_quantity_futures: Decimal::ZERO,
             current_chunk_index: 1,
             cumulative_spot_filled_quantity: Decimal::ZERO,
             cumulative_spot_filled_value: Decimal::ZERO, // Будет увеличиваться при продаже спота
             cumulative_futures_filled_quantity: Decimal::ZERO,
             cumulative_futures_filled_value: Decimal::ZERO, // Будет увеличиваться при покупке фьюча
             target_total_futures_value: Decimal::ZERO, // Не используется в unhedge так же, как в hedge
             initial_target_spot_value: target_spot_sell_qty, // Здесь это фактически target_qty
             initial_target_futures_qty: target_futures_buy_qty,
             initial_user_sum: initial_user_sum_equivalent, // Эквивалент суммы для консистентности
             active_spot_order: None,
             active_futures_order: None,
             spot_market_data: MarketUpdate::default(),
             futures_market_data: MarketUpdate::default(),
             status: HedgerWsStatus::Initializing,
         }
     }
}