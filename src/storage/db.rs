use sqlx::PgPool;
use anyhow::Result;

pub struct Db { pool: PgPool }

impl Db {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPool::connect(url).await?;
        Ok(Self { pool })
    }
    // CRUD для ордеров, сессий, funding rates
}
