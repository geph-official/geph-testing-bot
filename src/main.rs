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
            BotCommand::new("register", "Register your VM. Usage: /register <id>"),
            BotCommand::new("uptime", "Show your VM's total uptime"),
            BotCommand::new("unclaimed", "View unclaimed Plus days"),
            BotCommand::new("claim", "Claim accumulated Plus days"),
            BotCommand::new("deregister", "Deregister your VM"),
            BotCommand::new("menu", "Show command menu"),
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

// ---------------------------- Multilingual messages ----------------------------
const THANKS_ALREADY_REGISTERED: &str = "Thank you for running a testing VM! Your VM is already registered with us. We will send you a 1‑day Plus giftcard in this chat for every 24 hours of total time your VM is online!\n\n感谢您运行测试 VM！您的 VM 已经注册成功。每当您的 VM 总在线时间达到 24 小时，我们将在此聊天中向您发送一天的 Plus 礼品卡！\n\nاز اینکه ماشین مجازی آزمایشی را اجرا می‌کنید متشکریم! ماشین مجازی شما قبلاً ثبت شده است. به ازای هر ۲۴ ساعت آنلاین بودن، یک کارت هدیه یک روزه Plus در این چت برای شما ارسال می‌کنیم!\n\nСпасибо, что запустили тестовую ВМ! Ваша ВМ уже зарегистрирована. За каждые 24 часа работы мы будем отправлять вам подарочную карту Plus на один день в этом чате!";

const REGISTER_SUCCESS: &str = "Your VM has been successfully registered! We will send you a 1‑day Plus giftcard in this chat for every 24 hours of total time your VM is online!\n\n您的测试 VM 已成功注册！每当您的 VM 总在线时间达到 24 小时，我们将在此聊天中向您发送一天的 Plus 礼品卡！\n\nماشین مجازی شما با موفقیت ثبت شد! به ازای هر ۲۴ ساعت آنلاین بودن، یک کارت هدیه یک روزه Plus در این چت برای شما ارسال خواهیم کرد!\n\nВаша тестовая ВМ успешно зарегистрирована! За каждые 24 часа работы мы будем отправлять вам подарочную карту Plus на один день в этом чате!";

const GREETING: &str = "Hey there!\n\nTo register your testing VM to receive Plus, send us your VM ID without any other words or spaces in the text!\n\nMake sure your VM is running when you register.\n嗨！\n\n若要注册您的测试 VM 并领取 Plus，请直接发送您的 VM ID，不要包含其他单词或空格！\n\n请确保在注册时您的 VM 正在运行。\nسلام!\n\nبرای ثبت ماشین مجازی آزمایشی و دریافت Plus، شناسه VM خود را بدون هیچ کلمه یا فاصله اضافی برای ما ارسال کنید!\n\nاطمینان حاصل کنید که هنگام ثبت، ماشین مجازی شما در حال اجرا باشد.\nПривет!\n\nЧтобы зарегистрировать тестовую ВМ и получать Plus, отправьте нам идентификатор ВМ без каких‑либо других слов или пробелов!\n\nУбедитесь, что ваша ВМ запущена во время регистрации.";

const INVALID_VM: &str = "What you gave me is not a valid VM ID - please double check and make sure your text doesn't contain any other words or whitespace!\n\n您给我的不是有效的虚拟机 ID - 请再次检查并确保你的文本没有包含其他单词或空格！\n\nچیزی که به من دادید شناسه VM معتبر نیست - لطفاً دوباره بررسی کنید و مطمئن شوید که متن شما هیچ کلمه یا فاصله اضافی ندارد!\n\nТо, что вы мне дали, не является действительным идентификатором виртуальной машины - пожалуйста, перепроверьте и убедитесь, что в вашем тексте нет других слов или пробелов!";

const THANKS_FOR_DAY: &str = "Thank you for keeping your testing VM online for 24 hours! You've earned another day of Geph Plus. Use /claim to redeem your days.";

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
    let mut parts = text.trim().split_whitespace();
    let cmd = parts.next()?;
    match cmd {
        "/register" => parts.next().map(|id| Command::Register(id.to_owned())),
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
                "My VM's total uptime",
                "/uptime",
            )],
            vec![InlineKeyboardButton::switch_inline_query_current_chat(
                "View unclaimed Plus days",
                "/unclaimed",
            )],
            vec![InlineKeyboardButton::switch_inline_query_current_chat(
                "Claim Plus",
                "/claim",
            )],
            vec![InlineKeyboardButton::switch_inline_query_current_chat(
                "Deregister VM",
                "/deregister",
            )],
        ])
    } else {
        InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::switch_inline_query_current_chat(
                "Register VM",
                "/register ",
            ),
        ]])
    }
}

async fn send_menu(bot: &Bot, chat_id: ChatId, registered: bool) -> Result<(), RequestError> {
    bot.send_message(chat_id, "Choose a command from the menu below:")
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
                    "UPDATE agent_records SET telegram_chat_id = $1 WHERE vm_id = $2",
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
                bot.send_message(chat_id, format!("Your VM has been up for {hours} hours.")).await?;
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
                bot.send_message(chat_id, format!("Unclaimed Plus days: {days}")).await?;
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
                    bot.send_message(chat_id, "No unclaimed days yet.").await?;
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
                bot.send_message(chat_id, "Your VM has been deregistered.").await?;
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
    let mut ticker = smol::Timer::interval(Duration::from_secs(300));
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
VALUES ($1, NULL, 300, 0, 0)
ON CONFLICT(vm_id) DO UPDATE SET
    up_secs = agent_records.up_secs + 300;
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
    let mut ticker = smol::Timer::interval(Duration::from_secs(300));
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
