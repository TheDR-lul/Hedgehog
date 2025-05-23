// src/utils.rs

/// Округление вниз с шагом `step`
pub fn round_step(value: f64, step: f64) -> f64 {
    (value / step).floor() * step
}

pub fn trading_symbol(base: &str, quote: &str) -> String {
    format!("{}{}", base.to_uppercase(), quote.to_uppercase())
}
