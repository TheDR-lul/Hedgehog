// src/storage/db.rs
use anyhow::{Context, Result};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
    Executor, SqlitePool,
};
use std::{env, path::PathBuf};

#[derive(Debug)]
pub struct Db {
    pub pool: SqlitePool,
}

impl Db {
    /// Подключается к SQLite, создаёт файл, если его нет,
    /// и заводит таблицу `orders`.
    pub async fn connect(path: &str) -> Result<Self> {
        // 1) абсолютный путь к файлу
        let abs_path: PathBuf = {
            let p = PathBuf::from(path);
            if p.is_absolute() {
                p
            } else {
                env::current_dir()?
                    .join(p)
            }
        };

        // 2) создаём папку, если нужна
        if let Some(dir) = abs_path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Не удалось создать директорию {:?}", dir))?;
        }

        // 3) готовим строителя опций
        let opts = SqliteConnectOptions::new()
            .filename(&abs_path)        // путь к файлу .db
            .create_if_missing(true)    // <- ключевой момент: файл создаётся автоматически
            .journal_mode(SqliteJournalMode::Wal);

        // 4) создаём пул
        let pool = SqlitePool::connect_with(opts).await?;

        // 5) инициализируем минимальную схему
        pool.execute(
            r#"
            CREATE TABLE IF NOT EXISTS orders (
                id      TEXT    PRIMARY KEY,
                symbol  TEXT    NOT NULL,
                side    TEXT    NOT NULL,
                qty     REAL    NOT NULL,
                price   REAL,
                ts      INTEGER NOT NULL
            )
            "#,
        )
        .await?;

        Ok(Db { pool })
    }
}
