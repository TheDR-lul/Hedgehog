# Hedger Bot

Rust-based Telegram bot for automated spot-futures hedging on cryptocurrency exchanges (Bybit, optionally Bitget).

## Features

- Integration with Bybit API (and Bitget in later iterations)
- Secure configuration from file or environment variables
- Commands via Telegram:
  - `/status` — check bot and API connection status
  - `/hedge <sum> <symbol> <volatility>%` — execute hedging strategy
  - `/unhedge <sum> <symbol>` — execute unhedging (reverse) strategy
  - `/stats 30` / `/stats 180` — average funding rate over last 30/180 days
  - `/cancel` — cancel ongoing operation
- Modular architecture:
  - `exchange` module with `Exchange` trait for API operations
  - `hedger` module with core hedging / unhedging logic
  - `notifier` module for Telegram command parsing and notifications
  - `storage` module for PostgreSQL (via `sqlx`) to persist logs, orders, sessions
  - `utils` for helper functions (rounding, fee calculations)
  - `logger` setup via `tracing`
- Robust handling of limit orders: automatic cancellation and repositioning if unfilled
- Real-time logs and updates in Telegram

---

## Prerequisites

- Rust toolchain (Rust 1.64+)
- `cargo` build tool
- PostgreSQL database (or SQLite for prototyping)
- Telegram bot token
- API key and secret from Bybit (and Bitget if used)

---

## Configuration

Bot reads settings from:
1. **File**: `Config.toml` (default in working directory)
2. **Env var**: `HEDGER_CONFIG` to override file path
3. **Environment variables** prefixed with `HEDGER__`, e.g. `HEDGER__TELEGRAM_TOKEN`

### Example `Config.toml`

```toml
bybit_api_key    = "YOUR_BYBIT_API_KEY"
bybit_api_secret = "YOUR_BYBIT_API_SECRET"
db_url           = "postgres://user:password@localhost/hedger_db"
telegram_token   = "YOUR_TELEGRAM_BOT_TOKEN"
default_volatility = 0.6  # 60%
offset_points      = 10   # price offset for limit orders
```

---

## Building and Running

1. **Clone repository**
   ```bash
git clone https://github.com/TheDR-lul/Hedgehog
cd hedgerbot
```

2. **Configure**
   - Copy `Config.toml.example` to `Config.toml` and fill values, or
   - Set `HEDGER_CONFIG` or individual `HEDGER__*` environment variables

3. **Build**
   ```bash
cargo build --release
```

4. **Run**
   ```bash
./target/release/hedger_bot
```

Bot will print `Bybit API OK` on successful startup.

---

## Usage

Open Telegram and send commands to your bot:

| Command                         | Description                                                  |
|---------------------------------|--------------------------------------------------------------|
| `/status`                       | Check connection to bot and API                              |
| `/hedge 1000 USDT MNT 60`       | Hedge 1000 USDT on MNT with 60% volatility assumption        |
| `/unhedge 500 USDT MNT`         | Reverse hedge for 500 USDT on MNT                            |
| `/stats 30`                     | Get average funding rate for last 30 days                    |
| `/cancel`                       | Cancel ongoing hedging/unhedging operation                   |

---

## Project Structure

```
hedger_bot/
├── Config.toml                 # Default configuration file
├── Cargo.toml                  # Rust package manifest
├── src/
│   ├── main.rs                 # Entry point
│   ├── config.rs               # Configuration loading
│   ├── exchange/               # Exchange API abstractions and implementations
│   ├── hedger.rs               # Core hedging logic
│   ├── notifier.rs             # Telegram bot commands and notifications
│   ├── logger.rs               # Logging setup
│   ├── models.rs               # Domain models (request, orders, reports)
│   ├── storage/                # Database schema and operations
│   └── utils.rs                # Helper functions
└── migrations/                 # (Optional) database migrations
```

---

## Next Steps

- Implement full Bybit API calls and error handling
- Add Bitget support as second backend
- Extend test coverage with mocks for API and database
- (Optional) Dockerize for easy deployment

---

## License

MIT © Anton Kalantarenko

