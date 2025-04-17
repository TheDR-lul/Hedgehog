use crate::exchange::Bybit;
use crate::models::*;

pub struct Hedger<E: Exchange> { pub exchange: E, pub config: crate::config::Config }

impl<E: Exchange + Send + Sync> Hedger<E> {
    pub async fn run_hedge(&self, _req: HedgeRequest) -> anyhow::Result<()> { Ok(()) }
    pub async fn run_unhedge(&self, _req: UnhedgeRequest) -> anyhow::Result<()> { Ok(()) }
}
