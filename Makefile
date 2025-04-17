.PHONY: build run migrate test release

# Собрать в режиме отладки
build:
	cargo build

# Запустить бота
run:
	cargo run

# Применить миграции в SQLite
migrate:
	sqlite3 $(DB_PATH) < migrations/001_create_tables.sql

# Запустить тесты
test:
	cargo test

# Собрать релиз
release:
	cargo build --release
