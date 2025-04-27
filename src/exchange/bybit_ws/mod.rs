// src/exchange/bybit_ws/mod.rs

// Импортируем нужные базовые типы
use futures_util::{stream::{SplitSink, SplitStream}};
use tokio::net::TcpStream;
use tokio_tungstenite::{tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream};

// --- ДОБАВЛЕНО: Определяем общие типы и константы ---
// Делаем pub(super), чтобы они были видны в connection.rs, protocol.rs, read_loop.rs
pub(super) type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
pub(super) type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

pub(super) const CONNECT_TIMEOUT_SECONDS: u64 = 10;
pub(super) const READ_TIMEOUT_SECONDS: u64 = 60;
// --- КОНЕЦ ДОБАВЛЕНИЙ ---

// Объявляем внутренние подмодули
mod connection;
mod protocol;
mod read_loop;
mod types_internal;

// Реэкспортируем основную публичную функцию
pub use connection::connect_and_subscribe;