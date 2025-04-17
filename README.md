# Hedgehog

Rust-based Telegram bot for automated spot-futures hedging on cryptocurrency exchanges (Bybit, optionally Bitget).

## Features

- Integration with Bybit API (and Bitget in later iterations)
- Secure configuration from file or environment variables
- Local all-in-one SQLite database (no external DB server)
- Commands via Telegram:
  - `/status` — check bot and API connection status
  - `/hedge <sum> <symbol> <volatility>%` — execute hedging strategy
  - `/unhedge <sum> <symbol>` — execute unhedging (reverse) strategy
  - `/stats 30` / `/stats 180` — average funding rate over last 30/180 days
  - `/cancel` — cancel ongoing operation
- Modular architecture:
  - `exchange` module with `Exchange` trait for API operations
  - `hedger` module with core hedging/unhedging logic
  - `notifier` module for Telegram command parsing and notifications
  - `storage` module for SQLite (via `sqlx`) to persist logs, orders, sessions
  - `utils` for helper functions (rounding, fee calculations)
  - `logger` setup via `tracing`
  - `telegram` module to run the bot
- Robust handling of limit orders: automatic cancellation and repositioning if unfilled
- Real-time logs and updates in Telegram

---

## Prerequisites

- Rust toolchain (Rust 1.75+ or nightly)
- `cargo` build tool
- SQLite3 installed
- Telegram bot token
- API key and secret from Bybit (and Bitget if used)

---

## Configuration

Bot reads settings from:
1. **File**: `Config.toml` (by default copied from `Config.toml.example`)
2. **Env var**: `HEDGER_CONFIG` to override file path
3. **Environment variables** prefixed with `HEDGER__`, e.g. `HEDGER__TELEGRAM_TOKEN`

### Example `Config.toml`

```toml
bybit_api_key    = "YOUR_BYBIT_API_KEY"
bybit_api_secret = "YOUR_BYBIT_API_SECRET"
sqlite_path      = "data/hedgehog.db"
telegram_token   = "YOUR_TELEGRAM_BOT_TOKEN"
default_volatility = 0.6  # 60%
offset_points      = 10   # price offset for limit orders
```

---

## Building and Running

1. **Clone repository**
   ```bash
git clone https://github.com/your-repo/Hedgehog.git
cd Hedgehog
```

2. **Configure**
   - Copy `Config.toml.example` to `Config.toml` and fill values, or
   - Set `HEDGER_CONFIG` or individual `HEDGER__*` environment variables

3. **Migrate database**
   ```bash
make migrate DB_PATH=data/hedgehog.db
```

4. **Build**
   ```bash
make build
```

5. **Run**
   ```bash
make run
```

Bot will print `Bybit API OK` on successful startup.

---

## Usage

Open Telegram and send commands:
| Command               | Description                                 |
|-----------------------|---------------------------------------------|
| `/status`             | Check bot & API status                      |
| `/hedge 1000 USDT MNT 60` | Hedge 1000 USDT on MNT with 60% volatility |
| `/unhedge 500 USDT MNT`   | Reverse hedge for 500 USDT on MNT          |
| `/stats 30`           | Avg funding rate last 30 days               |
| `/cancel`             | Cancel ongoing operation                    |
