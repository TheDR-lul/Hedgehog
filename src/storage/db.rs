// src/storage/db.rs
use anyhow::{Context, Result};
use sqlx::{Executor, SqlitePool};
use std::path::Path;

/// Локальная база данных через SQLite
#[derive(Debug)]
pub struct Db {
    pub pool: SqlitePool,
}

impl Db {
    /// Подключается к базе по `path`, создаёт нужные каталоги,
    /// файл `.db` и минимальную схему, если их ещё нет.
    pub async fn connect(path: &str) -> Result<Self> {
        // 1) создаём родительскую папку, если указана
        if let Some(dir) = Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Не удалось создать директорию {:?}", dir))?;
        }

        // 2) Подключаемся (SQLite сам создаст файл, если его нет)
        let db_url = format!("sqlite:{}", path);
        let pool   = SqlitePool::connect(&db_url).await?;

        // 3) Минимальная схема — выполняется один раз
        pool.execute(
            r#"
            CREATE TABLE IF NOT EXISTS orders (
                id      TEXT    PRIMARY KEY,
                symbol  TEXT    NOT NULL,
                side    TEXT    NOT NULL,
                qty     REAL    NOT NULL,
                price   REAL,
                ts      INTEGER NOT NULL
            );
            "#,
        )
        .await?;

        Ok(Db { pool })
    }
}