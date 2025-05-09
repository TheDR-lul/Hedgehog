// src/webservice_hedge/hedge_logic/reconciliation.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{info, error, warn}; // Добавили warn обратно, т.к. используется
use tokio::time::sleep;
use std::time::Duration;

use crate::exchange::types::OrderSide;
use crate::webservice_hedge::hedge_task::HedgerWsHedgeTask;
use crate::webservice_hedge::state::{HedgerWsStatus, Leg};
use crate::webservice_hedge::hedge_logic::helpers::{get_current_price, round_down_step, update_final_db_status};

pub async fn reconcile(task: &mut HedgerWsHedgeTask) -> Result<()> {
    info!(operation_id = task.operation_id, "Starting final hedge reconciliation...");
    task.state.status = HedgerWsStatus::Reconciling;

    let final_spot_qty = task.state.cumulative_spot_filled_quantity;
    let final_futures_qty = task.state.cumulative_futures_filled_quantity;
    let final_spot_value = task.state.cumulative_spot_filled_value;
    let target_futures_value = final_spot_value;

    let current_futures_price = get_current_price(task, Leg::Futures)
        .ok_or_else(|| anyhow!("Cannot get current futures price for reconciliation"))?;
    let actual_futures_value = final_futures_qty * current_futures_price;
    let value_imbalance = target_futures_value - actual_futures_value.abs();

    info!(operation_id = task.operation_id,
          %final_spot_value, target_futures_value = %target_futures_value,
          %final_futures_qty, %current_futures_price, actual_futures_value = %actual_futures_value.abs(),
          %value_imbalance, "Calculated final value imbalance for reconciliation.");

    let tolerance_value = task.state.initial_target_spot_value * dec!(0.001);

    if value_imbalance.abs() > tolerance_value {
        let adjustment_qty_decimal = value_imbalance / current_futures_price;
        let adjustment_qty_rounded = round_down_step(task, adjustment_qty_decimal.abs(), task.state.futures_quantity_step)?;
        let adjustment_qty = adjustment_qty_rounded.to_f64().unwrap_or(0.0);

        if adjustment_qty_rounded >= task.state.min_futures_quantity {
            let side = if value_imbalance > Decimal::ZERO { OrderSide::Sell } else { OrderSide::Buy };
            info!(operation_id=task.operation_id, ?side, adjustment_qty, %current_futures_price, "Placing FUTURES market order for reconciliation...");

            match task.exchange_rest.place_futures_market_order(&task.state.symbol_futures, side, adjustment_qty).await {
                Ok(order) => {
                    info!(operation_id=task.operation_id, order_id=%order.id, ?side, adjustment_qty, "Reconciliation market order placed successfully.");
                    sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    error!(operation_id=task.operation_id, %e, ?side, adjustment_qty, "Failed to place FUTURES market order for reconciliation!");
                }
            }
        } else {
             // --- ИСПРАВЛЕНО: Заполняем warn! ---
             warn!(operation_id=task.operation_id, %adjustment_qty_decimal, %adjustment_qty_rounded, min_qty=%task.state.min_futures_quantity,
                   "Required futures adjustment quantity is below minimum. Skipping reconciliation order.");
        }
    } else {
        info!(operation_id = task.operation_id, "Value imbalance within tolerance. No reconciliation needed.");
    }

    task.state.status = HedgerWsStatus::Completed;
    update_final_db_status(task).await;
    info!(operation_id = task.operation_id, "Hedge reconciliation complete. Final Status: Completed.");
    Ok(())
}