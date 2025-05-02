// // tests/calculation_tests.rs

// use hedgehog::utils::round_step;   // <-- корректный путь

// #[test]
// fn test_round_step() {
//     let val  = 123.456_f64;
//     let step = 0.1_f64;
//     let rounded = round_step(val, step);
//     assert!((rounded - 123.4_f64).abs() < 1e-8);
// }

// #[test]
// fn test_volume_calculation() {
//     let sum  = 1000.0_f64;
//     let v    = 0.6_f64;
//     let mmr  = 0.02_f64;
//     let spot = sum / ((1.0 + v) * (1.0 + mmr));
//     let fut  = sum - spot;
//     assert!((spot - 612.7449_f64).abs() < 1e-4);
//     assert!((fut  - 387.2551_f64).abs() < 1e-4);
// }
