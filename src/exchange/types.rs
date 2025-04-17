pub struct Balance { pub free: f64, pub locked: f64 }
pub enum OrderSide { Buy, Sell }
pub struct Order { pub id: String, pub filled: f64 }
