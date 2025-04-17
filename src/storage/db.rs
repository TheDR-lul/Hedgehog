// src/storage/db.rs
use sqlx::SqlitePool;
use anyhow::{Result, Context};
use std::path::Path;

/// Локальная база данных через SQLite
pub struct Db {
    pub pool: SqlitePool,
}

impl Db {
    /// Подключается к базе по пути `path`, создавая папку при необходимости
    pub async fn connect(path: &str) -> Result<Self> {
        // Создаём родительскую директорию, если её нет
        if let Some(dir) = Path::new(path).parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Не удалось создать директорию для БД: {:?}", dir))?;
        }

        // Формируем URL для sqlite
        let db_url = format!("sqlite://{}", path);

        // Подключаемся к базе (создаст файл, если его нет)
        let pool = SqlitePool::connect(&db_url).await?;
        Ok(Db { pool })
    }
}