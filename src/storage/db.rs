// src/storage/db.rs

//! Функции для взаимодействия с базой данных SQLite.

use super::schema::{apply_migrations, HedgeOperation}; // Импортируем структуру
use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{Error as SqlxError}; // Переименовываем, чтобы не конфликтовать с anyhow::Error
use std::str::FromStr;
use tracing::{error, info, warn};
use std::time::{SystemTime, UNIX_EPOCH};

// Переопределяем Db как SqlitePool для простоты
pub type Db = SqlitePool;

/// Асинхронная функция для подключения к базе данных SQLite.
pub async fn connect(db_path: &str) -> Result<Db> {
    info!("Connecting to database: {}", db_path);
    let options = SqliteConnectOptions::from_str(db_path)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal); // WAL для лучшей производительности

    let pool = SqlitePoolOptions::new()
        .max_connections(5) // Настройте по необходимости
        .connect_with(options)
        .await
        .context(format!("Failed to connect to database at {}", db_path))?;

    // Применяем миграции при подключении
    apply_migrations(&pool).await?;

    info!("Database connection pool established.");
    Ok(pool)
}

/// Получить текущее время как Unix timestamp (секунды)
fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// --- Функции для работы с hedge_operations ---

/// Вставить новую операцию хеджирования и вернуть ее ID.
pub async fn insert_hedge_operation(
    db: &Db,
    chat_id: i64,
    base_symbol: &str,
    quote_currency: &str,
    initial_sum: f64,
    volatility: f64,
    target_spot_qty: f64,
    target_futures_qty: f64,
) -> Result<i64, SqlxError> {
    let ts = current_timestamp();
    let status = "Running"; // Начальный статус

    let result = sqlx::query!(
        r#"
        INSERT INTO hedge_operations (
            chat_id, base_symbol, quote_currency, initial_sum, volatility,
            target_spot_qty, target_futures_qty, start_timestamp, status
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        chat_id,
        base_symbol,
        quote_currency,
        initial_sum,
        volatility,
        target_spot_qty,
        target_futures_qty,
        ts,
        status
    )
    .execute(db)
    .await?;

    Ok(result.last_insert_rowid())
}

/// Обновить ID спотового ордера и исполненное количество для операции.
pub async fn update_hedge_spot_order(
    db: &Db,
    operation_id: i64,
    spot_order_id: Option<&str>, // Передаем Option<&str>
    spot_filled_qty: f64,
) -> Result<(), SqlxError> {
    sqlx::query!(
        r#"
        UPDATE hedge_operations
        SET spot_order_id = ?, spot_filled_qty = ?
        WHERE id = ?
        "#,
        spot_order_id, // sqlx умеет работать с Option<String> или Option<&str>
        spot_filled_qty,
        operation_id
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Обновить финальный статус, ID фьючерсного ордера и другие поля при завершении/ошибке/отмене.
pub async fn update_hedge_final_status(
    db: &Db,
    operation_id: i64,
    status: &str, // "Completed", "Cancelled", "Failed"
    futures_order_id: Option<&str>,
    futures_filled_qty: f64,
    error_message: Option<&str>,
) -> Result<(), SqlxError> {
    let ts = current_timestamp();
    sqlx::query!(
        r#"
        UPDATE hedge_operations
        SET status = ?,
            futures_order_id = ?,
            futures_filled_qty = ?,
            end_timestamp = ?,
            error_message = ?
        WHERE id = ? AND status = 'Running' -- Обновляем только если еще 'Running'
        "#,
        status,
        futures_order_id,
        futures_filled_qty,
        ts,
        error_message,
        operation_id
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Получить все операции хеджирования в статусе 'Running'.
pub async fn get_running_hedge_operations(db: &Db) -> Result<Vec<HedgeOperation>, SqlxError> {
    sqlx::query_as!(
        HedgeOperation,
        r#"
        SELECT
            id, chat_id, base_symbol, quote_currency, initial_sum, volatility,
            target_spot_qty, target_futures_qty, start_timestamp, status,
            spot_order_id, spot_filled_qty, futures_order_id, futures_filled_qty,
            end_timestamp, error_message, unhedged_op_id
        FROM hedge_operations
        WHERE status = 'Running'
        ORDER BY start_timestamp ASC
        "#
    )
    .fetch_all(db)
    .await
}

/// Получить операцию хеджирования по ID.
pub async fn get_hedge_operation_by_id(db: &Db, operation_id: i64) -> Result<Option<HedgeOperation>, SqlxError> {
     sqlx::query_as!(
        HedgeOperation,
        r#"
        SELECT
            id, chat_id, base_symbol, quote_currency, initial_sum, volatility,
            target_spot_qty, target_futures_qty, start_timestamp, status,
            spot_order_id, spot_filled_qty, futures_order_id, futures_filled_qty,
            end_timestamp, error_message, unhedged_op_id
        FROM hedge_operations
        WHERE id = ?
        "#,
        operation_id
    )
    .fetch_optional(db) // Используем fetch_optional, так как запись может не существовать
    .await
}

/// Получить список завершенных (Completed) и еще не расхеджированных операций для пользователя и символа.
pub async fn get_completed_unhedged_ops_for_symbol(
    db: &Db,
    chat_id: i64,
    base_symbol: &str,
) -> Result<Vec<HedgeOperation>, SqlxError> {
    sqlx::query_as!(
        HedgeOperation,
        r#"
        SELECT
            id, chat_id, base_symbol, quote_currency, initial_sum, volatility,
            target_spot_qty, target_futures_qty, start_timestamp, status,
            spot_order_id, spot_filled_qty, futures_order_id, futures_filled_qty,
            end_timestamp, error_message, unhedged_op_id
        FROM hedge_operations
        WHERE chat_id = ?
          AND base_symbol = ?
          AND status = 'Completed'
          AND unhedged_op_id IS NULL -- Ищем те, что еще не расхеджированы
        ORDER BY end_timestamp DESC -- Сначала самые свежие
        "#,
        chat_id,
        base_symbol
    )
    .fetch_all(db)
    .await
}

/// Пометить операцию хеджирования как расхеджированную (установить unhedged_op_id).
pub async fn mark_hedge_as_unhedged(
    db: &Db,
    hedge_operation_id: i64,
    unhedge_operation_id: i64, // ID записи в таблице unhedge_operations (если она есть)
) -> Result<(), SqlxError> {
    sqlx::query!(
        r#"
        UPDATE hedge_operations
        SET unhedged_op_id = ?
        WHERE id = ? AND status = 'Completed' AND unhedged_op_id IS NULL
        "#,
        unhedge_operation_id,
        hedge_operation_id
    )
    .execute(db)
    .await?;
    Ok(())
}

// TODO: Добавить функции для работы с unhedge_operations, если нужно
