// src/storage/schema.rs

//! Определение схемы базы данных SQLite с использованием sqlx.

use sqlx::sqlite::{SqlitePool, SqliteRow};
use sqlx::{Error, FromRow, Row};
use tracing::info;

/// Асинхронная функция для применения миграций и создания таблиц.
pub async fn apply_migrations(pool: &SqlitePool) -> Result<(), Error> {
    info!("Applying database migrations...");

    // Создаем таблицу hedge_operations, если она не существует
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS hedge_operations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chat_id BIGINT NOT NULL,
            base_symbol TEXT NOT NULL,
            quote_currency TEXT NOT NULL,
            initial_sum REAL NOT NULL,
            volatility REAL NOT NULL,
            target_spot_qty REAL NOT NULL,
            target_futures_qty REAL NOT NULL,
            start_timestamp INTEGER NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('Running', 'Completed', 'Cancelled', 'Failed', 'Interrupted')),
            spot_order_id TEXT,
            spot_filled_qty REAL NOT NULL DEFAULT 0.0,
            futures_order_id TEXT,
            futures_filled_qty REAL NOT NULL DEFAULT 0.0,
            end_timestamp INTEGER,
            error_message TEXT,
            unhedged_op_id INTEGER -- Ссылка на ID операции расхеджирования, если была
        );
        "#,
    )
    .execute(pool)
    .await?;

    // Можно добавить индексы для ускорения запросов
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_hedge_operations_chat_symbol_status
        ON hedge_operations (chat_id, base_symbol, status);
        "#,
    )
    .execute(pool)
    .await?;

    // TODO: Добавить таблицу unhedge_operations, если решим разделять
    // sqlx::query(
    //     r#"
    //     CREATE TABLE IF NOT EXISTS unhedge_operations (
    //         id INTEGER PRIMARY KEY AUTOINCREMENT,
    //         original_hedge_id INTEGER NOT NULL,
    //         chat_id BIGINT NOT NULL,
    //         base_symbol TEXT NOT NULL,
    //         target_spot_qty REAL NOT NULL,
    //         target_futures_qty REAL NOT NULL,
    //         start_timestamp INTEGER NOT NULL,
    //         status TEXT NOT NULL CHECK(status IN ('Running', 'Completed', 'Cancelled', 'Failed')),
    //         spot_order_id TEXT,
    //         spot_filled_qty REAL NOT NULL DEFAULT 0.0,
    //         futures_order_id TEXT,
    //         futures_filled_qty REAL NOT NULL DEFAULT 0.0,
    //         end_timestamp INTEGER,
    //         error_message TEXT,
    //         FOREIGN KEY (original_hedge_id) REFERENCES hedge_operations (id)
    //     );
    //     "#,
    // )
    // .execute(pool)
    // .await?;


    info!("Database migrations applied successfully.");
    Ok(())
}

// Структура, соответствующая строке в таблице hedge_operations
// Добавляем поля, которые нам нужны для чтения
#[derive(Debug, FromRow, Clone)] // Добавляем Clone
pub struct HedgeOperation {
    pub id: i64, // Используем i64 для AUTOINCREMENT
    pub chat_id: i64,
    pub base_symbol: String,
    pub quote_currency: String,
    pub initial_sum: f64,
    pub volatility: f64,
    pub target_spot_qty: f64,
    pub target_futures_qty: f64,
    pub start_timestamp: i64,
    pub status: String, // "Running", "Completed", "Cancelled", "Failed", "Interrupted"
    pub spot_order_id: Option<String>,
    pub spot_filled_qty: f64,
    pub futures_order_id: Option<String>,
    pub futures_filled_qty: f64,
    pub end_timestamp: Option<i64>,
    pub error_message: Option<String>,
    pub unhedged_op_id: Option<i64>,
}
