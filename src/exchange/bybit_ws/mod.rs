// src/exchange/bybit_ws/mod.rs

use futures_util::{stream::{SplitSink, SplitStream}};
use tokio::net::TcpStream;
use tokio_tungstenite::{tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream};

pub(super) type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
pub(super) type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

pub(super) const CONNECT_TIMEOUT_SECONDS: u64 = 10;
pub(super) const READ_TIMEOUT_SECONDS: u64 = 60; // Pong timeout

mod connection;
mod protocol;
mod read_loop;
mod types_internal;

// Реэкспортируем основную публичную функцию для приватных стримов
pub use connection::connect_and_subscribe;
// Реэкспортируем новую публичную функцию для публичных стримов
pub use connection::connect_public_stream;