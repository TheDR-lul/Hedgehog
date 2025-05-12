// src/webservice_hedge/unhedge_logic/reconciliation.rs

use anyhow::{Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{info, error, warn};
use tokio::time::sleep;
use std::time::Duration;

use crate::exchange::types::OrderSide;
use crate::storage;
use crate::webservice_hedge::unhedge_task::HedgerWsUnhedgeTask;
use crate::webservice_hedge::state::HedgerWsStatus;
use crate::webservice_hedge::unhedge_logic::helpers::{round_down_step, update_final_db_status};

pub async fn reconcile(task: &mut HedgerWsUnhedgeTask) -> Result<()> {
    info!(operation_id = task.operation_id, "Starting final unhedge reconciliation...");
    task.state.status = HedgerWsStatus::Reconciling;

    let target_spot_qty = task.actual_spot_sell_target;
    let target_futures_qty = task.original_futures_target.abs();

    let filled_spot_qty = task.state.cumulative_spot_filled_quantity;
    let filled_futures_qty = task.state.cumulative_futures_filled_quantity;

    let spot_imbalance = target_spot_qty - filled_spot_qty;
    let futures_imbalance = target_futures_qty - filled_futures_qty;

    info!(operation_id = task.operation_id,
          target_spot=%target_spot_qty, filled_spot=%filled_spot_qty, spot_imbalance=%spot_imbalance,
          target_futures=%target_futures_qty, filled_futures=%filled_futures_qty, futures_imbalance=%futures_imbalance,
          "Calculated final quantity imbalance for unhedge reconciliation.");

    let spot_adjustment_qty_rounded = round_down_step(task, spot_imbalance.abs(), task.state.spot_quantity_step)?;
    if spot_adjustment_qty_rounded >= task.state.min_spot_quantity {
        let side = if spot_imbalance > Decimal::ZERO { OrderSide::Sell } else { OrderSide::Buy }; // Если не допродали - продаем, если перепродали - откупаем (маловероятно)
        let adjustment_qty = spot_adjustment_qty_rounded.to_f64().unwrap_or(0.0);
        info!(operation_id=task.operation_id, ?side, adjustment_qty, "Placing SPOT market order for unhedge reconciliation...");
        // *** ИСПРАВЛЕНИЕ НАЧАЛО ***
        let base_spot_symbol_str = task.state.symbol_spot.trim_end_matches(&task.config.quote_currency.to_uppercase());
        if base_spot_symbol_str.is_empty() || base_spot_symbol_str == task.state.symbol_spot {
            error!(operation_id=task.operation_id, "Could not derive base symbol from spot_symbol for unhedge reconciliation: {}", task.state.symbol_spot);
        } else {
            match task.exchange_rest.place_spot_market_order(base_spot_symbol_str, side, adjustment_qty).await {
        // *** ИСПРАВЛЕНИЕ КОНЕЦ ***
                 Ok(order) => { info!(operation_id=task.operation_id, order_id=%order.id, ?side, adjustment_qty, "Spot reconciliation market order placed successfully."); }
                 Err(e) => { error!(operation_id=task.operation_id, %e, ?side, adjustment_qty, "Failed to place SPOT market order for reconciliation!"); }
            }
        }
    } else if spot_imbalance.abs() > dec!(1e-12) {
         warn!(operation_id = task.operation_id, %spot_imbalance, min_qty=%task.state.min_spot_quantity, "Required spot adjustment quantity is below minimum. Skipping.");
    }

     let futures_adjustment_qty_rounded = round_down_step(task, futures_imbalance.abs(), task.state.futures_quantity_step)?;
     if futures_adjustment_qty_rounded >= task.state.min_futures_quantity {
         let side = if futures_imbalance > Decimal::ZERO { OrderSide::Buy } else { OrderSide::Sell }; // Если не докупили - докупаем, если перекупили - продаем
         let adjustment_qty = futures_adjustment_qty_rounded.to_f64().unwrap_or(0.0);
         info!(operation_id=task.operation_id, ?side, adjustment_qty, "Placing FUTURES market order for unhedge reconciliation...");
         match task.exchange_rest.place_futures_market_order(&task.state.symbol_futures, side, adjustment_qty).await {
              Ok(order) => { info!(operation_id=task.operation_id, order_id=%order.id, ?side, adjustment_qty, "Futures reconciliation market order placed successfully."); }
              Err(e) => { error!(operation_id=task.operation_id, %e, ?side, adjustment_qty, "Failed to place FUTURES market order for reconciliation!"); }
         }
     } else if futures_imbalance.abs() > dec!(1e-12) {
          warn!(operation_id = task.operation_id, %futures_imbalance, min_qty=%task.state.min_futures_quantity, "Required futures adjustment quantity is below minimum. Skipping.");
     }

    sleep(Duration::from_secs(2)).await;

    match storage::mark_hedge_as_unhedged(&task.database, task.operation_id).await {
         Ok(_) => info!(operation_id=task.operation_id, "Marked original hedge operation as unhedged."),
         Err(e) => error!(operation_id=task.operation_id, %e, "Failed to mark original hedge operation as unhedged in DB!"),
    }

    task.state.status = HedgerWsStatus::Completed;
    update_final_db_status(task).await;
    info!(operation_id = task.operation_id, "Unhedge reconciliation complete. Final Status: Completed.");
    Ok(())
}