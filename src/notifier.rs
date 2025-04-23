use anyhow::Result;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, CallbackQuery, ChatId},
    utils::command::BotCommands,
};
use crate::exchange::Exchange;
use crate::hedger::Hedger;
use crate::models::{HedgeRequest, UnhedgeRequest};
use std::sync::Arc;
use tokio::sync::RwLock;
use std::collections::HashMap;

/// Все команды бота
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Доступные команды:")]
pub enum Command {
    #[command(description = "показать это сообщение", aliases = ["help", "?"])]
    Help,
    #[command(description = "проверить статус")]
    Status,
    #[command(description = "список всего баланса: /wallet")]
    Wallet,
    #[command(description = "баланс монеты: /balance <symbol>")]
    Balance(String),
    #[command(description = "захеджировать: /hedge <sum> <symbol> <volatility %>")]
    Hedge(String),
    #[command(description = "расхеджировать: /unhedge <sum> <symbol>")]
    Unhedge(String),
    #[command(description = "средняя ставка финансирования: /funding <symbol> [days]")]
    Funding(String),
}

/// Состояния пользователя
#[derive(Debug, Clone)]
pub enum UserState {
    AwaitingAssetSelection, // Ожидание выбора актива
    AwaitingSum { symbol: String }, // Ожидание ввода суммы
    AwaitingVolatility { symbol: String, sum: f64 }, // Ожидание ввода волатильности
    None, // Нет активного диалога
}

// Тип для хранения состояний пользователей
pub type StateStorage = Arc<RwLock<HashMap<ChatId, UserState>>>;

/// Обработка текстовых команд
pub async fn handle_command<E>(
    bot: Bot,
    msg: Message,
    cmd: Command,
    exchange: E,
    state_storage: StateStorage,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    match cmd {
        Command::Help => {
            let kb = InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("✅ Статус", "status"),
                    InlineKeyboardButton::callback("💼 Баланс", "wallet"),
                ],
                vec![
                    InlineKeyboardButton::callback("🪙 Баланс монеты", "balance"),
                    InlineKeyboardButton::callback("⚙️ Хедж", "hedge"),
                    InlineKeyboardButton::callback("🛠 Расхедж", "unhedge"),
                    InlineKeyboardButton::callback("📈 Funding", "funding"),
                ],
            ]);
            bot.send_message(chat_id, Command::descriptions().to_string())
                .reply_markup(kb)
                .await?;
        }
        Command::Status => {
            bot.send_message(chat_id, "✅ Бот запущен и подключён к бирже").await?;
        }
        Command::Wallet => {
            let list = exchange.get_all_balances().await?;
            let mut text = "💼 Баланс кошелька:\n".to_string();
            for (c, b) in list.into_iter() {
                if b.free > 0.0 || b.locked > 0.0 {
                    text.push_str(&format!("• {}: free={:.4}, locked={:.4}\n", c, b.free, b.locked));
                }
            }
            bot.send_message(chat_id, text).await?;
        }
        Command::Balance(arg) => {
            let sym = arg.trim().to_uppercase();
            if sym.is_empty() {
                bot.send_message(chat_id, "Использование: /balance <symbol>").await?;
            } else {
                match exchange.get_balance(&sym).await {
                    Ok(b) => {
                        bot.send_message(
                            chat_id,
                            format!("💰 {}: free={:.4}, locked={:.4}", sym, b.free, b.locked),
                        )
                        .await?;
                    }
                    Err(_) => {
                        bot.send_message(chat_id, format!("❌ Баланса {} нет", sym))
                            .await?;
                    }
                }
            }
        }
        Command::Hedge(_) | Command::Unhedge(_) => {
            bot.send_message(chat_id, "Используйте кнопки для хеджирования или расхеджирования.")
                .await?;
        }
        Command::Funding(arg) => {
            let parts: Vec<_> = arg.split_whitespace().collect();
            if parts.is_empty() {
                bot.send_message(chat_id, "Использование: /funding <symbol> [days]").await?;
            } else {
                let sym = parts[0].to_uppercase();
                let days = parts.get(1).and_then(|d| d.parse().ok()).unwrap_or(30);
                match exchange.get_funding_rate(&sym, days).await {
                    Ok(r) => {
                        bot.send_message(
                            chat_id,
                            format!(
                                "Средняя ставка финансирования {} за {} дн: {:.4}%",
                                sym,
                                days,
                                r * 100.0
                            ),
                        )
                        .await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("❌ Ошибка funding: {}", e))
                            .await?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Обработка inline‑callback событий
pub async fn handle_callback<E>(
    bot: Bot,
    q: CallbackQuery,
    exchange: E,
    state_storage: StateStorage,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    if let Some(data) = q.data {
        let message = q.message.as_ref().unwrap();
        let chat_id = message.chat().id;
        let message_id = message.id(); // Исправлен вызов метода

        // Исправление: добавлен .await и убран expect
        let mut state = state_storage.write().await;
        
        match data.as_str() {
            "status" => {
                bot.edit_message_text(chat_id, message_id, "✅ Бот запущен и подключён к бирже")
                    .await?;
            }
            "wallet" => {
                let list = exchange.get_all_balances().await?;
                let mut text = "💼 Баланс кошелька:\n".to_string();
                for (coin, bal) in list {
                    if bal.free > 0.0 || bal.locked > 0.0 {
                        text.push_str(&format!(
                            "• {}: free={:.4}, locked={:.4}\n",
                            coin, bal.free, bal.locked
                        ));
                    }
                }
                bot.edit_message_text(chat_id, message_id, text).await?;
            }
            "balance" => {
                bot.edit_message_text(chat_id, message_id, "Введите: /balance <symbol>")
                    .await?;
            }
            "hedge" => {
                let list = exchange.get_all_balances().await?;
                let mut buttons = vec![];
                for (coin, bal) in list {
                    if bal.free > 0.0 || bal.locked > 0.0 {
                        buttons.push(vec![
                            InlineKeyboardButton::callback(
                                format!("🪙 {} (free: {:.4}, locked: {:.4})", coin, bal.free, bal.locked),
                                format!("hedge_{}", coin),
                            ),
                        ]);
                    }
                }
                // Добавляем кнопку отмены
                buttons.push(vec![
                    InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge"),
                ]);
                let kb = InlineKeyboardMarkup::new(buttons);
                bot.edit_message_text(chat_id, message_id, "Выберите актив для хеджирования:")
                    .reply_markup(kb)
                    .await?;
                state.insert(chat_id, UserState::AwaitingAssetSelection);
            }
            "cancel_hedge" => {
                state.insert(chat_id, UserState::None);
                let kb = InlineKeyboardMarkup::new(vec![
                    vec![
                        InlineKeyboardButton::callback("✅ Статус", "status"),
                        InlineKeyboardButton::callback("💼 Баланс", "wallet"),
                    ],
                    vec![
                        InlineKeyboardButton::callback("🪙 Баланс монеты", "balance"),
                        InlineKeyboardButton::callback("⚙️ Хедж", "hedge"),
                        InlineKeyboardButton::callback("🛠 Расхедж", "unhedge"),
                        InlineKeyboardButton::callback("📈 Funding", "funding"),
                    ],
                ]);
                bot.edit_message_text(chat_id, message_id, "Действие отменено.")
                    .reply_markup(kb)
                    .await?;
            }
            _ if data.starts_with("hedge_") => {
                let sym = data.trim_start_matches("hedge_");
                state.insert(chat_id, UserState::AwaitingSum { symbol: sym.to_string() });
                let kb = InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge")],
                ]);
                bot.edit_message_text(chat_id, message_id, format!("Введите сумму для хеджирования {}:", sym))
                    .reply_markup(kb)
                    .await?;
            }
            _ => {}
        }
        bot.answer_callback_query(q.id).await?;
    }
    Ok(())
}

/// Обработка текстовых сообщений
pub async fn handle_message<E>(
    bot: Bot,
    msg: Message,
    state_storage: StateStorage,
    exchange: E,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let chat_id = msg.chat.id;
    let message_id = msg.id;
    let text = msg.text().unwrap_or("").trim();

    // Убираем `.await` и используем `.expect`
    let mut state = state_storage.write().await;
    
    if let Some(user_state) = state.get_mut(&chat_id) {
        match user_state.clone() {
            UserState::AwaitingSum { symbol } => {
                if let Ok(sum) = text.parse::<f64>() {
                    *user_state = UserState::AwaitingVolatility { symbol: symbol.clone(), sum };
                    let kb = InlineKeyboardMarkup::new(vec![
                        vec![InlineKeyboardButton::callback("❌ Отмена", "cancel_hedge")],
                    ]);
                    bot.delete_message(chat_id, message_id).await?; // Удаляем сообщение пользователя
                    bot.edit_message_text(
                        chat_id,
                        message_id, // ID сообщения бота, которое нужно обновить
                        format!("Введите волатильность для хеджирования {} (%):", symbol),
                    )
                    .reply_markup(kb)
                    .await?;
                } else {
                    bot.send_message(chat_id, "Неверный формат суммы. Введите число.")
                        .await?;
                }
            }
            UserState::AwaitingVolatility { symbol, sum } => {
                if let Ok(vol) = text.trim_end_matches('%').parse::<f64>() {
                    let vol = vol / 100.0;
                    *user_state = UserState::None; // Сбрасываем состояние
                    bot.delete_message(chat_id, message_id).await?; // Удаляем сообщение пользователя
                    // Вызов функции хеджирования
                    do_hedge(&bot, chat_id, format!("{} {} {:.2}", sum, symbol, vol), &exchange).await?;
                } else {
                    bot.send_message(chat_id, "Неверный формат волатильности. Введите число (%).")
                        .await?;
                }
            }
            _ => {}
        }
    } else {
        bot.send_message(chat_id, "Сейчас нет активного диалога. Используйте меню.").await?;
    }
    Ok(())
}

async fn do_hedge<E>(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    exchange: &E,
) -> Result<()>
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let parts: Vec<_> = args.split_whitespace().collect();
    if parts.len() != 3 {
        bot.send_message(chat_id, "Использование: /hedge <sum> <symbol> <volatility %>")
            .await?;
        return Ok(());
    }
    let sum: f64 = parts[0].parse().unwrap_or(0.0);
    let sym = parts[1].to_uppercase();
    let vol = parts[2].trim_end_matches('%').parse::<f64>().unwrap_or(0.0) / 100.0;
    // slippage 0.5%, commission 0.1%
    let hedger = Hedger::new(exchange.clone(), 0.005, 0.001);
    match hedger.run_hedge(HedgeRequest { sum, symbol: sym.clone(), volatility: vol }).await {
        Ok((spot, fut)) => {
            bot.send_message(
                chat_id,
                format!(
                    "Хеджирование {} USDT {} при V={:.1}%:
▸ Спот {:+.4}
▸ Фьючерс {:+.4}",
                    sum,
                    sym,
                    vol * 100.0,
                    spot,
                    fut,
                ),
            )
            .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Ошибка: {}", e)).await?;
        }
    }
    Ok(())
}

async fn do_unhedge<E>(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    exchange: &E, // Используем переданный exchange
) -> Result<()> 
where
    E: Exchange + Clone + Send + Sync + 'static,
{
    let parts: Vec<_> = args.split_whitespace().collect();
    if parts.len() != 2 {
        bot.send_message(chat_id, "Использование: /unhedge <sum> <symbol>").await?;
        return Ok(());
    }
    let sum: f64 = parts[0].parse().unwrap_or(0.0);
    let sym = parts[1].to_uppercase();
    // slippage 0.5%, commission 0.1%
    let hedger = Hedger::new(exchange.clone(), 0.005, 0.001); // Используем переданный exchange
    match hedger.run_unhedge(UnhedgeRequest { sum, symbol: sym.clone() }).await {
        Ok((sold, bought)) => {
            bot.send_message(
                chat_id,
                format!(
                    "Расхеджирование {} USDT {}:
▸ Продано спота {:+.4}
▸ Куплено фьюча {:+.4}",
                    sum,
                    sym,
                    sold,
                    bought
                ),
            )
            .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Ошибка unhedge: {}", e)).await?;
        }
    }
    Ok(())
}