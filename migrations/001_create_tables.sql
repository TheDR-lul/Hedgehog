-- Создаём таблицы для хранения сессий, ордеров и funding-rate

BEGIN;

CREATE TABLE IF NOT EXISTS sessions (
  id SERIAL PRIMARY KEY,
  chat_id BIGINT NOT NULL,
  symbol TEXT NOT NULL,
  sum NUMERIC NOT NULL,
  volatility NUMERIC NOT NULL,
  mmr NUMERIC,
  spot_qty NUMERIC,
  futures_qty NUMERIC,
  status TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS orders (
  id SERIAL PRIMARY KEY,
  session_id INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
  order_id TEXT NOT NULL,
  side TEXT NOT NULL,
  market TEXT NOT NULL,
  price NUMERIC,
  qty NUMERIC NOT NULL,
  status TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS funding_rates (
  id SERIAL PRIMARY KEY,
  symbol TEXT NOT NULL,
  rate NUMERIC NOT NULL,
  timestamp TIMESTAMPTZ NOT NULL
);

COMMIT;
