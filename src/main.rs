use std::{collections::HashMap, fs::File, str::FromStr, time::Duration};

use clap::Parser;
use isahc::prelude::*;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{Value, json};
use smol::future::FutureExt;
use sqlx::{
    Pool, Sqlite,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteQueryResult},
};
use teloxide::{
    prelude::*,
    types::{
        BotCommand, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MenuButton, Message, Seconds,
    },
    RequestError,
};
use futures_util::StreamExt;

// ---------------------------- Configuration ----------------------------
#[derive(Debug, Deserialize)]
struct Config {
    telegram_bot_token: String,
    vm_api_secret: String,
    giftcard_api_secret: String,
}

/// CLI wrapper (`-c <config.yaml>`) – parsed inside the lazy initializer
#[derive(Parser, Debug)]
struct Cli {
    /// Path to YAML config file
    #[arg(short, long)]
    config: String,
}

static CONFIG: Lazy<Config> = Lazy::new(|| {
    let cli = Cli::parse();
    serde_yaml::from_reader(File::open(&cli.config).expect("read config file"))
        .expect("parse config YAML")
});

// ---------------------------- Database ----------------------------
static DB: Lazy<Pool<Sqlite>> = Lazy::new(|| {
    smol::block_on(async {
        let opts = SqliteConnectOptions::from_str("sqlite://geph-testing-bot-store.db")
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS agent_records (
              vm_id TEXT PRIMARY KEY,
              telegram_chat_id INTEGER,
              up_secs INTEGER DEFAULT 0,
              notified_secs INTEGER DEFAULT 0,
              claimed_secs INTEGER DEFAULT 0
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    })
});

// ---------------------------- Entry ----------------------------

fn main() {
    env_logger::init();

    let bot = Bot::new(CONFIG.telegram_bot_token.clone());

    smolscale::block_on(async move {
        let commands = vec![
            BotCommand::new("register", "Register your VM. Usage: /register <id> / 注册您的 VM：/register <id>"),
            BotCommand::new("uptime", "Show your VM's total uptime / 查看 VM 总运行时间"),
            BotCommand::new("unclaimed", "View unclaimed Plus days / 查看未领取的 Plus 天数"),
            BotCommand::new("claim", "Claim accumulated Plus days / 领取累计的 Plus 天数"),
            BotCommand::new("deregister", "Deregister your VM / 取消注册 VM"),
            BotCommand::new("menu", "Show command menu / 显示命令菜单"),
        ];
        bot.set_my_commands(commands).await.unwrap();
        bot
            .set_chat_menu_button()
            .menu_button(MenuButton::Commands)
            .send()
            .await
            .unwrap();
        teloxide::repl(bot.clone(), handler)
            .race(async {
                update_uptime_loop().await.unwrap();
            })
            .race(async {
                notify_uptime_loop(bot).await.unwrap();
            })
            .await;
    })
}

// ---------------------------- Messages (English / 中文) ----------------------------
const THANKS_ALREADY_REGISTERED: &str = "Thank you for running a testing VM! Your VM is already registered with us. We will send you a 1-day Plus giftcard in this chat for every 24 hours your VM is online. / 感谢您运行测试 VM！您的 VM 已经注册成功。每当您的 VM 总在线时间达到 24 小时，我们将在此聊天中向您发送一天的 Plus 礼品卡！";

const REGISTER_SUCCESS: &str = "Your VM has been successfully registered! We will send you a 1-day Plus giftcard in this chat for every 24 hours your VM is online. / 您的测试 VM 已成功注册！每当您的 VM 总在线时间达到 24 小时，我们将在此聊天中向您发送一天的 Plus 礼品卡！";

const GREETING: &str = "Hey there!

To register your testing VM to receive Plus, send us your VM ID without any other words or spaces in the text. Make sure your VM is running when you register. / 嗨！若要注册您的测试 VM 并领取 Plus，请直接发送您的 VM ID，不要包含其他单词或空格！请确保在注册时您的 VM 正在运行。";

const INVALID_VM: &str = "What you gave me is not a valid VM ID - please double check and make sure your text doesn't contain any other words or whitespace! / 您给我的不是有效的虚拟机 ID - 请再次检查并确保你的文本没有包含其他单词或空格！";

const THANKS_FOR_DAY: &str = "Thank you for keeping your testing VM online for 24 hours! You've earned another day of Geph Plus. Use /claim to redeem your days. / 感谢您让测试 VM 在线 24 小时！您又获得了一天的 Geph Plus。使用 /claim 兑换您的天数。";

#[derive(Clone, Debug)]
enum Command {
    Register(String),
    Uptime,
    Unclaimed,
    Claim,
    Deregister,
    Menu,
}

fn parse_command(text: &str) -> Option<Command> {
    let mut words = text.trim().split_whitespace();
    let first = words.next()?;
    // Allow an optional leading mention like "@BotName"
    let cmd = if first.starts_with('/') { first } else if first.starts_with('@') {
        words.next()?
    } else {
        return None;
    };
    match cmd {
        "/register" => words.next().map(|id| Command::Register(id.to_owned())),
        "/uptime" => Some(Command::Uptime),
        "/unclaimed" => Some(Command::Unclaimed),
        "/claim" => Some(Command::Claim),
        "/deregister" => Some(Command::Deregister),
        "/menu" => Some(Command::Menu),
        _ => None,
    }
}

fn menu_markup(registered: bool) -> InlineKeyboardMarkup {
    if registered {
        InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::switch_inline_query_current_chat(
                "My VM's total uptime / 我的 VM 总运行时间",
                "/uptime",
            )],
            vec![InlineKeyboardButton::switch_inline_query_current_chat(
                "View unclaimed Plus days / 查看未领取的 Plus 天数",
                "/unclaimed",
            )],
            vec![InlineKeyboardButton::switch_inline_query_current_chat(
                "Claim Plus / 领取 Plus",
                "/claim",
            )],
            vec![InlineKeyboardButton::switch_inline_query_current_chat(
                "Deregister VM / 取消注册 VM",
                "/deregister",
            )],
        ])
    } else {
        InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::switch_inline_query_current_chat(
                "Register VM / 注册 VM",
                "/register ",
            ),
        ]])
    }
}

async fn send_menu(bot: &Bot, chat_id: ChatId, registered: bool) -> Result<(), RequestError> {
    bot.send_message(chat_id, "Choose a command from the menu below: / 请从下面的菜单选择命令：")
        .reply_markup(menu_markup(registered))
        .await?;
    Ok(())
}

// ---------------------------- Telegram handler ----------------------------
async fn handler(bot: Bot, msg: Message) -> Result<(), RequestError> {
    let Some(text) = msg.text() else { return Ok(()); };
    let chat_id = msg.chat.id;

    log::debug!("received message w/ text={text}");

    let registered = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM agent_records WHERE telegram_chat_id = ?)",
    )
    .bind(chat_id.0)
    .fetch_one(&*DB)
    .await
    .map_err(|_| RequestError::RetryAfter(Seconds::from_seconds(5)))?;

    if text == "/start" || text == "/menu" {
        send_menu(&bot, chat_id, registered).await?;
        return Ok(());
    }

    match parse_command(text) {
        Some(Command::Register(vm_id)) => {
            if registered {
                bot.send_message(chat_id, THANKS_ALREADY_REGISTERED).await?;
            } else {
                let result: SqliteQueryResult = sqlx::query(
                    "UPDATE agent_records SET telegram_chat_id = $1 WHERE vm_id = $2 AND telegram_chat_id IS NULL",
                )
                .bind(chat_id.0)
                .bind(vm_id)
                .execute(&*DB)
                .await
                .unwrap();
                if result.rows_affected() > 0 {
                    bot.send_message(chat_id, REGISTER_SUCCESS).await?;
                    send_menu(&bot, chat_id, true).await?;
                } else {
                    bot.send_message(chat_id, INVALID_VM).await?;
                }
            }
        }
        Some(Command::Uptime) => {
            if registered {
                let secs: i64 = sqlx::query_scalar(
                    "SELECT up_secs FROM agent_records WHERE telegram_chat_id = ?",
                )
                .bind(chat_id.0)
                .fetch_one(&*DB)
                .await
                .unwrap();
                let hours = secs / 3600;
                bot.send_message(chat_id, format!("Your VM has been up for {hours} hours. / 您的 VM 已经运行了 {hours} 小时。")).await?;
            } else {
                bot.send_message(chat_id, GREETING).await?;
            }
        }
        Some(Command::Unclaimed) => {
            if registered {
                let days: i64 = sqlx::query_scalar(
                    "SELECT (notified_secs - claimed_secs) / 86400 FROM agent_records WHERE telegram_chat_id = ?",
                )
                .bind(chat_id.0)
                .fetch_one(&*DB)
                .await
                .unwrap();
                bot.send_message(chat_id, format!("Unclaimed Plus days: {days} / 未领取的 Plus 天数：{days}")).await?;
            } else {
                bot.send_message(chat_id, GREETING).await?;
            }
        }
        Some(Command::Claim) => {
            if registered {
                let days: i64 = sqlx::query_scalar(
                    "SELECT (notified_secs - claimed_secs) / 86400 FROM agent_records WHERE telegram_chat_id = ?",
                )
                .bind(chat_id.0)
                .fetch_one(&*DB)
                .await
                .unwrap();
                if days > 0 {
                    let body = json!({
                        "days_per_card": days,
                        "num_cards": 1,
                        "secret": CONFIG.giftcard_api_secret
                    });
                    let giftcard = isahc::Request::post(
                        "https://web-backend.geph.io/support/create-giftcards",
                    )
                    .header(isahc::http::header::CONTENT_TYPE, "application/json")
                    .body(body.to_string())
                    .unwrap()
                    .send()
                    .unwrap()
                    .text()
                    .unwrap();
                    bot.send_message(chat_id, giftcard).await?;
                    sqlx::query(
                        "UPDATE agent_records SET claimed_secs = claimed_secs + $1 WHERE telegram_chat_id = $2;",
                    )
                    .bind(days * 86400)
                    .bind(chat_id.0)
                    .execute(&*DB)
                    .await
                    .unwrap();
                } else {
                    bot.send_message(chat_id, "No unclaimed days yet. / 还没有未领取的天数。").await?;
                }
            } else {
                bot.send_message(chat_id, GREETING).await?;
            }
        }
        Some(Command::Deregister) => {
            if registered {
                sqlx::query(
                    "UPDATE agent_records SET telegram_chat_id = NULL WHERE telegram_chat_id = ?",
                )
                .bind(chat_id.0)
                .execute(&*DB)
                .await
                .unwrap();
                bot.send_message(chat_id, "Your VM has been deregistered. / 您的 VM 已取消注册。").await?;
            } else {
                bot.send_message(chat_id, GREETING).await?;
            }
        }
        Some(Command::Menu) => {
            send_menu(&bot, chat_id, registered).await?;
        }
        None => {
            if registered {
                send_menu(&bot, chat_id, true).await?;
            } else {
                bot.send_message(chat_id, GREETING).await?;
            }
        }
    }
    Ok(())
}

// ---------------------------- Background loops ----------------------------
async fn update_uptime_loop() -> anyhow::Result<()> {
    let mut ticker = smol::Timer::interval(Duration::from_secs(60));
    loop {
        let url = format!(
            "http://104.194.80.160:3000/available_vms?secret={}",
            CONFIG.vm_api_secret
        );
        let resp_body = isahc::get(url)?.text()?;
        let map: HashMap<String, Value> = serde_json::from_str(&resp_body)?;

        for vm_id in map.keys() {
            log::debug!("updating up_secs for vm_id = {vm_id}");
            sqlx::query(
                r#"
INSERT INTO agent_records (
    vm_id,
    telegram_chat_id,
    up_secs,
    notified_secs,
    claimed_secs
)
VALUES ($1, NULL, 60, 0, 0)
ON CONFLICT(vm_id) DO UPDATE SET
    up_secs = agent_records.up_secs + 60;
            "#,
            )
            .bind(vm_id)
            .execute(&*DB)
            .await?;
        }
        ticker.next().await;
    }
}

async fn notify_uptime_loop(bot: Bot) -> anyhow::Result<()> {
    let mut ticker = smol::Timer::interval(Duration::from_secs(60));
    loop {
        let notifications: Vec<(i64, i64)> = sqlx::query_as(
            r#"
SELECT telegram_chat_id, (up_secs - notified_secs) / 86400 AS new_days
FROM agent_records
WHERE telegram_chat_id IS NOT NULL
  AND (up_secs - notified_secs) >= 86400
            "#,
        )
        .fetch_all(&*DB)
        .await?;

        for (chat_id, new_days) in notifications {
            for _ in 0..new_days {
                bot.send_message(ChatId(chat_id), THANKS_FOR_DAY).await?;
            }

            sqlx::query(
                "UPDATE agent_records SET notified_secs = notified_secs + $1 WHERE telegram_chat_id = $2;",
            )
            .bind(new_days * 86400)
            .bind(chat_id)
            .execute(&*DB)
            .await?;
        }

        ticker.next().await;
    }
}
