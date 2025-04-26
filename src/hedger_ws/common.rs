// src/hedger_ws/common.rs

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use tracing::{debug, warn}; // Добавили warn

// Функция для автоматического расчета параметров чанков
// Возвращает: Ok((итоговое_количество_чанков, объем_спота_на_чанк, объем_фьюча_на_чанк))
// или Err, если не удалось подобрать размер даже для 1 чанка.
pub fn calculate_auto_chunk_parameters(
    // --- Цели ---
    initial_target_spot_value: Decimal, // Общая целевая стоимость спота (из расчета по волатильности)
    initial_target_futures_quantity: Decimal, // Общий целевой объем фьючерса (из расчета)
    // --- Рыночные данные ---
    current_spot_price: Decimal, // Текущая цена спота для оценки объемов
    current_futures_price: Decimal, // --- ДОБАВЛЕНО: Цена фьючерса ---
    // --- Настройки ---
    target_chunk_count: u32, // Желаемое количество чанков из конфига
    // --- Лимиты Биржи ---
    min_spot_quantity: Decimal,
    min_futures_quantity: Decimal,
    spot_quantity_step: Decimal,    // Шаг округления объема спота
    futures_quantity_step: Decimal, // Шаг округления объема фьючерса
    min_spot_notional_value: Option<Decimal>, // Минимальная стоимость спот ордера (если есть)
    min_futures_notional_value: Option<Decimal>, // Минимальная стоимость фьюч ордера (если есть)

) -> Result<(u32, Decimal, Decimal)> {
    if target_chunk_count == 0 {
        return Err(anyhow!("Target chunk count cannot be zero"));
    }
    if current_spot_price <= Decimal::ZERO || current_futures_price <= Decimal::ZERO {
        return Err(anyhow!("Current spot and futures prices must be positive for chunk calculation"));
    }
     if initial_target_spot_value <= Decimal::ZERO || initial_target_futures_quantity <= Decimal::ZERO {
         return Err(anyhow!("Initial target values must be positive for chunk calculation"));
     }

    // Начальная оценка общего объема спота
    let total_spot_quantity_estimate = initial_target_spot_value / current_spot_price;
    if total_spot_quantity_estimate <= Decimal::ZERO {
        return Err(anyhow!("Initial estimated spot quantity is non-positive"));
    }

    let mut number_of_chunks = target_chunk_count;

    loop {
        if number_of_chunks == 0 {
            // Не смогли подобрать размер даже для 1 чанка
            return Err(anyhow!(
                "Failed to find suitable chunk size respecting exchange limits (min qty/notional). Total value might be too low or limits too high."
            ));
        }

        let num_chunks_decimal = Decimal::from(number_of_chunks);

        // Рассчитываем примерные объемы на чанк
        let chunk_spot_quantity_raw = total_spot_quantity_estimate / num_chunks_decimal;
        let chunk_futures_quantity_raw = initial_target_futures_quantity / num_chunks_decimal;

        // Округляем ВВЕРХ до шага инструмента
        let chunk_spot_quantity = round_up_step(chunk_spot_quantity_raw, spot_quantity_step)?;
        let chunk_futures_quantity = round_up_step(chunk_futures_quantity_raw, futures_quantity_step)?;

        // --- Проверки на соответствие лимитам ---

        // 1. Минимальный объем
        // Используем небольшой допуск, так как округление вверх может сделать значение чуть > 0, но < min
        let tolerance = dec!(1e-12);
        let spot_qty_ok = chunk_spot_quantity >= min_spot_quantity || chunk_spot_quantity < tolerance;
        let futures_qty_ok = chunk_futures_quantity >= min_futures_quantity || chunk_futures_quantity < tolerance;

        // 2. Минимальная стоимость (Notional Value)
        let chunk_spot_value_estimate = chunk_spot_quantity * current_spot_price;
        let spot_notional_ok = min_spot_notional_value.map_or(true, |min_val| chunk_spot_value_estimate >= min_val || chunk_spot_value_estimate < tolerance);

        // TODO: Нужна current_futures_price для точной проверки фьючерса
        // Пока проверяем грубо по спотовой цене или пропускаем, если лимита нет
        let chunk_futures_value_estimate = chunk_futures_quantity * current_futures_price; // Используем цену фьючерса
        let futures_notional_ok = min_futures_notional_value.map_or(true, |min_val| chunk_futures_value_estimate >= min_val || chunk_futures_value_estimate < tolerance);


        if spot_qty_ok && futures_qty_ok && spot_notional_ok && futures_notional_ok {
            // Размеры чанка подходят
            debug!(
                "Selected chunk count: {}, spot chunk qty: {}, fut chunk qty: {}",
                number_of_chunks, chunk_spot_quantity, chunk_futures_quantity
            );
            // Возвращаем рассчитанные базовые размеры чанка
            // Важно: эти объемы могут быть чуть больше из-за округления вверх,
            // логика исполнения последнего чанка должна это учитывать.
            return Ok((number_of_chunks, chunk_spot_quantity, chunk_futures_quantity));
        } else {
            // Уменьшаем количество чанков (увеличиваем их размер) и пробуем снова
            debug!(
                "Chunk size check failed for {} chunks. spot_qty_ok: {}, futures_qty_ok: {}, spot_notional_ok: {}, futures_notional_ok: {}. Reducing chunk count.",
                number_of_chunks, spot_qty_ok, futures_qty_ok, spot_notional_ok, futures_notional_ok
            );
            number_of_chunks -= 1;
        }
    }
}

// Округление вверх с шагом (вспомогательная функция)
// Теперь возвращает Result, так как деление на ноль возможно
fn round_up_step(value: Decimal, step: Decimal) -> Result<Decimal> {
    if step < Decimal::ZERO {
        return Err(anyhow!("Rounding step cannot be negative: {}", step));
    }
     if step == Decimal::ZERO {
        // Если шаг 0, не округляем (или возвращаем ошибку?)
        // Пока просто возвращаем значение, но логируем предупреждение
        if value != Decimal::ZERO { // Логируем только если значение не нулевое
             warn!("Rounding step is zero, returning original value: {}", value);
        }
        return Ok(value.normalize());
     }
    if value == Decimal::ZERO {
        return Ok(Decimal::ZERO);
    }
    // Используем `abs()` на шаге, чтобы избежать проблем с отрицательным шагом
    let positive_step = step.abs();
    // Округляем до точности шага + запас для избежания ошибок float при делении
    // Используем max(3) для точности, чтобы избежать слишком большой точности для малых шагов
    let precision = positive_step.scale().max(3) + 3; // Добавил max(3)
    let value_scaled = value.round_dp(precision);
    let step_scaled = positive_step.round_dp(precision);

    // (value / step).ceil() * step
    Ok(((value_scaled / step_scaled).ceil() * step_scaled).normalize())
}


// --- Тесты ---
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_round_up_step_basic() -> Result<()> {
        assert_eq!(round_up_step(dec!(1.234), dec!(0.1))?, dec!(1.3));
        assert_eq!(round_up_step(dec!(1.2), dec!(0.1))?, dec!(1.2));
        assert_eq!(round_up_step(dec!(1.234), dec!(0.01))?, dec!(1.24));
        assert_eq!(round_up_step(dec!(1.23), dec!(0.01))?, dec!(1.23));
        assert_eq!(round_up_step(dec!(123), dec!(10))?, dec!(130));
        assert_eq!(round_up_step(dec!(120), dec!(10))?, dec!(120));
        assert_eq!(round_up_step(dec!(0.000123), dec!(0.00001))?, dec!(0.00013));
         // Тест с очень маленьким шагом
         assert_eq!(round_up_step(dec!(1.123456789), dec!(0.00000001))?, dec!(1.12345679));
        Ok(())
    }

     #[test]
     fn test_round_up_step_zero_edge_cases() -> Result<()> {
         assert_eq!(round_up_step(dec!(0.0), dec!(0.1))?, dec!(0.0));
         assert_eq!(round_up_step(dec!(1.23), dec!(0.0))?, dec!(1.23)); // Шаг ноль
         Ok(())
     }

      #[test]
      fn test_round_up_step_negative_step_error() {
          assert!(round_up_step(dec!(1.23), dec!(-0.1)).is_err());
      }


    #[test]
    fn test_calculate_chunks_simple() -> Result<()> {
        let (count, spot_q, fut_q) = calculate_auto_chunk_parameters(
            dec!(1000.0), // target_spot_value
            dec!(0.5),    // target_futures_quantity
            dec!(2000.0), // current_spot_price
            dec!(2001.0), // current_futures_price (немного отличается)
            10,           // target_chunk_count
            dec!(0.001),  // min_spot_quantity
            dec!(0.001),  // min_futures_quantity
            dec!(0.001),  // spot_quantity_step
            dec!(0.001),  // futures_quantity_step
            Some(dec!(10.0)), // min_spot_notional
            Some(dec!(10.0)), // min_futures_notional
        )?;

        assert_eq!(count, 10);
        // chunk_spot_raw = (1000/2000) / 10 = 0.05 -> округление вверх = 0.05
        assert_eq!(spot_q, dec!(0.05));
        // chunk_fut_raw = 0.5 / 10 = 0.05 -> округление вверх = 0.05
        assert_eq!(fut_q, dec!(0.05));
        // Проверка notional: spot_val=0.05*2000=100 (>=10), fut_val=0.05*2001=100.05 (>=10) - OK
        Ok(())
    }

     #[test]
     fn test_calculate_chunks_adjust_count_due_to_min_qty() -> Result<()> {
         let (count, spot_q, fut_q) = calculate_auto_chunk_parameters(
             dec!(1000.0),
             dec!(0.5),
             dec!(2000.0),
             dec!(2000.0),
             10,
             dec!(0.001),
             dec!(0.1),    // min_futures_quantity = 0.1
             dec!(0.001),
             dec!(0.001),
             Some(dec!(10.0)),
             None, // Нет мин. стоимости фьюча
         )?;

         assert_eq!(count, 5); // fut_q = 0.5 / 5 = 0.1 >= 0.1
         assert_eq!(spot_q, dec!(0.1)); // spot_q = (1000/2000) / 5 = 0.1
         assert_eq!(fut_q, dec!(0.1));
         Ok(())
     }

      #[test]
      fn test_calculate_chunks_adjust_count_due_to_min_notional() -> Result<()> {
           let (count, spot_q, fut_q) = calculate_auto_chunk_parameters(
               dec!(50.0),
               dec!(1.0),
               dec!(50.0),
               dec!(50.0),
               10,
               dec!(0.01),
               dec!(0.01),
               dec!(0.01),
               dec!(0.01),
               Some(dec!(10.0)), // min_spot_notional = $10
               None,
           )?;
           // При 10 чанках: spot_q_raw = (50/50) / 10 = 0.1. chunk_value = 0.1 * 50 = $5. < $10. Не ОК.
           // При 5 чанках: spot_q_raw = 1.0 / 5 = 0.2. chunk_value = 0.2*50 = $10. >= $10. ОК.
           assert_eq!(count, 5);
           assert_eq!(spot_q, dec!(0.2));
           assert_eq!(fut_q, dec!(0.2)); // fut_q = 1.0 / 5 = 0.2
           Ok(())
      }

       #[test]
       fn test_calculate_chunks_adjust_due_to_fut_notional() -> Result<()> {
            // Похоже на прошлый, но лимит на фьюче
            let (count, spot_q, fut_q) = calculate_auto_chunk_parameters(
                dec!(100.0), // spot value
                dec!(1.0),   // futures qty
                dec!(50.0),  // spot price
                dec!(50.0),  // futures price
                10,          // target chunks
                dec!(0.01), dec!(0.01), dec!(0.01), dec!(0.01),
                None,
                Some(dec!(10.0)) // min futures notional $10
            )?;
            // При 10 чанках: fut_q_raw = 1.0 / 10 = 0.1. chunk_value = 0.1 * 50 = $5 < $10. Не ОК.
            // При 5 чанках: fut_q_raw = 1.0 / 5 = 0.2. chunk_value = 0.2 * 50 = $10 >= $10. ОК.
            assert_eq!(count, 5);
            assert_eq!(spot_q, dec!(0.4)); // spot_q = (100/50) / 5 = 0.4
            assert_eq!(fut_q, dec!(0.2));
            Ok(())
       }


       #[test]
       fn test_calculate_chunks_fails_if_1_chunk_too_small() {
            let result = calculate_auto_chunk_parameters(
                dec!(5.0),    // target_spot_value (всего $5)
                dec!(0.1),    // target_futures_quantity
                dec!(50.0),   // current_spot_price
                dec!(50.0),   // current_futures_price
                10,           // target_chunk_count
                dec!(0.01),   // min_spot_quantity
                dec!(0.01),   // min_futures_quantity
                dec!(0.01),   // spot_quantity_step
                dec!(0.01),   // futures_quantity_step
                Some(dec!(10.0)), // min_spot_notional ($10) <--- Выше всей суммы
                None,         // min_futures_notional
            );
           assert!(result.is_err());
           assert!(result.unwrap_err().to_string().contains("Failed to find suitable chunk size"));
       }
}