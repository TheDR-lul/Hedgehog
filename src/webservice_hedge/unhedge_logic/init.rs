// src/webservice_hedge/unhedge_logic/init.rs

use anyhow::{anyhow, Context, Result};
// ИЗМЕНЕНО: Убран неиспользуемый import rust_decimal::prelude
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use std::str::FromStr;

use crate::config::Config;
use crate::exchange::Exchange;
use crate::exchange::types::WebSocketMessage;
use crate::hedger::HedgeProgressCallback;
use crate::storage::{self, HedgeOperation};
use crate::webservice_hedge::unhedge_task::HedgerWsUnhedgeTask;
use crate::webservice_hedge::state::{HedgerWsState, HedgerWsStatus};
use crate::webservice_hedge::common::calculate_auto_chunk_parameters;
// Используем хелперы из hedge_logic для общих функций


pub(crate) async fn initialize_task(
    original_operation: HedgeOperation,
    config: Arc<Config>,
    database: Arc<storage::Db>,
    exchange_rest: Arc<dyn Exchange>,
    progress_callback: HedgeProgressCallback,
    // ИЗМЕНЕНО: Теперь для unhedge_task мы также можем ожидать два приемника,
    // но в текущей реализации unhedge_task принимает один. Если unhedge тоже будет разделять потоки,
    // эту функцию и HedgerWsUnhedgeTask::new нужно будет адаптировать аналогично hedge_task.
    // Пока что оставим один ws_receiver для unhedge, так как его рефакторинг не был основной задачей.
    // Если unhedge НЕ будет использовать WebSocket для стакана, то этот ws_receiver может быть только для ордеров.
    ws_receiver: mpsc::Receiver<Result<WebSocketMessage>>,
) -> Result<HedgerWsUnhedgeTask> {
    let operation_id = original_operation.id;
    info!(operation_id, "Initializing HedgerWsUnhedgeTask...");

    let base_symbol = original_operation.base_symbol.to_uppercase();
    let spot_symbol_name = format!("{}{}", base_symbol, config.quote_currency);
    let futures_symbol_name = format!("{}{}", base_symbol, config.quote_currency);

    let target_spot_sell_quantity = Decimal::try_from(original_operation.spot_filled_qty)
        .context("Failed to convert original spot_filled_qty to Decimal")?;
    let target_futures_buy_quantity = Decimal::try_from(original_operation.target_futures_qty) // Это уже корректное значение из БД
        .context("Failed to convert original target_futures_qty to Decimal")?;

    if target_spot_sell_quantity <= Decimal::ZERO || target_futures_buy_quantity.abs() <= Decimal::ZERO {
         return Err(anyhow!("Original operation quantities are zero or negative. Cannot unhedge. Spot: {}, Futures: {}", target_spot_sell_quantity, target_futures_buy_quantity.abs()));
    }

    debug!(operation_id, %spot_symbol_name, %futures_symbol_name, %target_spot_sell_quantity, abs_target_futures_buy_quantity = %target_futures_buy_quantity.abs(), "Fetching instrument info and balance for unhedge...");

    let (
        spot_info_res,
        linear_info_res,
        spot_balance_res,
        spot_price_res,
        futures_price_res
    ) = tokio::join!(
        exchange_rest.get_spot_instrument_info(&base_symbol),
        exchange_rest.get_linear_instrument_info(&base_symbol),
        exchange_rest.get_balance(&base_symbol),
        exchange_rest.get_spot_price(&base_symbol),
        exchange_rest.get_market_price(&futures_symbol_name, false) 
    );

    let spot_info = spot_info_res.context("Failed to get SPOT instrument info for unhedge")?;
    let linear_info = linear_info_res.context("Failed to get LINEAR instrument info for unhedge")?;
    let spot_balance = spot_balance_res.context("Failed to get SPOT balance for unhedge")?;
    let current_spot_price_f64 = spot_price_res.context("Failed to get current SPOT price for unhedge")?;
    let current_futures_price_f64 = futures_price_res.context("Failed to get current FUTURES price for unhedge")?;

    if current_spot_price_f64 <= 0.0 || current_futures_price_f64 <= 0.0 {
         return Err(anyhow!("Spot or Futures price is non-positive for unhedge"));
    }
    let current_spot_price = Decimal::try_from(current_spot_price_f64)?;
    let current_futures_price = Decimal::try_from(current_futures_price_f64)?;

    let available_spot_balance = Decimal::try_from(spot_balance.free)?;
    let mut actual_spot_sell_target_quantity = target_spot_sell_quantity;

    if available_spot_balance < target_spot_sell_quantity {
        warn!(operation_id,
              target = %target_spot_sell_quantity,
              available = %available_spot_balance,
              "Available spot balance is less than target sell quantity for unhedge. Adjusting target.");
        actual_spot_sell_target_quantity = available_spot_balance;
    }
    let min_spot_quantity = Decimal::from_str(&spot_info.lot_size_filter.min_order_qty)?;
    if actual_spot_sell_target_quantity < min_spot_quantity {
         return Err(anyhow!("Available spot balance ({}) is less than minimum order quantity ({}) for {}. Cannot unhedge.", actual_spot_sell_target_quantity, min_spot_quantity, spot_symbol_name));
    }

    // Оценка начальной стоимости продаваемого спота (эквивалент initial_user_sum для конструктора HedgerWsState)
    let initial_spot_value_estimate_for_state = actual_spot_sell_target_quantity * current_spot_price;


    debug!(operation_id, status=?HedgerWsStatus::CalculatingChunks, "for unhedge");
    let target_chunk_count = config.ws_auto_chunk_target_count;

    let min_futures_quantity = Decimal::from_str(&linear_info.lot_size_filter.min_order_qty)?;
    let spot_quantity_step = crate::webservice_hedge::hedge_logic::helpers::get_step_decimal(spot_info.lot_size_filter.base_precision.as_deref())?;
    let futures_quantity_step = crate::webservice_hedge::hedge_logic::helpers::get_step_decimal(linear_info.lot_size_filter.qty_step.as_deref())?;
    let min_spot_notional = spot_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());
    let min_futures_notional = linear_info.lot_size_filter.min_notional_value.as_deref().and_then(|s| Decimal::from_str(s).ok());

    let (final_chunk_count, chunk_spot_quantity, chunk_futures_quantity) =
        calculate_auto_chunk_parameters(
            initial_spot_value_estimate_for_state, // Стоимость спота, которую будем продавать
            target_futures_buy_quantity.abs(),    // Количество фьючерсов, которое будем покупать
            current_spot_price,
            current_futures_price,
            target_chunk_count,
            min_spot_quantity,
            min_futures_quantity,
            spot_quantity_step,
            futures_quantity_step,
            min_spot_notional,
            min_futures_notional,
        )?;
    info!(operation_id, final_chunk_count, %chunk_spot_quantity, %chunk_futures_quantity, "Unhedge chunk parameters calculated");

    let spot_tick_size = Decimal::from_str(&spot_info.price_filter.tick_size)?;
    let futures_tick_size = Decimal::from_str(&linear_info.price_filter.tick_size)?;

    // ИЗМЕНЕНО: Передаем initial_spot_value_estimate_for_state как шестой аргумент
    let mut state = HedgerWsState::new_unhedge(
        operation_id,
        spot_symbol_name.clone(),
        futures_symbol_name.clone(),
        actual_spot_sell_target_quantity, // Целевое количество спота к продаже
        target_futures_buy_quantity,      // Целевое количество фьючерсов к покупке (может быть отрицательным, если из БД так пришло)
        initial_spot_value_estimate_for_state // <--- Добавлен этот аргумент
    );
    
    state.spot_tick_size = spot_tick_size;
    state.spot_quantity_step = spot_quantity_step;
    state.min_spot_quantity = min_spot_quantity;
    state.min_spot_notional = min_spot_notional;
    state.futures_tick_size = futures_tick_size;
    state.futures_quantity_step = futures_quantity_step;
    state.min_futures_quantity = min_futures_quantity;
    state.min_futures_notional = min_futures_notional;
    state.total_chunks = final_chunk_count;
    state.chunk_base_quantity_spot = chunk_spot_quantity;
    state.chunk_base_quantity_futures = chunk_futures_quantity;
    // initial_target_spot_value в state для unhedge будет равно actual_spot_sell_target_quantity (это количество, а не стоимость)
    // target_total_futures_value для unhedge не используется так же, как для hedge
    state.status = HedgerWsStatus::StartingChunk(1);

    info!(operation_id, "HedgerWsUnhedgeTask initialized successfully. Ready to run.");

    Ok(HedgerWsUnhedgeTask {
        operation_id,
        config,
        database,
        state,
        ws_receiver, // Для unhedge пока оставляем один ws_receiver
        exchange_rest,
        progress_callback,
        original_spot_target: target_spot_sell_quantity, // Изначальная цель из БД
        original_futures_target: target_futures_buy_quantity, // Изначальная цель из БД
        actual_spot_sell_target: actual_spot_sell_target_quantity, // Скорректированная по балансу цель
    })
}