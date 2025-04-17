#[cfg(test)]
mod tests {
    use crate::utils::round_step;

    #[test]
    fn test_round_step() {
        let val = 123.456;
        let step = 0.1;
        let rounded = round_step(val, step);
        assert!((rounded - 123.4).abs() < 1e-8);
    }

    #[test]
    fn test_volume_calculation() {
        // Пример из ТЗ: Sum=1000, V=0.6, MMR=0.02
        let sum = 1000.0;
        let v = 0.6;
        let mmr = 0.02;
        let spot = sum / ((1.0 + v) * (1.0 + mmr));
        let fut = sum - spot;
        assert!((spot - 612.7449).abs() < 1e-4);
        assert!((fut - 387.2551).abs() < 1e-4);
    }
}
