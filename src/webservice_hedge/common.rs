// src/webservice_hedge/common.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{debug, warn, error};

const MAX_PER_CHUNK_VALUE_IMBALANCE_RATIO_FOR_SETUP: Decimal = dec!(0.15); // 15%

pub fn calculate_auto_chunk_parameters(
    overall_target_spot_btc: Decimal,    // ПРИНИМАЕМ общую цель по споту в BTC
    overall_target_futures_btc: Decimal, // ПРИНИМАЕМ общую цель по фьючерсу в BTC
    current_spot_price: Decimal,
    current_futures_price_estimate: Decimal,
    target_chunk_count: u32,
    min_spot_quantity: Decimal,
    min_futures_quantity: Decimal,
    spot_quantity_step: Decimal,
    futures_quantity_step: Decimal,
    min_spot_notional_value: Option<Decimal>,
    min_futures_notional_value: Option<Decimal>,
) -> Result<(u32, Decimal, Decimal)> {
    if target_chunk_count == 0 {
        return Err(anyhow!("Target chunk count cannot be zero"));
    }
    if current_spot_price <= Decimal::ZERO || current_futures_price_estimate <= Decimal::ZERO {
        return Err(anyhow!("Current spot and futures prices must be positive for chunk calculation"));
    }
    if overall_target_spot_btc <= Decimal::ZERO {
        return Err(anyhow!("Overall target spot BTC quantity must be positive"));
    }
    if overall_target_futures_btc <= Decimal::ZERO {
        error!("Overall target futures BTC quantity ({}) is zero or negative, which is invalid for a typical hedge setup.", overall_target_futures_btc);
        return Err(anyhow!("Overall target futures BTC quantity must be positive for hedge."));
    }

    debug!(
        "calculate_auto_chunk_parameters input: overall_target_spot_btc={}, overall_target_futures_btc={}, current_spot_price={}, current_futures_price_estimate={}",
        overall_target_spot_btc, overall_target_futures_btc, current_spot_price, current_futures_price_estimate
    );

    let spot_to_fut_btc_ratio = if overall_target_futures_btc != Decimal::ZERO {
        overall_target_spot_btc / overall_target_futures_btc
    } else {
        warn!("overall_target_futures_btc is zero in ratio calculation, setting ratio to 1.0");
        Decimal::ONE
    };

    debug!(
        "Calculated spot_to_fut_btc_ratio: {:.6} (overall_target_spot_btc: {:.8}, overall_target_futures_btc: {:.8})",
        spot_to_fut_btc_ratio, overall_target_spot_btc, overall_target_futures_btc
    );
    
    let mut number_of_chunks = target_chunk_count.max(1);

    loop {
        if number_of_chunks == 0 {
            error!("Failed to find suitable chunk size (number_of_chunks became 0). Check min order sizes for spot/futures relative to total target quantities.");
            return Err(anyhow!("Failed to find suitable chunk size (number_of_chunks became 0). Review min order sizes and total target quantities."));
        }

        let num_chunks_decimal = Decimal::from(number_of_chunks);
        let tolerance = dec!(1e-12);

        let chunk_futures_quantity_raw = overall_target_futures_btc / num_chunks_decimal;
        let chunk_futures_quantity = round_up_step(chunk_futures_quantity_raw, futures_quantity_step)
            .unwrap_or(chunk_futures_quantity_raw);

        if chunk_futures_quantity < min_futures_quantity && chunk_futures_quantity.abs() > tolerance {
            if number_of_chunks == 1 {
                return Err(anyhow!("Futures chunk qty {} for 1 chunk < min_futures_qty {}. Target futures BTC: {}", chunk_futures_quantity.round_dp(8), min_futures_quantity, overall_target_futures_btc.round_dp(8)));
            }
            debug!("Futures chunk qty {} for {} chunks < min_futures_qty {}. Reducing chunk count.", chunk_futures_quantity.round_dp(8), number_of_chunks, min_futures_quantity);
            number_of_chunks -= 1;
            continue;
        }
        if chunk_futures_quantity < tolerance && min_futures_quantity >= tolerance {
             if number_of_chunks == 1 { return Err(anyhow!("Futures chunk qty is zero for 1 chunk, but min_futures_qty {} is non-zero.", min_futures_quantity)); }
            debug!("Futures chunk qty is zero for {} chunks with non-zero min_futures_qty. Reducing chunk count.", number_of_chunks);
            number_of_chunks -=1;
            continue;
        }

        let target_spot_btc_for_chunk = chunk_futures_quantity * spot_to_fut_btc_ratio;
        let chunk_spot_quantity = round_up_step(target_spot_btc_for_chunk, spot_quantity_step)
            .unwrap_or(target_spot_btc_for_chunk);

        if chunk_spot_quantity < min_spot_quantity && chunk_spot_quantity.abs() > tolerance {
            if number_of_chunks == 1 { return Err(anyhow!("Spot chunk qty {} for 1 chunk < min_spot_qty {}", chunk_spot_quantity.round_dp(8), min_spot_quantity)); }
            debug!("Spot chunk qty {} for {} chunks < min_spot_qty {}. Reducing chunk count.", chunk_spot_quantity.round_dp(8), number_of_chunks, min_spot_quantity);
            number_of_chunks -= 1;
            continue;
        }
        if chunk_spot_quantity < tolerance && min_spot_quantity >= tolerance {
            if number_of_chunks == 1 { return Err(anyhow!("Spot chunk qty is zero for 1 chunk, but min_spot_qty {} is non-zero.", min_spot_quantity));}
            debug!("Spot chunk qty is zero for {} chunks with non-zero min_spot_qty. Reducing chunk count.", number_of_chunks);
            number_of_chunks -=1;
            continue;
        }

        let chunk_spot_value_estimate = chunk_spot_quantity * current_spot_price;
        let spot_notional_ok = min_spot_notional_value.map_or(true, |min_val| chunk_spot_value_estimate >= min_val || (chunk_spot_value_estimate < tolerance && min_val < tolerance));

        let chunk_futures_value_estimate = chunk_futures_quantity * current_futures_price_estimate;
        let futures_notional_ok = min_futures_notional_value.map_or(true, |min_val| chunk_futures_value_estimate >= min_val || (chunk_futures_value_estimate < tolerance && min_val < tolerance));

        if !spot_notional_ok || !futures_notional_ok {
            if number_of_chunks == 1 {
                return Err(anyhow!(
                    "Failed to meet min notional for 1 chunk. Spot ValEst: {} (MinNotional:{:?}), Fut ValEst: {} (MinNotional:{:?})",
                    chunk_spot_value_estimate.round_dp(2), min_spot_notional_value, chunk_futures_value_estimate.round_dp(2), min_futures_notional_value
                ));
            }
            debug!("Notional value check failed for {} chunks. SpotNotionalOK={}, FutNotionalOK={}. Reducing chunk count.", number_of_chunks, spot_notional_ok, futures_notional_ok);
            number_of_chunks -= 1;
            continue;
        }

        let chunk_value_imbalance = (chunk_spot_value_estimate - chunk_futures_value_estimate).abs();
        let value_comparison_base = if chunk_spot_value_estimate > tolerance && chunk_futures_value_estimate > tolerance {
            chunk_spot_value_estimate.max(chunk_futures_value_estimate)
        } else {
            (chunk_spot_value_estimate.abs() + chunk_futures_value_estimate.abs()).max(tolerance)
        };

        let per_chunk_imbalance_ratio = if value_comparison_base > dec!(1e-9) {
            chunk_value_imbalance / value_comparison_base
        } else if chunk_value_imbalance > dec!(1e-9) {
            Decimal::MAX
        } else {
            Decimal::ZERO
        };

        let per_chunk_imbalance_ok = per_chunk_imbalance_ratio <= MAX_PER_CHUNK_VALUE_IMBALANCE_RATIO_FOR_SETUP;

        if number_of_chunks == 1 && !per_chunk_imbalance_ok {
            error!(
                "Final attempt with 1 chunk failed value balance check. Ratio: {:.4} (max: {}). Chunk Spot Qty: {:.8}, Val: {:.2}. Chunk Fut Qty: {:.8}, Val: {:.2}. This indicates that target BTC quantities and current prices make it impossible to form a balanced chunk meeting all criteria.",
                per_chunk_imbalance_ratio, MAX_PER_CHUNK_VALUE_IMBALANCE_RATIO_FOR_SETUP,
                chunk_spot_quantity, chunk_spot_value_estimate,
                chunk_futures_quantity, chunk_futures_value_estimate
            );
             return Err(anyhow!(
                "Cannot achieve per-chunk value balance (ratio {:.4} > max {:.2}) even with 1 chunk. SpotQty: {}, FutQty: {}. SpotValEst: {}, FutValEst: {}",
                per_chunk_imbalance_ratio, MAX_PER_CHUNK_VALUE_IMBALANCE_RATIO_FOR_SETUP, chunk_spot_quantity.round_dp(8), chunk_futures_quantity.round_dp(8),
                chunk_spot_value_estimate.round_dp(2), chunk_futures_value_estimate.round_dp(2)
            ));
        }

        if per_chunk_imbalance_ok {
            debug!(
                "Selected chunk count: {}, spot_chunk_qty: {}, fut_chunk_qty: {}. Spot_val_est/chunk: ~{}, Fut_val_est/chunk: ~{}. Per-chunk_imbalance_ratio: {:.4}",
                number_of_chunks, chunk_spot_quantity.round_dp(8), chunk_futures_quantity.round_dp(8),
                chunk_spot_value_estimate.round_dp(2), chunk_futures_value_estimate.round_dp(2), per_chunk_imbalance_ratio
            );
            return Ok((number_of_chunks, chunk_spot_quantity, chunk_futures_quantity));
        } else {
            debug!(
                "Chunk value balance check failed for {} chunks (ratio: {:.4}, spot_val_est: {:.2}, fut_val_est: {:.2}). Reducing chunk count.",
                number_of_chunks, per_chunk_imbalance_ratio, chunk_spot_value_estimate, chunk_futures_value_estimate
            );
            number_of_chunks -= 1;
        }
    }
}

fn round_up_step(value: Decimal, step: Decimal) -> Result<Decimal> {
    if step < Decimal::ZERO {
        return Err(anyhow!("Rounding step cannot be negative: {}", step));
    }
    if step == Decimal::ZERO {
        return Ok(value.normalize());
    }
    if value <= Decimal::ZERO {
        if value == Decimal::ZERO { return Ok(Decimal::ZERO); }
        warn!("round_up_step called with negative value: {} for quantity, returning zero.", value);
        return Ok(Decimal::ZERO);
    }

    let positive_step = step.abs();
    let internal_precision = value.scale().max(positive_step.scale()) + positive_step.scale() + 5;

    let value_scaled = value.round_dp_with_strategy(internal_precision, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
    let step_scaled = positive_step.round_dp_with_strategy(internal_precision, rust_decimal::RoundingStrategy::MidpointAwayFromZero);

    if step_scaled == Decimal::ZERO {
        warn!("Scaled rounding step became zero for value={}, step={}. Returning original value normalized.", value, step);
        return Ok(value.normalize());
    }
    let result = ((value_scaled / step_scaled).ceil() * step_scaled).normalize();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_round_up_step_basic() -> Result<()> {
        assert_eq!(round_up_step(dec!(1.234), dec!(0.1))?, dec!(1.3));
        assert_eq!(round_up_step(dec!(1.2), dec!(0.1))?, dec!(1.2));
        assert_eq!(round_up_step(dec!(0.0004), dec!(0.001))?, dec!(0.001));
        assert_eq!(round_up_step(dec!(0.0010000001), dec!(0.001))?, dec!(0.002));
        assert_eq!(round_up_step(dec!(0.0008571), dec!(0.001))?, dec!(0.001));
        assert_eq!(round_up_step(dec!(0.0008574), dec!(0.000001))?, dec!(0.000858));
        assert_eq!(round_up_step(Decimal::ZERO, dec!(0.001))?, Decimal::ZERO);
        assert_eq!(round_up_step(dec!(-0.0005), dec!(0.001))?, Decimal::ZERO);
        Ok(())
    }

    #[test]
    fn test_calculate_chunks_corrected_inputs() -> Result<()> {
        let overall_target_spot_btc = dec!(0.005001);
        let overall_target_futures_btc = dec!(0.005);
        let spot_price = dec!(90573.08);
        let fut_price_estimate = dec!(90573.08);
        let target_chunks = 15;
        let min_spot_q = dec!(0.000048);
        let min_fut_q = dec!(0.001);
        let spot_step = dec!(0.000001);
        let fut_step = dec!(0.001);

        let (count, spot_q, fut_q) = calculate_auto_chunk_parameters(
            overall_target_spot_btc, overall_target_futures_btc,
            spot_price, fut_price_estimate,
            target_chunks,
            min_spot_q, min_fut_q, spot_step, fut_step,
            None, None,
        )?;
        println!("Test with corrected inputs (expect 6 chunks): count={}, spot_q={}, fut_q={}", count, spot_q, fut_q);
        assert_eq!(count, 6);
        assert_eq!(spot_q.round_dp_with_strategy(6, rust_decimal::RoundingStrategy::MidpointAwayFromZero), dec!(0.001001));
        assert_eq!(fut_q, dec!(0.001));
        Ok(())
    }

    #[test]
    fn test_calculate_chunks_force_one_chunk() -> Result<()> {
        let overall_target_spot_btc = dec!(0.001001);
        let overall_target_futures_btc = dec!(0.001);
        let spot_price = dec!(100000.0);
        let fut_price_estimate = dec!(100000.0);
        let target_chunks = 15;
        let min_spot_q = dec!(0.001);
        let min_fut_q = dec!(0.001);
        let spot_step = dec!(0.000001);
        let fut_step = dec!(0.001);

        let (count, spot_q, fut_q) = calculate_auto_chunk_parameters(
            overall_target_spot_btc, overall_target_futures_btc,
            spot_price, fut_price_estimate,
            target_chunks,
            min_spot_q, min_fut_q, spot_step, fut_step,
            None, None,
        )?;
        println!("Test force one chunk: count={}, spot_q={}, fut_q={}", count, spot_q, fut_q);
        assert_eq!(count, 1);
        assert_eq!(spot_q.round_dp_with_strategy(6, rust_decimal::RoundingStrategy::MidpointAwayFromZero), dec!(0.001001));
        assert_eq!(fut_q, dec!(0.001));
        Ok(())
    }
}