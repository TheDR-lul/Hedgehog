// src/webservice_hedge/unhedge_logic/chunk_execution.rs

use anyhow::{anyhow, Result, Context};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{debug, error, info, warn};

use crate::exchange::types::OrderSide;
use crate::webservice_hedge::unhedge_task::HedgerWsUnhedgeTask;
use crate::webservice_hedge::state::{ChunkOrderState, HedgerWsStatus, Leg};
use crate::webservice_hedge::unhedge_logic::helpers::{calculate_limit_price_for_leg, round_down_step, get_current_price, send_progress_update, update_final_db_status};

pub(crate) async fn start_next_chunk(task: &mut HedgerWsUnhedgeTask) -> Result<()> {
    let chunk_index = match task.state.status {
         HedgerWsStatus::StartingChunk(index) => index,
         HedgerWsStatus::RunningChunk(index) => {
             warn!(operation_id = task.operation_id, chunk_index=index, "start_next_chunk (unhedge) called while chunk already running.");
             return Ok(());
        }
         _ => return Err(anyhow!("start_next_chunk (unhedge) called in unexpected state: {:?}", task.state.status)),
    };
    info!(operation_id = task.operation_id, chunk_index, "Starting unhedge chunk placement...");
    task.state.active_spot_order = None;
    task.state.active_futures_order = None;

    let spot_quantity_chunk: Decimal;
    let futures_quantity_chunk: Decimal;
    let is_last_chunk = chunk_index == task.state.total_chunks;

    if is_last_chunk {
        spot_quantity_chunk = (task.actual_spot_sell_target - task.state.cumulative_spot_filled_quantity).max(Decimal::ZERO);
        futures_quantity_chunk = (task.original_futures_target.abs() - task.state.cumulative_futures_filled_quantity).max(Decimal::ZERO);
        info!(operation_id=task.operation_id, chunk_index, "Calculating quantities for last unhedge chunk.");
    } else {
        spot_quantity_chunk = task.state.chunk_base_quantity_spot;
        futures_quantity_chunk = task.state.chunk_base_quantity_futures;
    }
    debug!(operation_id=task.operation_id, chunk_index, %spot_quantity_chunk, %futures_quantity_chunk, "Calculated unhedge chunk quantities.");

    let spot_quantity_rounded = round_down_step(task, spot_quantity_chunk, task.state.spot_quantity_step)?;
    let futures_quantity_rounded = round_down_step(task, futures_quantity_chunk, task.state.futures_quantity_step)?;

    let tolerance = dec!(1e-12);
    let place_spot = spot_quantity_rounded >= task.state.min_spot_quantity || spot_quantity_rounded < tolerance;
    let place_futures = futures_quantity_rounded >= task.state.min_futures_quantity || futures_quantity_rounded < tolerance;

    let current_spot_price_estimate = get_current_price(task, Leg::Spot).unwrap_or(dec!(1.0));
    let spot_notional_ok = task.state.min_spot_notional.map_or(true, |min_val| {
        (spot_quantity_rounded * current_spot_price_estimate) >= min_val || spot_quantity_rounded < tolerance
    });
    let futures_price_estimate = get_current_price(task, Leg::Futures).unwrap_or(current_spot_price_estimate);
    let futures_notional_ok = task.state.min_futures_notional.map_or(true, |min_val| {
        (futures_quantity_rounded * futures_price_estimate) >= min_val || futures_quantity_rounded < tolerance
    });

    if (!place_spot || !spot_notional_ok) && (!place_futures || !futures_notional_ok) {
         warn!(operation_id=task.operation_id, chunk_index, %spot_quantity_rounded, %futures_quantity_rounded,
               place_spot, spot_notional_ok, place_futures, futures_notional_ok,
               "Both legs quantities for unhedge chunk are below minimums or notionals. Skipping chunk.");
          if is_last_chunk {
             task.state.status = HedgerWsStatus::Reconciling;
          } else {
              let next_chunk_index = chunk_index + 1;
              task.state.current_chunk_index = next_chunk_index;
              task.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
          }
          return Ok(());
    }
     if !place_spot || !spot_notional_ok { warn!(operation_id=task.operation_id, chunk_index, %spot_quantity_rounded, "Spot leg quantity too small or below notional. Skipping spot placement."); }
     if !place_futures || !futures_notional_ok { warn!(operation_id=task.operation_id, chunk_index, %futures_quantity_rounded, "Futures leg quantity too small or below notional. Skipping futures placement."); }

    let spot_limit_price = if place_spot && spot_notional_ok { Some(calculate_limit_price_for_leg(task, Leg::Spot)?) } else { None };
    let futures_limit_price = if place_futures && futures_notional_ok { Some(calculate_limit_price_for_leg(task, Leg::Futures)?) } else { None };

    let mut placed_spot_order: Option<ChunkOrderState> = None;
    let mut placed_futures_order: Option<ChunkOrderState> = None;
    let mut spot_place_error: Option<anyhow::Error> = None;
    let mut futures_place_error: Option<anyhow::Error> = None;

    if let (true, true, Some(limit_price)) = (place_futures, futures_notional_ok, futures_limit_price) {
       task.state.status = HedgerWsStatus::PlacingFuturesOrder(chunk_index);
        let qty_f64 = futures_quantity_rounded.round_dp(task.state.futures_quantity_step.scale()).to_f64().unwrap_or(0.0);
        let price_f64 = limit_price.round_dp(task.state.futures_tick_size.scale()).to_f64().unwrap_or(0.0);
        if qty_f64 <= 0.0 || price_f64 <= 0.0 {
             futures_place_error = Some(anyhow!("Invalid FUTURES order parameters (qty={}, price={})", qty_f64, price_f64));
        } else {
            info!(operation_id=task.operation_id, chunk_index, %qty_f64, %price_f64, "Placing FUTURES BUY order via REST");
            let futures_order_result = task.exchange_rest.place_futures_limit_order(
                 &task.state.symbol_futures, // Для фьючерсов символ уже полный
                 OrderSide::Buy,
                 qty_f64,
                 price_f64
            ).await;
             match futures_order_result {
                 Ok(o) => {
                      info!(operation_id=task.operation_id, chunk_index, order_id=%o.id, "FUTURES BUY order placed.");
                      placed_futures_order = Some(ChunkOrderState::new(o.id, task.state.symbol_futures.clone(), OrderSide::Buy, limit_price, futures_quantity_rounded));
                 },
                 Err(e) => {
                      error!(operation_id=task.operation_id, chunk_index, error=%e, "Failed to place FUTURES BUY order!");
                      futures_place_error = Some(e.context("Failed to place FUTURES BUY order"));
                 }
            };
        }
    }

   if futures_place_error.is_none() {
       if let (true, true, Some(limit_price)) = (place_spot, spot_notional_ok, spot_limit_price) {
          task.state.status = HedgerWsStatus::PlacingSpotOrder(chunk_index);
          let qty_f64 = spot_quantity_rounded.round_dp(task.state.spot_quantity_step.scale()).to_f64().unwrap_or(0.0);
          let price_f64 = limit_price.round_dp(task.state.spot_tick_size.scale()).to_f64().unwrap_or(0.0);
          if qty_f64 <= 0.0 || price_f64 <= 0.0 {
              spot_place_error = Some(anyhow!("Invalid SPOT order parameters (qty={}, price={})", qty_f64, price_f64));
              let mut fut_cancelled_in_rollback = false;
              if let Some(ref fut_order) = placed_futures_order {
                  warn!(operation_id=task.operation_id, chunk_index, order_id=%fut_order.order_id, "Attempting to cancel FUTURES order due to SPOT invalid params.");
                  if let Err(cancel_err) = task.exchange_rest.cancel_futures_order(&task.state.symbol_futures, &fut_order.order_id).await {
                      error!(operation_id=task.operation_id, chunk_index, order_id=%fut_order.order_id, %cancel_err, "Failed to cancel FUTURES order after SPOT failure!");
                      spot_place_error = spot_place_error.map(|err| err.context(format!("Also failed to cancel futures: {}", cancel_err)));
                  } else {
                      fut_cancelled_in_rollback = true;
                      info!(operation_id=task.operation_id, chunk_index, order_id=%fut_order.order_id, "FUTURES order cancelled successfully after SPOT invalid params.");
                  }
              }
              if fut_cancelled_in_rollback { placed_futures_order = None; }
          } else {
              info!(operation_id=task.operation_id, chunk_index, %qty_f64, %price_f64, "Placing SPOT SELL order via REST");
              // *** ИСПРАВЛЕНИЕ НАЧАЛО ***
              let base_spot_symbol_str = task.state.symbol_spot.trim_end_matches(&task.config.quote_currency.to_uppercase());
              if base_spot_symbol_str.is_empty() || base_spot_symbol_str == task.state.symbol_spot {
                  spot_place_error = Some(anyhow!("Could not derive base symbol from spot_symbol for unhedge: {}", task.state.symbol_spot));
              } else {
                  let spot_order_result = task.exchange_rest.place_limit_order(
                       base_spot_symbol_str, // Используем базовый символ
                       OrderSide::Sell,
                       qty_f64,
                       price_f64
                  ).await;
              // *** ИСПРАВЛЕНИЕ КОНЕЦ ***
                   match spot_order_result {
                        Ok(o) => {
                             info!(operation_id=task.operation_id, chunk_index, order_id=%o.id, "SPOT SELL order placed.");
                             placed_spot_order = Some(ChunkOrderState::new(o.id, task.state.symbol_spot.clone(), OrderSide::Sell, limit_price, spot_quantity_rounded));
                        },
                        Err(e) => {
                             error!(operation_id=task.operation_id, chunk_index, error=%e, "Failed to place SPOT SELL order!");
                             spot_place_error = Some(e.context("Failed to place SPOT SELL order"));
                            let mut fut_cancelled_in_rollback = false;
                             if let Some(ref fut_order) = placed_futures_order {
                                  warn!(operation_id=task.operation_id, chunk_index, order_id=%fut_order.order_id, "Attempting to cancel FUTURES order due to SPOT placement failure.");
                                  if let Err(cancel_err) = task.exchange_rest.cancel_futures_order(&task.state.symbol_futures, &fut_order.order_id).await {
                                       error!(operation_id=task.operation_id, chunk_index, order_id=%fut_order.order_id, %cancel_err, "Failed to cancel FUTURES order after SPOT failure!");
                                       spot_place_error = spot_place_error.map(|err| err.context(format!("Also failed to cancel futures: {}", cancel_err)));
                                  } else {
                                       fut_cancelled_in_rollback = true;
                                       info!(operation_id=task.operation_id, chunk_index, order_id=%fut_order.order_id, "FUTURES order cancelled successfully after SPOT failure.");
                                  }
                             }
                             if fut_cancelled_in_rollback { placed_futures_order = None; }
                        }
                   };
              }
          }
      }
   } else {
        warn!(operation_id=task.operation_id, chunk_index, "Skipping SPOT placement because FUTURES placement failed.");
   }

   if let Some(error) = spot_place_error.or(futures_place_error) {
       task.state.status = HedgerWsStatus::Failed(format!("Unhedge Chunk {} placement error: {}", chunk_index, error));
       update_final_db_status(task).await;
       return Err(error);
   }

   task.state.active_spot_order = placed_spot_order;
   task.state.active_futures_order = placed_futures_order;
   if task.state.active_spot_order.is_some() || task.state.active_futures_order.is_some() {
        task.state.current_chunk_index = chunk_index + 1;
        task.state.status = HedgerWsStatus::RunningChunk(chunk_index);
        info!(operation_id = task.operation_id, chunk_index, "Unhedge chunk placement finished. Status: RunningChunk");
        send_progress_update(task).await?;
   } else {
        warn!(operation_id=task.operation_id, chunk_index, "No orders were placed for this unhedge chunk.");
         if is_last_chunk {
            task.state.status = HedgerWsStatus::Reconciling;
         } else {
             let next_chunk_index = chunk_index + 1;
             task.state.current_chunk_index = next_chunk_index;
             task.state.status = HedgerWsStatus::StartingChunk(next_chunk_index);
         }
   }

   Ok(())
}