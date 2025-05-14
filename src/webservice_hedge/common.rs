// src/webservice_hedge/common.rs

use anyhow::{anyhow, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{debug, warn, error};

const MAX_PER_CHUNK_VALUE_IMBALANCE_RATIO_FOR_SETUP: Decimal = dec!(0.15);

pub fn calculate_auto_chunk_parameters(
    initial_target_spot_value: Decimal,
    initial_target_futures_quantity: Decimal,
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
    if initial_target_spot_value <= Decimal::ZERO {
        return Err(anyhow!("Initial target spot value must be positive"));
    }
    // initial_target_futures_quantity может быть 0, если это spot-only операция (не для хеджа)
    // Для хеджа он должен быть > 0
    if initial_target_futures_quantity < Decimal::ZERO { // Строго меньше нуля - ошибка
        return Err(anyhow!("Initial target futures quantity cannot be negative"));
    }
    if initial_target_futures_quantity == Decimal::ZERO && initial_target_spot_value > Decimal::ZERO {
        // Это скорее для unhedge или spot-only, для хеджа фьючи должны быть
        warn!("Initial target futures quantity is zero, but spot value is positive. Proceeding as spot-only if possible.");
    }


    let total_spot_quantity_estimate_for_op = if current_spot_price > Decimal::ZERO {
        initial_target_spot_value / current_spot_price
    } else {
        return Err(anyhow!("Current spot price is zero, cannot estimate total spot quantity"));
    };

    if total_spot_quantity_estimate_for_op <= Decimal::ZERO && initial_target_spot_value > Decimal::ZERO {
        return Err(anyhow!("Initial estimated total spot quantity is non-positive while target spot value is positive (price issue?)"));
    }

    // --- ДОБАВЛЕНО ДЕТАЛЬНОЕ ЛОГИРОВАНИЕ ---
    debug!(
        "calculate_auto_chunk_parameters input: initial_target_spot_value={}, initial_target_futures_quantity={}, current_spot_price={}, current_futures_price_estimate={}",
        initial_target_spot_value, initial_target_futures_quantity, current_spot_price, current_futures_price_estimate
    );
    debug!(
        "Derived total_spot_quantity_estimate_for_op = {}",
        total_spot_quantity_estimate_for_op
    );
    // --- КОНЕЦ ЛОГИРОВАНИЯ ---

    if initial_target_futures_quantity == Decimal::ZERO {
        // Логика для spot-only (для unhedge или если хедж только спотом, что нетипично)
        // Оставляем как есть, т.к. основной фокус на хедже с фьючерсами
        if total_spot_quantity_estimate_for_op < min_spot_quantity && total_spot_quantity_estimate_for_op > dec!(1e-12) {
             return Err(anyhow!("Total spot quantity is less than min spot quantity for spot-only operation."));
        }
        let mut num_chunks_spot_only = target_chunk_count.max(1);
        loop {
            if num_chunks_spot_only == 0 { return Err(anyhow!("Failed to calculate chunks for spot-only operation (num_chunks is 0)"));}
            let chunk_spot_raw = total_spot_quantity_estimate_for_op / Decimal::from(num_chunks_spot_only);
            let chunk_spot = round_up_step(chunk_spot_raw, spot_quantity_step).unwrap_or(chunk_spot_raw);

            let spot_qty_ok = chunk_spot >= min_spot_quantity || chunk_spot < dec!(1e-12);
            let spot_val_est = chunk_spot * current_spot_price;
            let spot_notional_ok = min_spot_notional_value.map_or(true, |min_val| spot_val_est >= min_val || (spot_val_est < dec!(1e-12) && min_val < dec!(1e-12)));

            if spot_qty_ok && spot_notional_ok {
                debug!("Spot-only chunk setup: count={}, spot_chunk_qty={}", num_chunks_spot_only, chunk_spot);
                return Ok((num_chunks_spot_only, chunk_spot, Decimal::ZERO));
            }
            if num_chunks_spot_only == 1 {
                return Err(anyhow!("Failed to meet min requirements for spot-only operation with 1 chunk. Qty: {}, ValEst: {}", chunk_spot, spot_val_est));
            }
            num_chunks_spot_only -= 1;
        }
    }


    let spot_to_fut_btc_ratio = total_spot_quantity_estimate_for_op / initial_target_futures_quantity;
    // --- ИЗМЕНЕН WARN для большей информативности ---
    if spot_to_fut_btc_ratio < dec!(0.98) || spot_to_fut_btc_ratio > dec!(1.02) { // Слегка расширил допустимый диапазон перед варнингом, т.к. комиссии влияют
        warn!(
            "Unusual spot_to_fut_btc_ratio: {:.6}. Calculated from: total_spot_qty_est ({:.8}) / initial_target_fut_qty ({:.8}). This might lead to value imbalances if not intended. Ratio should ideally be very close to 1.0 plus spot_fee_rate.",
            spot_to_fut_btc_ratio, total_spot_quantity_estimate_for_op, initial_target_futures_quantity
        );
    } else {
        debug!(
            "Calculated spot_to_fut_btc_ratio: {:.6} (total_spot_qty_est: {:.8}, initial_target_fut_qty: {:.8})",
            spot_to_fut_btc_ratio, total_spot_quantity_estimate_for_op, initial_target_futures_quantity
        );
    }
    // --- КОНЕЦ ИЗМЕНЕНИЯ WARN ---

    let mut number_of_chunks = target_chunk_count.max(1);

    loop {
        if number_of_chunks == 0 {
            return Err(anyhow!("Failed to find suitable chunk size (number_of_chunks became 0 unexpectedly)"));
        }

        let num_chunks_decimal = Decimal::from(number_of_chunks);
        let tolerance = dec!(1e-12);

        let chunk_futures_quantity_raw = initial_target_futures_quantity / num_chunks_decimal;
        let chunk_futures_quantity = round_up_step(chunk_futures_quantity_raw, futures_quantity_step)
            .unwrap_or(chunk_futures_quantity_raw);

        if chunk_futures_quantity < min_futures_quantity && chunk_futures_quantity.abs() > tolerance {
            if number_of_chunks == 1 {
                return Err(anyhow!("Futures chunk qty {} for 1 chunk < min_futures_qty {}. Target futures: {}", chunk_futures_quantity.round_dp(8), min_futures_quantity, initial_target_futures_quantity.round_dp(8)));
            }
            debug!("Futures chunk qty {} too small for {} chunks. Reducing chunk count.", chunk_futures_quantity.round_dp(8), number_of_chunks);
            number_of_chunks -= 1;
            continue;
        }
        if chunk_futures_quantity < tolerance && min_futures_quantity >= tolerance {
             if number_of_chunks == 1 { return Err(anyhow!("Futures chunk qty is zero for 1 chunk, but min_futures_qty {} is non-zero.", min_futures_quantity)); }
            debug!("Futures chunk qty is zero for {} chunks. Reducing chunk count.", number_of_chunks);
            number_of_chunks -=1;
            continue;
        }

        let target_spot_btc_for_chunk = chunk_futures_quantity * spot_to_fut_btc_ratio;
        let chunk_spot_quantity = round_up_step(target_spot_btc_for_chunk, spot_quantity_step)
            .unwrap_or(target_spot_btc_for_chunk);

        if chunk_spot_quantity < min_spot_quantity && chunk_spot_quantity.abs() > tolerance {
            if number_of_chunks == 1 { return Err(anyhow!("Spot chunk qty {} for 1 chunk < min_spot_qty {}", chunk_spot_quantity.round_dp(8), min_spot_quantity)); }
            debug!("Spot chunk qty {} too small for {} chunks. Reducing chunk count.", chunk_spot_quantity.round_dp(8), number_of_chunks);
            number_of_chunks -= 1;
            continue;
        }
        if chunk_spot_quantity < tolerance && min_spot_quantity >= tolerance {
            if number_of_chunks == 1 { return Err(anyhow!("Spot chunk qty is zero for 1 chunk, but min_spot_qty {} is non-zero.", min_spot_quantity));}
            debug!("Spot chunk qty is zero for {} chunks. Reducing chunk count.", number_of_chunks);
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
            debug!("Notional value check failed for {} chunks. SpotOK={}, FutOK={}. Reducing chunk count.", number_of_chunks, spot_notional_ok, futures_notional_ok);
            number_of_chunks -= 1;
            continue;
        }

        let chunk_value_imbalance = (chunk_spot_value_estimate - chunk_futures_value_estimate).abs();
        let value_comparison_base = if chunk_spot_value_estimate > tolerance && chunk_futures_value_estimate > tolerance {
            chunk_spot_value_estimate.max(chunk_futures_value_estimate)
        } else {
            (chunk_spot_value_estimate.abs() + chunk_futures_value_estimate.abs()).max(tolerance)
        };

        let per_chunk_imbalance_ratio = if value_comparison_base > dec!(1e-9) { // Защита от деления на ноль
            chunk_value_imbalance / value_comparison_base
        } else if chunk_value_imbalance > dec!(1e-9) { // Если дисбаланс есть, а база близка к нулю, считаем ratio большим
            Decimal::MAX
        } else { // Если и дисбаланс и база близки к нулю, считаем ratio нулевым
            Decimal::ZERO
        };

        let per_chunk_imbalance_ok = per_chunk_imbalance_ratio <= MAX_PER_CHUNK_VALUE_IMBALANCE_RATIO_FOR_SETUP;

        if per_chunk_imbalance_ok {
            debug!(
                "Selected chunk count: {}, spot_chunk_qty: {}, fut_chunk_qty: {}. Spot_val_est/chunk: ~{}, Fut_val_est/chunk: ~{}. Per-chunk_imbalance_ratio: {:.4}",
                number_of_chunks, chunk_spot_quantity.round_dp(8), chunk_futures_quantity.round_dp(8),
                chunk_spot_value_estimate.round_dp(2), chunk_futures_value_estimate.round_dp(2), per_chunk_imbalance_ratio
            );
            return Ok((number_of_chunks, chunk_spot_quantity, chunk_futures_quantity));
        } else {
            if number_of_chunks == 1 {
                error!(
                    "Failed to find suitable parameters even for 1 chunk due to per-chunk value imbalance. Ratio: {:.4} (max allowed: {}). Spot Qty: {}, Fut Qty: {}, SpotVal: {}, FutVal: {}",
                    per_chunk_imbalance_ratio, MAX_PER_CHUNK_VALUE_IMBALANCE_RATIO_FOR_SETUP, chunk_spot_quantity.round_dp(8), chunk_futures_quantity.round_dp(8),
                    chunk_spot_value_estimate.round_dp(2), chunk_futures_value_estimate.round_dp(2)
                );
                return Err(anyhow!(
                    "Cannot achieve per-chunk value balance (ratio {:.4} > max {:.2}) even with 1 chunk. SpotQty: {}, FutQty: {}. SpotValEst: {}, FutValEst: {}",
                    per_chunk_imbalance_ratio, MAX_PER_CHUNK_VALUE_IMBALANCE_RATIO_FOR_SETUP, chunk_spot_quantity.round_dp(8), chunk_futures_quantity.round_dp(8),
                    chunk_spot_value_estimate.round_dp(2), chunk_futures_value_estimate.round_dp(2)
                ));
            }
            debug!(
                "Chunk value balance check failed for {} chunks (ratio: {:.4}). Reducing chunk count.",
                number_of_chunks, per_chunk_imbalance_ratio
            );
            number_of_chunks -= 1;
        }
    }
}

// Оставляем round_up_step без изменений, если он корректен
fn round_up_step(value: Decimal, step: Decimal) -> Result<Decimal> {
    if step < Decimal::ZERO {
        return Err(anyhow!("Rounding step cannot be negative: {}", step));
    }
    if step == Decimal::ZERO {
        return Ok(value.normalize());
    }
    if value <= Decimal::ZERO { // Если значение 0 или отрицательное, округление вверх до шага не имеет смысла так, как для положительных.
                              // Оставляем как есть или возвращаем 0, если value было <= 0.
        return Ok(value.max(Decimal::ZERO).normalize()); // Возвращаем 0, если value отрицательное, иначе само value
    }

    let positive_step = step.abs();
    let internal_precision = value.scale().max(positive_step.scale()) + positive_step.scale() + 5;

    let value_scaled = value.round_dp_with_strategy(internal_precision, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
    let step_scaled = positive_step.round_dp_with_strategy(internal_precision, rust_decimal::RoundingStrategy::MidpointAwayFromZero);

    if step_scaled == Decimal::ZERO {
        warn!("Scaled rounding step became zero for value={}, step={}. Returning original value.", value, step);
        return Ok(value.normalize());
    }
    let result = ((value_scaled / step_scaled).ceil() * step_scaled).normalize();
    Ok(result)
}

// Тесты оставляем без изменений
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
        assert_eq!(round_up_step(dec!(-0.0005), dec!(0.001))?, Decimal::ZERO); // Округление отрицательного вверх к нулю
        Ok(())
    }

     #[test]
     fn test_calculate_chunks_op_id_7_scenario() -> Result<()> {
         let initial_spot_val = dec!(600.1975992);
         let initial_fut_qty = dec!(0.006);
         let spot_price = dec!(99999.6);
         let fut_price_estimate = dec!(100000.0);
         let target_chunks = 15;
         let min_spot_q = dec!(0.000048);
         let min_fut_q = dec!(0.001);
         let spot_step = dec!(0.000001);
         let fut_step = dec!(0.001);

         let (count, spot_q, fut_q) = calculate_auto_chunk_parameters(
             initial_spot_val, initial_fut_qty, spot_price, fut_price_estimate,
             target_chunks,
             min_spot_q, min_fut_q, spot_step, fut_step,
             None, None,
         )?;
         // total_spot_quantity_estimate_for_op = 600.1975992 / 99999.6 = 0.0060020008...
         // spot_to_fut_btc_ratio = 0.0060020008 / 0.006 = 1.00033346...
         // chunk_fut_qty for 6 chunks: round_up_step(0.006/6, 0.001) = 0.001
         // target_spot_btc = 0.001 * 1.00033346... = 0.00100033346...
         // chunk_spot_qty = round_up_step(0.00100033346, 0.000001) = 0.001001

         println!("Test op_id=7 case: count={}, spot_q={}, fut_q={}", count, spot_q, fut_q);
         assert_eq!(count, 6);
         assert_eq!(spot_q.round_dp_with_strategy(6, rust_decimal::RoundingStrategy::MidpointAwayFromZero), dec!(0.001001));
         assert_eq!(fut_q, dec!(0.001));
         Ok(())
     }

    #[test]
    fn test_calculate_chunks_scenario_from_log_op_id_1() -> Result<()> {
        let initial_spot_val = dec!(613.61495016);
        let initial_fut_qty = dec!(0.006);
        let spot_price = dec!(102235.08);
        let fut_price_estimate = dec!(102235.08);
        let target_chunks = 15;
        let min_spot_q = dec!(0.000048);
        let min_fut_q = dec!(0.001);
        let spot_step = dec!(0.000001);
        let fut_step = dec!(0.001);

        let (count, spot_q, fut_q) = calculate_auto_chunk_parameters(
            initial_spot_val, initial_fut_qty, spot_price, fut_price_estimate,
            target_chunks,
            min_spot_q, min_fut_q, spot_step, fut_step,
            None, None,
        )?;
        // total_spot_quantity_estimate_for_op = 613.61495016 / 102235.08 = 0.006002
        // spot_to_fut_btc_ratio = 0.006002 / 0.006 = 1.000333...

        // Ожидаем, что при таком ratio и min_fut_q=0.001, если fut_raw = 0.0004 (0.006/15), то округлится до 0.001.
        // Тогда spot_chunk будет ~0.001 * 1.000333... = 0.001000333... округленный до 0.001001
        println!("Test op_id=1 case (new logic): count={}, spot_q={}, fut_q={}", count, spot_q, fut_q);
        assert_eq!(count, 6); // Из-за min_fut_q=0.001, 15 чанков по 0.0004 не пройдут, снизится до 6 чанков по 0.001
        assert_eq!(spot_q.round_dp_with_strategy(6, rust_decimal::RoundingStrategy::MidpointAwayFromZero), dec!(0.001001));
        assert_eq!(fut_q, dec!(0.001));
        Ok(())
    }
}