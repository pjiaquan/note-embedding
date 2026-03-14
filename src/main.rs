mod config;
mod service;
mod ui;

use anyhow::{Context, Result, bail};
use clap::Parser;
use config::{
    AppConfig, TelegramConfig, create_default_config_if_missing, populate_missing_config_fields,
    resolve_config_path,
};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::Connection;
use service::{ServiceAction, handle_service_action};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::convert::TryFrom;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{self, Duration, MissedTickBehavior};
use walkdir::WalkDir;

const TELEGRAM_AUDIT_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
const TELEGRAM_TEXT_MESSAGE_MAX_CHARS: usize = 3800;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Watch a directory and index new notes for embedding workflows.",
    long_about = "Watch a directory and index new notes for embedding workflows.\n\nOn first run, the program creates a config template and exits.\nEdit the config file, then start the program again.\n\nDefault config path:\n  ~/.config/note-embedding/config",
    after_long_help = "Examples:\n  note-embedding\n  note-embedding --doctor\n  note-embedding --telegram-discover-chat\n  note-embedding --service start\n  note-embedding --service status\n  note-embedding --service log\n  note-embedding --config ~/.config/note-embedding/config\n  NOTE_EMBEDDING_CONFIG=~/.config/note-embedding/config note-embedding\n\nBuild:\n  cargo build --release\n\nFirst-run setup:\n  1. Run the program once to create the config template.\n  2. Edit watch_dir, file_types, Telegram settings, and embedding providers.\n  3. Run --telegram-discover-chat after sending a message to the bot.\n  4. Run --doctor to verify the setup.\n\nService install:\n  1. Build the binary: cargo build --release\n  2. Run the binary once: ./target/release/note-embedding\n  3. Edit ~/.config/note-embedding/config\n  4. Verify setup: ./target/release/note-embedding --doctor\n  5. Install and start the user service: ./target/release/note-embedding --service start\n\nService commands:\n  --service start      install/update and start the user service\n  --service status     show systemd user service status\n  --service log        show recent journald logs\n  --service restart    restart the user service\n  --service stop       stop the user service\n  --service uninstall  remove the user service, symlink, config, and data\n\nNotes:\n  - service management uses systemd --user\n  - the installed command symlink is ~/.local/bin/note-embedding\n  - the unit file is ~/.config/systemd/user/note-embedding.service"
)]
struct Cli {
    /// Override the config file path.
    ///
    /// If omitted, the program uses ~/.config/note-embedding/config.
    /// You can also set NOTE_EMBEDDING_CONFIG instead.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Run startup diagnostics and print small in-memory search sessionrecommended fixes.
    ///
    /// This does not start the background watcher loop.
    #[arg(long)]
    doctor: bool,

    /// Discover recent Telegram chats seen by the bot and write telegram.chat_id to config.
    #[arg(long)]
    telegram_discover_chat: bool,

    /// Manage the background user service.
    #[arg(long, value_enum, value_name = "ACTION")]
    service: Option<ServiceAction>,

    /// Alias for --doctor.
    #[arg(long, hide = true)]
    dry_run: bool,

    /// Alias for --service uninstall.
    #[arg(long, hide = true)]
    uninstall: bool,
}

impl Cli {
    fn config_path(&self) -> Option<PathBuf> {
        self.config
            .clone()
            .or_else(|| env::var_os("NOTE_EMBEDDING_CONFIG").map(PathBuf::from))
    }
}

// --- 数据库初始化 ---
fn init_db(database_path: &Path) -> Result<Connection> {
    if let Some(parent) = database_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }
    }

    let conn = Connection::open(database_path)
        .with_context(|| format!("failed to open database at {}", database_path.display()))?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS docs (
            id INTEGER PRIMARY KEY,
            path TEXT UNIQUE,
            content_summary TEXT,
            embedding_provider TEXT,
            embedding_dimensions INTEGER,
            embedding_vector TEXT,
            embedding_status TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )?;
    let _ = conn.execute("ALTER TABLE docs ADD COLUMN embedding_provider TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE docs ADD COLUMN embedding_dimensions INTEGER",
        [],
    );
    let _ = conn.execute("ALTER TABLE docs ADD COLUMN embedding_vector TEXT", []);
    let _ = conn.execute("ALTER TABLE docs ADD COLUMN embedding_status TEXT", []);
    Ok(conn)
}

fn find_embedding_status_by_path(db: &Connection, path: &Path) -> Result<Option<String>> {
    let mut statement =
        db.prepare("SELECT COALESCE(embedding_status, '') FROM docs WHERE path = ?1 LIMIT 1")?;
    let mut rows = statement.query([path.to_string_lossy().as_ref()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let status: String = row.get(0)?;
    Ok(Some(status))
}

fn upsert_pdf_tracking_record(db: &Connection, path: &Path, status: &str) -> Result<()> {
    db.execute(
        "INSERT INTO docs (path, content_summary, embedding_provider, embedding_dimensions, embedding_vector, embedding_status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET
             content_summary = excluded.content_summary,
             embedding_provider = excluded.embedding_provider,
             embedding_dimensions = excluded.embedding_dimensions,
             embedding_vector = excluded.embedding_vector,
             embedding_status = excluded.embedding_status",
        (
            path.to_string_lossy().as_ref(),
            status,
            "telegram",
            0_i64,
            "",
            status,
        ),
    )?;
    Ok(())
}

// --- Telegram 发送函数 ---
fn send_to_telegram(config: &TelegramConfig, message: &str) -> Result<()> {
    if !config.enabled || config.chat_id == 0 {
        return Ok(());
    }

    send_text_to_chat(&config.bot_token, config.chat_id, message, None)
}

fn send_text_to_chat(
    bot_token: &str,
    chat_id: i64,
    message: &str,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    send_text_to_chat_with_parse_mode(bot_token, chat_id, message, reply_to_message_id, None)
}

fn send_text_to_chat_with_parse_mode(
    bot_token: &str,
    chat_id: i64,
    message: &str,
    reply_to_message_id: Option<i32>,
    parse_mode: Option<frankenstein::ParseMode>,
) -> Result<()> {
    use frankenstein::TelegramApi;
    let api = frankenstein::Api::new(bot_token);
    let send_message_params = match (reply_to_message_id, parse_mode) {
        (Some(reply_to_message_id), Some(parse_mode)) => frankenstein::SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .parse_mode(parse_mode)
            .reply_to_message_id(reply_to_message_id)
            .build(),
        (Some(reply_to_message_id), None) => frankenstein::SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .reply_to_message_id(reply_to_message_id)
            .build(),
        (None, Some(parse_mode)) => frankenstein::SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .parse_mode(parse_mode)
            .build(),
        (None, None) => frankenstein::SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .build(),
    };
    api.send_message(&send_message_params)?;
    Ok(())
}

fn send_large_text_to_chat(
    bot_token: &str,
    chat_id: i64,
    message: &str,
    reply_to_message_id: Option<i32>,
    parse_mode: Option<frankenstein::ParseMode>,
) -> Result<()> {
    if message.is_empty() {
        return Ok(());
    }

    let mut start = 0;
    let chars: Vec<char> = message.chars().collect();
    while start < chars.len() {
        let end = usize::min(start + TELEGRAM_TEXT_MESSAGE_MAX_CHARS, chars.len());
        let chunk: String = chars[start..end].iter().collect();
        if let Some(mode) = parse_mode.clone() {
            if send_text_to_chat_with_parse_mode(
                bot_token,
                chat_id,
                &chunk,
                reply_to_message_id,
                Some(mode),
            )
            .is_ok()
            {
                start = end;
                continue;
            }
        }
        send_text_to_chat(bot_token, chat_id, &chunk, reply_to_message_id)?;
        start = end;
    }

    Ok(())
}

fn send_document_content_to_chat(
    bot_token: &str,
    chat_id: i64,
    path: &Path,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    if is_pdf_path(path) {
        return Ok(());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read document content from {}", path.display()))?;
    if content.trim().is_empty() {
        return send_text_to_chat(
            bot_token,
            chat_id,
            "Document content is empty.",
            reply_to_message_id,
        );
    }

    let message = format!("Content:\n\n{content}");
    #[allow(deprecated)]
    let parse_mode = if looks_like_markdown(path, &content) {
        Some(frankenstein::ParseMode::Markdown)
    } else {
        None
    };
    send_large_text_to_chat(bot_token, chat_id, &message, reply_to_message_id, parse_mode)
}

fn looks_like_markdown(path: &Path, content: &str) -> bool {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
    {
        return true;
    }

    content.lines().take(20).any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with('#')
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("> ")
            || trimmed.starts_with("```")
            || looks_like_ordered_list(trimmed)
            || (trimmed.contains('[') && trimmed.contains("]("))
    })
}

fn looks_like_ordered_list(line: &str) -> bool {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digits > 0 && line[digits..].starts_with(". ")
}

fn send_text_to_chat_with_search_results(
    bot_token: &str,
    chat_id: i64,
    session_id: u64,
    session: &SearchSession,
    page: usize,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    use frankenstein::{ReplyMarkup, SendMessageParams, TelegramApi};

    let api = frankenstein::Api::new(bot_token);
    let message = render_search_results_message(session, page);
    let inline_keyboard = build_search_results_keyboard(session_id, session, page);

    let send_message_params = match reply_to_message_id {
        Some(reply_to_message_id) => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(&message)
            .reply_to_message_id(reply_to_message_id)
            .reply_markup(ReplyMarkup::InlineKeyboardMarkup(inline_keyboard))
            .build(),
        None => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(&message)
            .reply_markup(ReplyMarkup::InlineKeyboardMarkup(inline_keyboard))
            .build(),
    };
    api.send_message(&send_message_params)?;
    Ok(())
}

fn send_text_to_chat_with_document_button(
    bot_token: &str,
    chat_id: i64,
    message: &str,
    doc_match: &QueryMatch,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    use frankenstein::{
        InlineKeyboardButton, InlineKeyboardMarkup, ReplyMarkup, SendMessageParams, TelegramApi,
    };

    let api = frankenstein::Api::new(bot_token);
    let keyboard = InlineKeyboardMarkup::builder()
        .inline_keyboard(vec![vec![
            InlineKeyboardButton::builder()
                .text(format_button_label(&doc_match.header))
                .callback_data(format!("doc:{}", doc_match.id))
                .build(),
        ]])
        .build();

    let send_message_params = match reply_to_message_id {
        Some(reply_to_message_id) => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .reply_to_message_id(reply_to_message_id)
            .reply_markup(ReplyMarkup::InlineKeyboardMarkup(keyboard))
            .build(),
        None => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .reply_markup(ReplyMarkup::InlineKeyboardMarkup(keyboard))
            .build(),
    };
    api.send_message(&send_message_params)?;
    Ok(())
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn truncate_audit_value(value: &str, limit: usize) -> String {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = cleaned.chars();
    let truncated: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn append_telegram_audit_log(config: &AppConfig, line: &str) -> Result<()> {
    if let Some(parent) = config.telegram.audit_log_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create audit log directory {}", parent.display())
            })?;
        }
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.telegram.audit_log_path)
        .with_context(|| {
            format!(
                "failed to open Telegram audit log {}",
                config.telegram.audit_log_path.display()
            )
        })?;
    writeln!(file, "{line}")?;
    trim_telegram_audit_log(&config.telegram.audit_log_path, TELEGRAM_AUDIT_LOG_MAX_BYTES)?;
    Ok(())
}

fn trim_telegram_audit_log(path: &Path, max_bytes: u64) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read audit log metadata {}", path.display()))?;
    if metadata.len() <= max_bytes {
        return Ok(());
    }

    let content = fs::read(path)
        .with_context(|| format!("failed to read audit log for trimming {}", path.display()))?;
    let keep_from = content.len().saturating_sub(max_bytes as usize);
    let trimmed = &content[keep_from..];
    let start = trimmed
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    fs::write(path, &trimmed[start..])
        .with_context(|| format!("failed to rewrite trimmed audit log {}", path.display()))?;
    Ok(())
}

fn log_telegram_usage(config: &AppConfig, line: String) {
    println!("{} {}", ui::info_label(), line);
    if let Err(err) = append_telegram_audit_log(config, &line) {
        eprintln!(
            "Failed to write Telegram audit log at {}: {err}",
            config.telegram.audit_log_path.display()
        );
    }
}

fn log_telegram_message_usage(
    config: &AppConfig,
    message: &frankenstein::Message,
    allowed: bool,
    action: &str,
    detail: &str,
) {
    let username = message
        .from
        .as_ref()
        .and_then(|user| user.username.as_deref())
        .unwrap_or("-");
    let user_id = message.from.as_ref().map(|user| user.id).unwrap_or_default();
    let line = format!(
        "telegram usage ts={} type=message allowed={} chat_id={} user_id={} username={} action={} detail={}",
        unix_timestamp_secs(),
        allowed,
        message.chat.id,
        user_id,
        username,
        action,
        truncate_audit_value(detail, 120)
    );
    log_telegram_usage(config, line);
}

fn log_telegram_callback_usage(
    config: &AppConfig,
    callback_query: &frankenstein::CallbackQuery,
    allowed: bool,
    action: &str,
    detail: &str,
) {
    let username = callback_query.from.username.as_deref().unwrap_or("-");
    let chat_id = callback_query
        .message
        .as_ref()
        .map(|message| message.chat.id.to_string())
        .unwrap_or_else(|| "-".to_string());
    let line = format!(
        "telegram usage ts={} type=callback allowed={} chat_id={} user_id={} username={} action={} detail={}",
        unix_timestamp_secs(),
        allowed,
        chat_id,
        callback_query.from.id,
        username,
        action,
        truncate_audit_value(detail, 120)
    );
    log_telegram_usage(config, line);
}

fn send_access_request_to_admins(
    bot_token: &str,
    admin_user_ids: &[u64],
    requester_user_id: u64,
) -> Result<usize> {
    use frankenstein::{
        InlineKeyboardButton, InlineKeyboardMarkup, ReplyMarkup, SendMessageParams, TelegramApi,
    };

    let api = frankenstein::Api::new(bot_token);
    let keyboard = InlineKeyboardMarkup::builder()
        .inline_keyboard(vec![vec![
            InlineKeyboardButton::builder()
                .text(format!("Approve {requester_user_id}"))
                .callback_data(format!("approve:{requester_user_id}"))
                .build(),
        ]])
        .build();
    let text = format!(
        "Access request received.\nUser ID: {requester_user_id}\nApprove with /approve {requester_user_id} or the button below."
    );

    let mut delivered = 0;
    for admin_user_id in admin_user_ids {
        let Ok(chat_id) = i64::try_from(*admin_user_id) else {
            continue;
        };
        let params = SendMessageParams::builder()
            .chat_id(chat_id)
            .text(&text)
            .reply_markup(ReplyMarkup::InlineKeyboardMarkup(keyboard.clone()))
            .build();
        if api.send_message(&params).is_ok() {
            delivered += 1;
        }
    }

    Ok(delivered)
}

fn edit_search_results_message(
    bot_token: &str,
    chat_id: i64,
    message_id: i32,
    session_id: u64,
    session: &SearchSession,
    page: usize,
) -> Result<()> {
    use frankenstein::{EditMessageTextParams, TelegramApi};

    let api = frankenstein::Api::new(bot_token);
    let params = EditMessageTextParams::builder()
        .chat_id(chat_id)
        .message_id(message_id)
        .text(render_search_results_message(session, page))
        .reply_markup(build_search_results_keyboard(session_id, session, page))
        .build();
    api.edit_message_text(&params)?;
    Ok(())
}

fn send_document_to_chat(
    bot_token: &str,
    chat_id: i64,
    document_path: &Path,
    caption: &str,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    use frankenstein::TelegramApi;
    let api = frankenstein::Api::new(bot_token);
    let send_document_params = match reply_to_message_id {
        Some(reply_to_message_id) => frankenstein::SendDocumentParams::builder()
            .chat_id(chat_id)
            .document(document_path.to_path_buf())
            .caption(caption.to_string())
            .reply_to_message_id(reply_to_message_id)
            .build(),
        None => frankenstein::SendDocumentParams::builder()
            .chat_id(chat_id)
            .document(document_path.to_path_buf())
            .caption(caption.to_string())
            .build(),
    };
    api.send_document(&send_document_params)?;
    Ok(())
}

fn answer_callback_query(
    bot_token: &str,
    callback_query_id: &str,
    text: Option<&str>,
) -> Result<()> {
    use frankenstein::{AnswerCallbackQueryParams, TelegramApi};
    let api = frankenstein::Api::new(bot_token);
    let params = match text {
        Some(text) => AnswerCallbackQueryParams::builder()
            .callback_query_id(callback_query_id)
            .text(text)
            .build(),
        None => AnswerCallbackQueryParams::builder()
            .callback_query_id(callback_query_id)
            .build(),
    };
    api.answer_callback_query(&params)?;
    Ok(())
}

fn write_telegram_allowed_user_ids(config_path: &Path, allowed_user_ids: &[u64]) -> Result<()> {
    let original = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config at {}", config_path.display()))?;
    let mut root: toml::Value = toml::from_str(&original)
        .with_context(|| format!("failed to parse config at {}", config_path.display()))?;
    let root_table = root
        .as_table_mut()
        .context("config root must be a TOML table")?;
    let telegram_value = root_table
        .entry("telegram")
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
    let telegram_table = telegram_value
        .as_table_mut()
        .context("telegram section must be a TOML table")?;
    let values = allowed_user_ids
        .iter()
        .map(|user_id| toml::Value::Integer(*user_id as i64))
        .collect();
    telegram_table.insert("allowed_user_ids".to_string(), toml::Value::Array(values));
    let rewritten = toml::to_string_pretty(&root).context("failed to render updated config")?;
    fs::write(config_path, rewritten)
        .with_context(|| format!("failed to write config at {}", config_path.display()))
}

fn grant_telegram_user_access(
    config_path: &Path,
    config: &mut AppConfig,
    user_id: u64,
) -> Result<bool> {
    if is_telegram_admin(config, user_id) || config.telegram.allowed_user_ids.contains(&user_id) {
        return Ok(false);
    }

    config.telegram.allowed_user_ids.push(user_id);
    config.telegram.allowed_user_ids.sort_unstable();
    config.telegram.allowed_user_ids.dedup();
    write_telegram_allowed_user_ids(config_path, &config.telegram.allowed_user_ids)?;
    Ok(true)
}

fn notify_user_access_approved(bot_token: &str, user_id: u64) -> Result<()> {
    let Ok(chat_id) = i64::try_from(user_id) else {
        return Ok(());
    };
    send_text_to_chat(
        bot_token,
        chat_id,
        "Your access request was approved. You can use the bot now.",
        None,
    )
}

#[derive(Debug, Clone)]
struct QueryMatch {
    id: i64,
    header: String,
    path: PathBuf,
    score: f32,
}

#[derive(Debug)]
struct EmbeddingRecord {
    provider: String,
    status: String,
    vector: Vec<f32>,
}

#[derive(Debug, serde::Deserialize)]
struct OllamaEmbedResponse {
    #[serde(default)]
    embedding: Vec<f32>,
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
}

#[derive(Debug, serde::Deserialize)]
struct GeminiEmbedResponse {
    embedding: GeminiEmbedding,
}

#[derive(Debug, serde::Deserialize)]
struct GeminiEmbedding {
    values: Vec<f32>,
}

#[derive(Debug, Default)]
struct TelegramPollingState {
    next_offset: Option<u32>,
    next_search_session_id: u64,
    awaiting_new_chats: HashSet<i64>,
    search_sessions: HashMap<u64, SearchSession>,
}

#[derive(Debug, Clone)]
struct SearchSession {
    matches: Vec<QueryMatch>,
}

fn check_telegram(config: &TelegramConfig) -> Result<String> {
    if !config.enabled {
        return Ok("disabled".to_string());
    }

    use frankenstein::TelegramApi;
    let api = frankenstein::Api::new(&config.bot_token);
    let me = api.get_me().context("Telegram getMe request failed")?;
    Ok(format!(
        "authenticated as @{}",
        me.result
            .username
            .unwrap_or_else(|| "<no-username>".to_string())
    ))
}

#[derive(Debug, Default, serde::Deserialize)]
struct TelegramDiscoveryConfig {
    #[serde(default)]
    telegram: TelegramConfig,
}

#[derive(Clone, Debug)]
struct TelegramChatCandidate {
    id: i64,
    kind: String,
    title: String,
    source: &'static str,
}

fn discover_telegram_chat(config_override: Option<PathBuf>) -> Result<()> {
    let config_path = resolve_config_path(config_override)?;
    let created = create_default_config_if_missing(&config_path)?;
    if created {
        println!(
            "{} Created config template: {}",
            ui::info_label(),
            ui::path(config_path.display())
        );
        println!(
            "Set {} then run {} again.",
            ui::value("telegram.bot_token"),
            ui::command("note-embedding --telegram-discover-chat")
        );
        return Ok(());
    }

    let config_text = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read config at {}", config_path.display()))?;
    let discovery_config: TelegramDiscoveryConfig = toml::from_str(&config_text)
        .with_context(|| format!("failed to parse config at {}", config_path.display()))?;
    let telegram = discovery_config.telegram;

    if telegram.bot_token.trim().is_empty() || telegram.bot_token.trim().starts_with("PASTE_") {
        bail!(
            "telegram.bot_token is not configured in {}; set it before discovering chats",
            config_path.display()
        );
    }

    let candidates = fetch_telegram_chat_candidates(&telegram.bot_token)?;
    if candidates.is_empty() {
        println!(
            "{} No recent Telegram chats were found for this bot.",
            ui::warn_label()
        );
        println!(
            "Send a message to the bot, or add it to a group/channel and generate an update, then run --telegram-discover-chat again."
        );
        return Ok(());
    }

    let selected = select_telegram_chat(&candidates)?;
    write_telegram_chat_id(&config_path, selected.id)?;

    println!(
        "{} Updated {} in {} to {} ({})",
        ui::ok_label(),
        ui::value("telegram.chat_id"),
        ui::path(config_path.display()),
        ui::value(selected.id),
        selected.title
    );
    println!(
        "If {} is false, set it to true to enable notifications.",
        ui::value("telegram.enabled")
    );
    Ok(())
}

fn fetch_telegram_chat_candidates(bot_token: &str) -> Result<Vec<TelegramChatCandidate>> {
    use frankenstein::{AllowedUpdate, GetUpdatesParams, TelegramApi, UpdateContent};

    let api = frankenstein::Api::new(bot_token);
    let params = GetUpdatesParams::builder()
        .limit(100_u32)
        .allowed_updates(vec![
            AllowedUpdate::Message,
            AllowedUpdate::EditedMessage,
            AllowedUpdate::ChannelPost,
            AllowedUpdate::EditedChannelPost,
            AllowedUpdate::MyChatMember,
            AllowedUpdate::ChatMember,
            AllowedUpdate::ChatJoinRequest,
        ])
        .build();

    let updates = api
        .get_updates(&params)
        .context("Telegram getUpdates request failed")?
        .result;
    let mut chats = BTreeMap::<i64, TelegramChatCandidate>::new();

    for update in updates {
        match update.content {
            UpdateContent::Message(message)
            | UpdateContent::EditedMessage(message)
            | UpdateContent::ChannelPost(message)
            | UpdateContent::EditedChannelPost(message) => {
                let chat = *message.chat;
                chats
                    .entry(chat.id)
                    .or_insert_with(|| TelegramChatCandidate {
                        id: chat.id,
                        kind: format!("{:?}", chat.type_field),
                        title: format_chat_title(&chat),
                        source: "message",
                    });
            }
            UpdateContent::MyChatMember(update) | UpdateContent::ChatMember(update) => {
                let chat = update.chat;
                chats
                    .entry(chat.id)
                    .or_insert_with(|| TelegramChatCandidate {
                        id: chat.id,
                        kind: format!("{:?}", chat.type_field),
                        title: format_chat_title(&chat),
                        source: "member-update",
                    });
            }
            UpdateContent::ChatJoinRequest(request) => {
                let chat = request.chat;
                chats
                    .entry(chat.id)
                    .or_insert_with(|| TelegramChatCandidate {
                        id: chat.id,
                        kind: format!("{:?}", chat.type_field),
                        title: format_chat_title(&chat),
                        source: "join-request",
                    });
            }
            _ => {}
        }
    }

    Ok(chats.into_values().collect())
}

fn format_chat_title(chat: &frankenstein::Chat) -> String {
    if let Some(title) = &chat.title {
        return title.clone();
    }
    if let Some(username) = &chat.username {
        return format!("@{username}");
    }

    let mut name_parts = Vec::new();
    if let Some(first_name) = &chat.first_name {
        name_parts.push(first_name.as_str());
    }
    if let Some(last_name) = &chat.last_name {
        name_parts.push(last_name.as_str());
    }

    if name_parts.is_empty() {
        format!("chat {}", chat.id)
    } else {
        name_parts.join(" ")
    }
}

fn select_telegram_chat(candidates: &[TelegramChatCandidate]) -> Result<&TelegramChatCandidate> {
    println!("{}", ui::heading("Recent Telegram chats seen by the bot:"));
    for (index, candidate) in candidates.iter().enumerate() {
        println!(
            "  {}. {} [{}] id={} source={}",
            ui::value(index + 1),
            candidate.title,
            candidate.kind,
            ui::value(candidate.id),
            ui::detail(candidate.source)
        );
    }

    if candidates.len() == 1 {
        println!(
            "{} Only one chat was found. Selecting it automatically.",
            ui::info_label()
        );
        return Ok(&candidates[0]);
    }

    print!("Select a chat number to save as telegram.chat_id: ");
    io::stdout().flush().context("failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read selection from stdin")?;
    let selection = input
        .trim()
        .parse::<usize>()
        .context("invalid selection; enter a number from the list")?;

    if selection == 0 || selection > candidates.len() {
        bail!(
            "selection out of range; choose a number between 1 and {}",
            candidates.len()
        );
    }

    Ok(&candidates[selection - 1])
}

fn write_telegram_chat_id(config_path: &Path, chat_id: i64) -> Result<()> {
    let original = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config at {}", config_path.display()))?;
    let mut lines: Vec<String> = original.lines().map(ToString::to_string).collect();
    let mut in_telegram = false;
    let mut telegram_found = false;
    let mut chat_id_updated = false;
    let mut telegram_section_end = None;

    for (index, line) in lines.iter_mut().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_telegram && trimmed != "[telegram]" && telegram_section_end.is_none() {
                telegram_section_end = Some(index);
            }
            in_telegram = trimmed == "[telegram]";
            if in_telegram {
                telegram_found = true;
            }
            continue;
        }

        if in_telegram && trimmed.starts_with("chat_id") {
            *line = format!("chat_id = {chat_id}");
            chat_id_updated = true;
        }
    }

    if !telegram_found {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
            lines.push(String::new());
        }
        lines.push("[telegram]".to_string());
        lines.push("enabled = false".to_string());
        lines.push("bot_token = \"\"".to_string());
        lines.push(format!("chat_id = {chat_id}"));
        chat_id_updated = true;
    }

    if telegram_found && !chat_id_updated {
        let insert_at = telegram_section_end.unwrap_or(lines.len());
        lines.insert(insert_at, format!("chat_id = {chat_id}"));
        chat_id_updated = true;
    }

    if !chat_id_updated {
        bail!(
            "failed to update telegram.chat_id in {}",
            config_path.display()
        );
    }

    let mut rewritten = lines.join("\n");
    if original.ends_with('\n') || !rewritten.ends_with('\n') {
        rewritten.push('\n');
    }

    fs::write(config_path, rewritten)
        .with_context(|| format!("failed to write config at {}", config_path.display()))
}

fn poll_telegram_queries(
    db: &mut Connection,
    config: &mut AppConfig,
    config_path: &Path,
    state: &mut TelegramPollingState,
) -> Result<()> {
    if !config.telegram.enabled {
        return Ok(());
    }

    use frankenstein::{AllowedUpdate, GetUpdatesParams, TelegramApi, UpdateContent};

    let api = frankenstein::Api::new(&config.telegram.bot_token);
    let params = GetUpdatesParams::builder()
        .limit(100_u32)
        .allowed_updates(vec![
            AllowedUpdate::Message,
            AllowedUpdate::EditedMessage,
            AllowedUpdate::ChannelPost,
            AllowedUpdate::EditedChannelPost,
            AllowedUpdate::CallbackQuery,
        ]);
    let params = match state.next_offset {
        Some(offset) => params.offset(offset).build(),
        None => params.build(),
    };

    let response = api
        .get_updates(&params)
        .context("Telegram getUpdates request failed")?;

    for update in response.result {
        state.next_offset = Some(update.update_id + 1);
        match update.content {
            UpdateContent::Message(message)
            | UpdateContent::EditedMessage(message)
            | UpdateContent::ChannelPost(message)
            | UpdateContent::EditedChannelPost(message) => {
                if let Err(err) =
                    handle_telegram_query_message(db, config, config_path, state, &message)
                {
                    eprintln!("Telegram query handling failed: {err}");
                }
            }
            UpdateContent::CallbackQuery(callback_query) => {
                if let Err(err) = handle_telegram_callback_query(
                    db,
                    config,
                    config_path,
                    state,
                    &callback_query,
                )
                {
                    eprintln!("Telegram callback handling failed: {err}");
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn handle_telegram_query_message(
    db: &mut Connection,
    config: &mut AppConfig,
    config_path: &Path,
    state: &mut TelegramPollingState,
    message: &frankenstein::Message,
) -> Result<()> {
    if message.from.as_ref().is_some_and(|user| user.is_bot) {
        return Ok(());
    }

    let Some(from_user) = message.from.as_ref() else {
        return Ok(());
    };
    let chat_id = message.chat.id;
    let reply_to_message_id = Some(message.message_id);
    let text = message.text.as_deref().unwrap_or("").trim();
    let is_admin = is_telegram_admin(config, from_user.id);
    let action = if is_new_command(text) {
        "/new"
    } else if is_recent_documents_command(text) {
        "/start-or-show"
    } else if is_clean_command(text) {
        "/clean"
    } else if is_join_command(text) {
        "/join"
    } else if parse_approve_command(text).is_some() || is_approve_command(text) {
        "/approve"
    } else if parse_search_command(text).is_some() {
        "/s"
    } else if message.document.is_some() {
        "document"
    } else if text.is_empty() {
        "empty"
    } else {
        "text"
    };
    let detail = if let Some(document) = message.document.as_ref() {
        document.file_name.as_deref().unwrap_or("telegram-document")
    } else if text.is_empty() {
        "<empty>"
    } else {
        text
    };

    if !is_telegram_user_allowed(config, from_user.id) {
        log_telegram_message_usage(config, message, false, action, detail);
        if is_join_command(text) {
            let delivered =
                send_access_request_to_admins(&config.telegram.bot_token, &config.telegram.admin_user_ids, from_user.id)?;
            let response = if delivered == 0 {
                format!(
                    "No admin could be notified. Ask the owner to add your Telegram user ID {} manually.",
                    from_user.id
                )
            } else {
                format!(
                    "Access request sent to {} admin(s).\nYour Telegram user ID: {}",
                    delivered, from_user.id
                )
            };
            send_text_to_chat(
                &config.telegram.bot_token,
                chat_id,
                &response,
                reply_to_message_id,
            )?;
            return Ok(());
        }
        send_text_to_chat(
            &config.telegram.bot_token,
            chat_id,
            &format!(
                "Unauthorized user.\nSend /join to request access.\nYour Telegram user ID: {}",
                from_user.id
            ),
            reply_to_message_id,
        )?;
        return Ok(());
    }

    log_telegram_message_usage(config, message, true, action, detail);

    if is_join_command(text) {
        send_text_to_chat(
            &config.telegram.bot_token,
            chat_id,
            "You already have access.",
            reply_to_message_id,
        )?;
        return Ok(());
    }

    if let Some(requested_user_id) = parse_approve_command(text) {
        if !is_admin {
            send_text_to_chat(
                &config.telegram.bot_token,
                chat_id,
                "Admin only command.",
                reply_to_message_id,
            )?;
            return Ok(());
        }
        let added = grant_telegram_user_access(config_path, config, requested_user_id)?;
        let response = if added {
            format!("Approved Telegram user ID {requested_user_id}.")
        } else {
            format!("Telegram user ID {requested_user_id} already has access.")
        };
        send_text_to_chat(
            &config.telegram.bot_token,
            chat_id,
            &response,
            reply_to_message_id,
        )?;
        if added {
            notify_user_access_approved(&config.telegram.bot_token, requested_user_id)?;
        }
        return Ok(());
    }
    if is_approve_command(text) {
        send_text_to_chat(
            &config.telegram.bot_token,
            chat_id,
            "Usage: /approve <telegram_user_id>",
            reply_to_message_id,
        )?;
        return Ok(());
    }

    if is_new_command(text) {
        state.awaiting_new_chats.insert(chat_id);
        send_text_to_chat(
            &config.telegram.bot_token,
            chat_id,
            "Ready to receive new content. Send plain text or upload a .txt/.md file in your next message.",
            reply_to_message_id,
        )?;
        return Ok(());
    }

    if state.awaiting_new_chats.contains(&chat_id) {
        if let Some(document) = message.document.as_deref() {
            let file_name = document
                .file_name
                .as_deref()
                .unwrap_or("telegram-document.txt");
            let downloaded = download_telegram_text_document(&config.telegram.bot_token, document)?;
            let stored_doc =
                process_telegram_submission(db, config, file_name, &downloaded, "document")?;
            state.awaiting_new_chats.remove(&chat_id);
            send_text_to_chat_with_document_button(
                &config.telegram.bot_token,
                chat_id,
                "Saved new document.",
                &stored_doc,
                reply_to_message_id,
            )?;
            return Ok(());
        }

        if !text.is_empty() && !text.starts_with('/') {
            let file_name = format!("telegram-{}-{}.md", chat_id, message.message_id);
            let stored_doc = process_telegram_submission(db, config, &file_name, text, "text")?;
            state.awaiting_new_chats.remove(&chat_id);
            send_text_to_chat_with_document_button(
                &config.telegram.bot_token,
                chat_id,
                "Saved new text.",
                &stored_doc,
                reply_to_message_id,
            )?;
            return Ok(());
        }
    }

    if text.is_empty() {
        if state.awaiting_new_chats.contains(&chat_id) {
            send_text_to_chat(
                &config.telegram.bot_token,
                chat_id,
                "Send plain text or upload a .txt/.md file.",
                reply_to_message_id,
            )?;
        }
        return Ok(());
    }

    if is_recent_documents_command(text) {
        let latest_docs = find_latest_documents(db, 10)?;
        if latest_docs.is_empty() {
            send_text_to_chat(
                &config.telegram.bot_token,
                chat_id,
                "No documents are stored yet.",
                reply_to_message_id,
            )?;
            return Ok(());
        }

        let session_id = state.next_search_session_id;
        state.next_search_session_id += 1;
        let session = SearchSession {
            matches: latest_docs,
        };
        state.search_sessions.insert(session_id, session.clone());
        send_text_to_chat_with_search_results(
            &config.telegram.bot_token,
            chat_id,
            session_id,
            &session,
            0,
            reply_to_message_id,
        )?;
        return Ok(());
    }
    if is_clean_command(text) {
        let cleanup = clean_duplicate_docs(db)?;
        let response = format!(
            "Database cleanup finished.\nRemoved duplicate path rows: {}\nRemoved duplicate embedding rows: {}\nTotal removed: {}",
            cleanup.duplicate_paths_removed,
            cleanup.duplicate_embeddings_removed,
            cleanup.total_removed()
        );
        send_text_to_chat(
            &config.telegram.bot_token,
            chat_id,
            &response,
            reply_to_message_id,
        )?;
        return Ok(());
    }
    let Some(query) = parse_search_command(text) else {
        let help = telegram_help_message(text, is_admin);
        send_text_to_chat(
            &config.telegram.bot_token,
            chat_id,
            &help,
            reply_to_message_id,
        )?;
        return Ok(());
    };

    if query.is_empty() {
        send_text_to_chat(
            &config.telegram.bot_token,
            chat_id,
            "Usage: /s <keywords>",
            reply_to_message_id,
        )?;
        return Ok(());
    }

    let matches = find_query_matches(db, config, query, SEARCH_RESULTS_MAX_MATCHES)?;
    let threshold_matches: Vec<QueryMatch> = matches
        .into_iter()
        .filter(|doc_match| doc_match.score >= config.telegram.match_accuracy)
        .collect();

    match threshold_matches.as_slice() {
        [] => {
            send_text_to_chat(
                &config.telegram.bot_token,
                chat_id,
                "No documents matched that query above the configured threshold. Try more specific keywords or lower telegram.match_accuracy.",
                reply_to_message_id,
            )?;
        }
        many => {
            let session_id = state.next_search_session_id;
            state.next_search_session_id += 1;
            let session = SearchSession {
                matches: many.to_vec(),
            };
            state.search_sessions.insert(session_id, session.clone());
            send_text_to_chat_with_search_results(
                &config.telegram.bot_token,
                chat_id,
                session_id,
                &session,
                0,
                reply_to_message_id,
            )?;
        }
    }

    Ok(())
}

#[derive(Debug, Default)]
struct CleanupResult {
    duplicate_paths_removed: usize,
    duplicate_embeddings_removed: usize,
}

impl CleanupResult {
    fn total_removed(&self) -> usize {
        self.duplicate_paths_removed + self.duplicate_embeddings_removed
    }
}

fn clean_duplicate_docs(db: &Connection) -> Result<CleanupResult> {
    let duplicate_paths_removed = db.execute(
        "DELETE FROM docs
         WHERE id IN (
             SELECT older.id
             FROM docs AS older
             JOIN docs AS newer
               ON older.path = newer.path
              AND older.id < newer.id
         )",
        [],
    )?;

    let duplicate_embeddings_removed = db.execute(
        "DELETE FROM docs
         WHERE id IN (
             SELECT older.id
             FROM docs AS older
             JOIN docs AS newer
               ON COALESCE(older.content_summary, '') = COALESCE(newer.content_summary, '')
              AND COALESCE(older.embedding_provider, '') = COALESCE(newer.embedding_provider, '')
              AND COALESCE(older.embedding_dimensions, -1) = COALESCE(newer.embedding_dimensions, -1)
              AND COALESCE(older.embedding_vector, '') = COALESCE(newer.embedding_vector, '')
              AND COALESCE(older.embedding_status, '') = COALESCE(newer.embedding_status, '')
              AND older.id < newer.id
             WHERE COALESCE(older.embedding_vector, '') != ''
         )",
        [],
    )?;

    Ok(CleanupResult {
        duplicate_paths_removed,
        duplicate_embeddings_removed,
    })
}

fn process_telegram_submission(
    db: &mut Connection,
    config: &AppConfig,
    file_name: &str,
    content: &str,
    source_label: &str,
) -> Result<QueryMatch> {
    let summary = summarize(content, 100);
    let embedding = generate_embedding(config, content)?;
    let embedding_vector = serialize_embedding(&embedding.vector)?;
    let desired_name = normalize_telegram_submission_name(file_name, source_label);
    let processed_path =
        prepare_processed_destination(Path::new(&desired_name), &config.processed_dir)?;

    fs::write(&processed_path, content).with_context(|| {
        format!(
            "failed to write telegram submission to {}",
            processed_path.display()
        )
    })?;
    db.execute(
        "INSERT OR REPLACE INTO docs (path, content_summary, embedding_provider, embedding_dimensions, embedding_vector, embedding_status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            processed_path.to_string_lossy().as_ref(),
            summary.as_str(),
            embedding.provider.as_str(),
            embedding.vector.len() as i64,
            embedding_vector.as_str(),
            embedding.status.as_str(),
        ),
    )?;

    find_document_by_path(db, &processed_path)?
        .with_context(|| format!("failed to load saved document {}", processed_path.display()))
}

fn download_telegram_text_document(
    bot_token: &str,
    document: &frankenstein::Document,
) -> Result<String> {
    use frankenstein::{GetFileParams, TelegramApi};

    let file_name = document
        .file_name
        .as_deref()
        .unwrap_or("telegram-document.txt");
    let extension = Path::new(file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();
    if extension != "txt" && extension != "md" {
        bail!("only .txt and .md files are supported for /new");
    }

    let api = frankenstein::Api::new(bot_token);
    let params = GetFileParams::builder().file_id(&document.file_id).build();
    let file = api
        .get_file(&params)
        .context("Telegram getFile request failed")?;
    let file_path = file
        .result
        .file_path
        .context("Telegram file response did not include file_path")?;
    let download_url = format!("https://api.telegram.org/file/bot{bot_token}/{file_path}");
    let response = ureq::get(&download_url)
        .call()
        .with_context(|| format!("failed to download Telegram file from {download_url}"))?;
    response
        .into_string()
        .context("failed to decode Telegram file as UTF-8 text")
}

fn normalize_telegram_submission_name(file_name: &str, source_label: &str) -> String {
    let trimmed = file_name.trim();
    let fallback = format!("telegram-{source_label}.md");
    let candidate = if trimmed.is_empty() {
        &fallback
    } else {
        trimmed
    };
    let sanitized: String = candidate
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        fallback
    } else {
        sanitized
    }
}

fn find_latest_documents(db: &Connection, limit: usize) -> Result<Vec<QueryMatch>> {
    let mut statement = db.prepare(
        "SELECT id, path
         FROM docs
         ORDER BY created_at DESC, id DESC
         LIMIT ?1",
    )?;
    let rows = statement.query_map([limit as i64], |row| {
        let id: i64 = row.get(0)?;
        let path: String = row.get(1)?;
        Ok((id, path))
    })?;

    let mut matches = Vec::new();
    for row in rows {
        let (id, path) = row?;
        let path = PathBuf::from(path);
        if !path.exists() {
            continue;
        }

        let content = fs::read_to_string(&path).unwrap_or_default();
        matches.push(QueryMatch {
            id,
            header: extract_document_header(&content, &path),
            path,
            score: 0.0,
        });
    }

    Ok(matches)
}

fn handle_telegram_callback_query(
    db: &Connection,
    config: &mut AppConfig,
    config_path: &Path,
    state: &mut TelegramPollingState,
    callback_query: &frankenstein::CallbackQuery,
) -> Result<()> {
    let callback_data = callback_query.data.as_deref().unwrap_or("");
    let action = if parse_approve_callback(callback_data).is_some() {
        "approve-callback"
    } else if parse_page_callback(callback_data).is_some() {
        "page-callback"
    } else if parse_document_callback(callback_data).is_some() {
        "document-callback"
    } else {
        "callback"
    };
    if !is_telegram_user_allowed(config, callback_query.from.id) {
        log_telegram_callback_usage(config, callback_query, false, action, callback_data);
        answer_callback_query(
            &config.telegram.bot_token,
            &callback_query.id,
            Some("Unauthorized."),
        )?;
        return Ok(());
    }

    log_telegram_callback_usage(config, callback_query, true, action, callback_data);

    let Some(data) = callback_query.data.as_deref() else {
        return Ok(());
    };
    if let Some(requested_user_id) = parse_approve_callback(data) {
        if !is_telegram_admin(config, callback_query.from.id) {
            answer_callback_query(
                &config.telegram.bot_token,
                &callback_query.id,
                Some("Admin only."),
            )?;
            return Ok(());
        }
        let added = grant_telegram_user_access(config_path, config, requested_user_id)?;
        answer_callback_query(
            &config.telegram.bot_token,
            &callback_query.id,
            Some(if added {
                "User approved."
            } else {
                "User already allowed."
            }),
        )?;
        if let Some(message) = callback_query.message.as_ref() {
            let response = if added {
                format!("Approved Telegram user ID {requested_user_id}.")
            } else {
                format!("Telegram user ID {requested_user_id} already has access.")
            };
            send_text_to_chat(
                &config.telegram.bot_token,
                message.chat.id,
                &response,
                Some(message.message_id),
            )?;
        }
        if added {
            notify_user_access_approved(&config.telegram.bot_token, requested_user_id)?;
        }
        return Ok(());
    }
    if let Some((session_id, page)) = parse_page_callback(data) {
        let Some(message) = callback_query.message.as_ref() else {
            answer_callback_query(
                &config.telegram.bot_token,
                &callback_query.id,
                Some("This page is no longer available."),
            )?;
            return Ok(());
        };
        let Some(session) = state.search_sessions.get(&session_id) else {
            answer_callback_query(
                &config.telegram.bot_token,
                &callback_query.id,
                Some("Search results expired. Run /s again."),
            )?;
            return Ok(());
        };
        let max_page = search_results_page_count(session).saturating_sub(1);
        let page = page.min(max_page);
        edit_search_results_message(
            &config.telegram.bot_token,
            message.chat.id,
            message.message_id,
            session_id,
            session,
            page,
        )?;
        answer_callback_query(&config.telegram.bot_token, &callback_query.id, None)?;
        return Ok(());
    }
    let Some(doc_id) = parse_document_callback(data) else {
        return Ok(());
    };
    let Some(message) = callback_query.message.as_ref() else {
        answer_callback_query(
            &config.telegram.bot_token,
            &callback_query.id,
            Some("This selection is no longer available."),
        )?;
        return Ok(());
    };

    let Some(doc_match) = find_document_by_id(db, doc_id)? else {
        answer_callback_query(
            &config.telegram.bot_token,
            &callback_query.id,
            Some("Document not found."),
        )?;
        return Ok(());
    };

    if !doc_match.path.exists() {
        answer_callback_query(
            &config.telegram.bot_token,
            &callback_query.id,
            Some("The file is no longer available."),
        )?;
        return Ok(());
    }

    answer_callback_query(
        &config.telegram.bot_token,
        &callback_query.id,
        Some("Sending document..."),
    )?;

    let caption = format!("");
    send_document_to_chat(
        &config.telegram.bot_token,
        message.chat.id,
        &doc_match.path,
        &caption,
        Some(message.message_id),
    )?;
    send_document_content_to_chat(
        &config.telegram.bot_token,
        message.chat.id,
        &doc_match.path,
        Some(message.message_id),
    )?;

    Ok(())
}

fn find_query_matches(
    db: &Connection,
    config: &AppConfig,
    query: &str,
    limit: usize,
) -> Result<Vec<QueryMatch>> {
    let mut statement = db.prepare(
        "SELECT id, path, content_summary, COALESCE(embedding_provider, ''), COALESCE(embedding_vector, ''), COALESCE(embedding_status, '') FROM docs ORDER BY created_at DESC, id DESC LIMIT 200",
    )?;
    let rows = statement.query_map([], |row| {
        let id: i64 = row.get(0)?;
        let path: String = row.get(1)?;
        let summary: String = row.get(2)?;
        let embedding_provider: String = row.get(3)?;
        let embedding_vector: String = row.get(4)?;
        let _embedding_status: String = row.get(5)?;
        Ok((
            id,
            path,
            summary,
            embedding_provider,
            embedding_vector,
            _embedding_status,
        ))
    })?;

    let query_embedding = generate_embedding(config, query);
    let mut matches = Vec::new();

    for row in rows {
        let (id, path, summary, _embedding_provider, embedding_vector, _embedding_status) = row?;
        let path = PathBuf::from(path);
        if !path.exists() {
            continue;
        }

        let content = fs::read_to_string(&path).unwrap_or_default();
        let lexical_score = lexical_similarity(query, &format!("{summary}\n{content}"));
        let mut best_match_for_doc = QueryMatch {
            id,
            header: extract_document_header(&content, &path),
            path: path.clone(),
            score: lexical_score,
        };

        if let Ok(query_embedding) = &query_embedding {
            let Ok(document_embedding) = deserialize_embedding(&embedding_vector) else {
                matches.push(best_match_for_doc);
                continue;
            };
            let Some(score) = cosine_similarity(&query_embedding.vector, &document_embedding)
            else {
                matches.push(best_match_for_doc);
                continue;
            };

            if score > best_match_for_doc.score {
                best_match_for_doc = QueryMatch {
                    id,
                    header: extract_document_header(&content, &path),
                    path,
                    score,
                };
            }
        }

        matches.push(best_match_for_doc);
    }

    matches.sort_by(|left, right| right.score.total_cmp(&left.score));
    if matches.len() > limit {
        matches.truncate(limit);
    }
    Ok(matches)
}

fn find_document_by_id(db: &Connection, doc_id: i64) -> Result<Option<QueryMatch>> {
    let mut statement = db.prepare(
        "SELECT id, path, content_summary, COALESCE(embedding_provider, ''), COALESCE(embedding_status, '') FROM docs WHERE id = ?1",
    )?;
    let mut rows = statement.query([doc_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };

    let id: i64 = row.get(0)?;
    let path: String = row.get(1)?;
    let _summary: String = row.get(2)?;
    let _embedding_provider: String = row.get(3)?;
    let _embedding_status: String = row.get(4)?;
    let path = PathBuf::from(path);
    let content = fs::read_to_string(&path).unwrap_or_default();

    Ok(Some(QueryMatch {
        id,
        header: extract_document_header(&content, &path),
        path,
        score: 0.0,
    }))
}

fn find_document_by_path(db: &Connection, path: &Path) -> Result<Option<QueryMatch>> {
    let mut statement = db.prepare("SELECT id, path FROM docs WHERE path = ?1 LIMIT 1")?;
    let mut rows = statement.query([path.to_string_lossy().as_ref()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };

    let id: i64 = row.get(0)?;
    let path: String = row.get(1)?;
    let path = PathBuf::from(path);
    let content = fs::read_to_string(&path).unwrap_or_default();

    Ok(Some(QueryMatch {
        id,
        header: extract_document_header(&content, &path),
        path,
        score: 0.0,
    }))
}

fn parse_search_command(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let (command, rest) = trimmed
        .split_once(char::is_whitespace)
        .unwrap_or((trimmed, ""));
    if command == "/s" || command.starts_with("/s@") {
        return Some(rest.trim());
    }
    None
}

fn is_help_command(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "/help" || trimmed.starts_with("/help@")
}

fn is_telegram_user_allowed(config: &AppConfig, user_id: u64) -> bool {
    if is_telegram_admin(config, user_id) {
        return true;
    }

    config.telegram.allowed_user_ids.contains(&user_id)
}

fn is_telegram_admin(config: &AppConfig, user_id: u64) -> bool {
    config.telegram.admin_user_ids.contains(&user_id)
}

fn is_recent_documents_command(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "/start"
        || trimmed.starts_with("/start@")
        || trimmed == "/show"
        || trimmed.starts_with("/show@")
}

fn is_clean_command(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "/clean" || trimmed.starts_with("/clean@")
}

fn is_new_command(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "/new" || trimmed.starts_with("/new@")
}

fn is_join_command(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "/join" || trimmed.starts_with("/join@")
}

fn parse_approve_command(text: &str) -> Option<u64> {
    let trimmed = text.trim();
    let (command, rest) = trimmed.split_once(char::is_whitespace)?;
    if command == "/approve" || command.starts_with("/approve@") {
        return rest.trim().parse().ok();
    }
    None
}

fn is_approve_command(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "/approve" || trimmed.starts_with("/approve@")
}

fn telegram_help_message(text: &str, is_admin: bool) -> String {
    let trimmed = text.trim();
    let admin_line = if is_admin {
        "\n/approve <user_id> admin only: grant access"
    } else {
        ""
    };
    if is_help_command(trimmed) {
        format!("Commands:\n/start show the latest 10 documents\n/show show the latest 10 documents\n/new receive plain text or a .txt/.md file and store it\n/s <keywords> search similar documents\n/clean remove duplicate rows from the database\n/join request access\n/help show this help{admin_line}")
    } else if trimmed.starts_with('/') {
        format!("Unknown command.\n\nCommands:\n/start show the latest 10 documents\n/show show the latest 10 documents\n/new receive plain text or a .txt/.md file and store it\n/s <keywords> search similar documents\n/clean remove duplicate rows from the database\n/join request access\n/help show this help{admin_line}")
    } else {
        format!("Send a command to continue.\n\nCommands:\n/start show the latest 10 documents\n/show show the latest 10 documents\n/new receive plain text or a .txt/.md file and store it\n/s <keywords> search similar documents\n/clean remove duplicate rows from the database\n/join request access\n/help show this help{admin_line}")
    }
}

fn parse_document_callback(data: &str) -> Option<i64> {
    data.strip_prefix("doc:")?.parse().ok()
}

fn parse_approve_callback(data: &str) -> Option<u64> {
    data.strip_prefix("approve:")?.parse().ok()
}

fn parse_page_callback(data: &str) -> Option<(u64, usize)> {
    let mut parts = data.split(':');
    let prefix = parts.next()?;
    if prefix != "page" {
        return None;
    }
    let session_id = parts.next()?.parse().ok()?;
    let page = parts.next()?.parse().ok()?;
    Some((session_id, page))
}

fn extract_document_header(content: &str, path: &Path) -> String {
    for line in content.lines() {
        let trimmed = line.trim().trim_start_matches('#').trim();
        if !trimmed.is_empty() {
            return trimmed.chars().take(80).collect();
        }
    }

    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("Untitled document")
        .to_string()
}

fn format_button_label(header: &str) -> String {
    let label = header.trim();
    let mut chars = label.chars();
    let truncated: String = chars.by_ref().take(48).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

const SEARCH_RESULTS_PAGE_SIZE: usize = 5;
const SEARCH_RESULTS_MAX_MATCHES: usize = 50;

fn build_search_results_keyboard(
    session_id: u64,
    session: &SearchSession,
    page: usize,
) -> frankenstein::InlineKeyboardMarkup {
    use frankenstein::{InlineKeyboardButton, InlineKeyboardMarkup};

    let total_pages = search_results_page_count(session);
    let start = page * SEARCH_RESULTS_PAGE_SIZE;
    let end = usize::min(start + SEARCH_RESULTS_PAGE_SIZE, session.matches.len());
    let mut keyboard: Vec<Vec<InlineKeyboardButton>> = session.matches[start..end]
        .iter()
        .map(|doc_match| {
            let label = format_button_label(&doc_match.header);
            vec![
                InlineKeyboardButton::builder()
                    .text(label)
                    .callback_data(format!("doc:{}", doc_match.id))
                    .build(),
            ]
        })
        .collect();

    if total_pages > 1 {
        let mut nav_row = Vec::new();
        if page > 0 {
            nav_row.push(
                InlineKeyboardButton::builder()
                    .text("◀ Left")
                    .callback_data(format!("page:{session_id}:{}", page - 1))
                    .build(),
            );
        }
        if page + 1 < total_pages {
            nav_row.push(
                InlineKeyboardButton::builder()
                    .text("Right ▶")
                    .callback_data(format!("page:{session_id}:{}", page + 1))
                    .build(),
            );
        }
        if !nav_row.is_empty() {
            keyboard.push(nav_row);
        }
    }

    InlineKeyboardMarkup::builder()
        .inline_keyboard(keyboard)
        .build()
}

fn render_search_results_message(session: &SearchSession, page: usize) -> String {
    let total_pages = search_results_page_count(session);
    let current_page = page + 1;
    format!(
        "Pagination\nTotal: {}\nCurrent page: {}/{}",
        session.matches.len(),
        current_page,
        total_pages
    )
}

fn search_results_page_count(session: &SearchSession) -> usize {
    usize::max(1, session.matches.len().div_ceil(SEARCH_RESULTS_PAGE_SIZE))
}

fn serialize_embedding(vector: &[f32]) -> Result<String> {
    serde_json::to_string(vector).context("failed to serialize embedding vector")
}

fn deserialize_embedding(value: &str) -> Result<Vec<f32>> {
    serde_json::from_str(value).context("failed to deserialize embedding vector")
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.is_empty() || right.is_empty() || left.len() != right.len() {
        return None;
    }

    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;

    for (a, b) in left.iter().zip(right.iter()) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }

    if left_norm == 0.0 || right_norm == 0.0 {
        return None;
    }

    let cosine = dot / (left_norm.sqrt() * right_norm.sqrt());
    Some(((cosine.clamp(-1.0, 1.0)) + 1.0) / 2.0)
}

fn lexical_similarity(query: &str, candidate: &str) -> f32 {
    let query_tokens = tokenize(query);
    if query_tokens.is_empty() {
        return 0.0;
    }

    let candidate_lower = candidate.to_ascii_lowercase();
    let query_lower = query.to_ascii_lowercase();
    if candidate_lower.contains(query_lower.trim()) && !query_lower.trim().is_empty() {
        return 1.0;
    }

    let candidate_tokens = tokenize(candidate);
    let overlap = query_tokens
        .iter()
        .filter(|token| candidate_tokens.contains(token.as_str()))
        .count();

    overlap as f32 / query_tokens.len() as f32
}

fn tokenize(text: &str) -> HashSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter_map(|token| {
            let token = token.trim().to_ascii_lowercase();
            if token.is_empty() { None } else { Some(token) }
        })
        .collect()
}

fn build_watcher(sender: mpsc::UnboundedSender<Event>) -> Result<RecommendedWatcher> {
    Ok(RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                if event.kind.is_create() || event.kind.is_modify() {
                    let _ = sender.send(event);
                }
            }
        },
        Config::default(),
    )?)
}

fn process_existing_documents(db: &mut Connection, config: &AppConfig) -> Result<()> {
    let paths: Vec<PathBuf> = WalkDir::new(&config.watch_dir)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| should_watch_path(config, path))
        .collect();

    if paths.is_empty() {
        return Ok(());
    }

    println!(
        "{} Found {} existing document(s) in {}",
        ui::info_label(),
        ui::value(paths.len()),
        ui::path(config.watch_dir.display())
    );

    for path in paths {
        if let Err(err) = process_new_file(db, config, &path) {
            eprintln!("处理现有文件失败 ({}): {err}", path.display());
        }
    }

    Ok(())
}

fn summarize(content: &str, max_chars: usize) -> String {
    content.chars().take(max_chars).collect()
}

fn init_processed_dir(processed_dir: &Path) -> Result<()> {
    fs::create_dir_all(processed_dir)
        .with_context(|| format!("failed to create processed_dir {}", processed_dir.display()))
}

fn prepare_processed_destination(source_path: &Path, processed_dir: &Path) -> Result<PathBuf> {
    init_processed_dir(processed_dir)?;

    let file_name = source_path
        .file_name()
        .context("source file does not have a file name")?;
    let mut destination = processed_dir.join(file_name);

    if destination.exists() {
        destination = unique_destination_path(processed_dir, source_path)?;
    }

    Ok(destination)
}

fn move_to_processed_destination(source_path: &Path, destination: &Path) -> Result<()> {
    match fs::rename(source_path, destination) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            fs::copy(source_path, destination).with_context(|| {
                format!(
                    "failed to copy {} to {} after rename failed: {rename_error}",
                    source_path.display(),
                    destination.display(),
                )
            })?;
            fs::remove_file(source_path).with_context(|| {
                format!(
                    "failed to remove original file {} after copy",
                    source_path.display()
                )
            })?;
            Ok(())
        }
    }
}

fn unique_destination_path(processed_dir: &Path, source_path: &Path) -> Result<PathBuf> {
    let stem = source_path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("processed");
    let extension = source_path.extension().and_then(|value| value.to_str());

    for index in 1..10_000 {
        let candidate_name = match extension {
            Some(extension) => format!("{stem}-{index}.{extension}"),
            None => format!("{stem}-{index}"),
        };
        let candidate = processed_dir.join(candidate_name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!(
        "failed to generate a unique destination file name in {}",
        processed_dir.display()
    )
}

fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

fn should_watch_path(config: &AppConfig, path: &Path) -> bool {
    config.supports_path(path) || (is_pdf_path(path) && !config.is_processed_path(path))
}

#[derive(Clone, Copy)]
enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

struct DoctorCheck {
    name: &'static str,
    status: DoctorStatus,
    detail: String,
    fix: Option<String>,
}

impl DoctorCheck {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: DoctorStatus::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            status: DoctorStatus::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            status: DoctorStatus::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

fn check_ollama(config: &AppConfig) -> Result<String> {
    let base_url = config.embedding.ollama.base_url.trim_end_matches('/');
    let response = ureq::get(&format!("{base_url}/api/tags"))
        .call()
        .context("failed to reach Ollama /api/tags")?;
    if response.status() != 200 {
        bail!("Ollama returned HTTP {}", response.status());
    }

    Ok(format!(
        "reachable at {} using model {}",
        config.embedding.ollama.base_url, config.embedding.ollama.model
    ))
}

fn request_ollama_embedding(config: &AppConfig, text: &str) -> Result<Vec<f32>> {
    let base_url = config.embedding.ollama.base_url.trim_end_matches('/');
    let embed_url = format!("{base_url}/api/embed");
    let request_body = serde_json::json!({
        "model": config.embedding.ollama.model,
        "input": text,
    })
    .to_string();
    let response = ureq::post(&embed_url)
        .set("Content-Type", "application/json")
        .send_string(&request_body)
        .with_context(|| format!("failed to request Ollama embeddings from {embed_url}"))?;

    if response.status() != 200 {
        bail!("Ollama embed returned HTTP {}", response.status());
    }

    let payload: OllamaEmbedResponse = serde_json::from_str(
        &response
            .into_string()
            .context("failed to read Ollama embedding response body")?,
    )
    .context("failed to decode Ollama embedding response")?;
    if !payload.embedding.is_empty() {
        return Ok(payload.embedding);
    }
    if let Some(first) = payload.embeddings.into_iter().next() {
        let first: Vec<f32> = first;
        if !first.is_empty() {
            return Ok(first);
        }
    }

    bail!("Ollama embedding response did not contain a vector")
}

fn check_gemini(config: &AppConfig) -> Result<String> {
    let model = &config.embedding.gemini.model;
    let response = ureq::get(&format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}"
    ))
    .query("key", &config.embedding.gemini.api_key)
    .call()
    .with_context(|| format!("failed to query Gemini model {model}"))?;

    if response.status() != 200 {
        bail!("Gemini returned HTTP {}", response.status());
    }

    Ok(format!("model {} is accessible", model))
}

fn request_gemini_embedding(config: &AppConfig, text: &str) -> Result<Vec<f32>> {
    let model = &config.embedding.gemini.model;
    let url =
        format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:embedContent");
    let request_body = serde_json::json!({
        "content": {
            "parts": [{ "text": text }]
        }
    })
    .to_string();
    let response = ureq::post(&url)
        .query("key", &config.embedding.gemini.api_key)
        .set("Content-Type", "application/json")
        .send_string(&request_body)
        .with_context(|| format!("failed to request Gemini embedding from model {model}"))?;

    if response.status() != 200 {
        bail!("Gemini embed returned HTTP {}", response.status());
    }

    let payload: GeminiEmbedResponse = serde_json::from_str(
        &response
            .into_string()
            .context("failed to read Gemini embedding response body")?,
    )
    .context("failed to decode Gemini embedding response")?;
    if payload.embedding.values.is_empty() {
        bail!("Gemini embedding response did not contain a vector");
    }
    Ok(payload.embedding.values)
}

fn generate_embedding_with_provider(
    config: &AppConfig,
    provider: &str,
    text: &str,
) -> Result<EmbeddingRecord> {
    let vector = match provider {
        "ollama" => request_ollama_embedding(config, text)?,
        "gemini" => request_gemini_embedding(config, text)?,
        other => bail!("unsupported provider {other}"),
    };

    Ok(EmbeddingRecord {
        provider: provider.to_string(),
        status: format!(
            "embedding generated via {provider} ({} dimensions)",
            vector.len()
        ),
        vector,
    })
}

fn generate_embedding(config: &AppConfig, text: &str) -> Result<EmbeddingRecord> {
    let primary = config.embedding.preferred_provider.as_str();
    match generate_embedding_with_provider(config, primary, text) {
        Ok(mut record) => {
            record.status = format!(
                "embedding generated via {} ({} dimensions)",
                record.provider,
                record.vector.len()
            );
            Ok(record)
        }
        Err(primary_error) => {
            let fallback = config.embedding.fallback_provider.as_str();
            if !config.is_provider_enabled(fallback) {
                bail!("primary {primary} failed ({primary_error}); fallback disabled");
            }

            let mut record = generate_embedding_with_provider(config, fallback, text).with_context(
                || {
                    format!(
                        "primary {primary} failed ({primary_error}); fallback {fallback} also failed"
                    )
                },
            )?;
            record.status = format!(
                "primary {primary} failed ({primary_error}); embedding generated via fallback {} ({} dimensions)",
                record.provider,
                record.vector.len()
            );
            Ok(record)
        }
    }
}

fn run_check<F>(name: &'static str, fix: impl Into<String>, check: F) -> DoctorCheck
where
    F: FnOnce() -> Result<String>,
{
    match check() {
        Ok(message) => DoctorCheck::ok(name, message),
        Err(err) => DoctorCheck::fail(name, err.to_string(), fix),
    }
}

fn print_doctor_report(checks: &[DoctorCheck]) {
    for check in checks {
        let label = match check.status {
            DoctorStatus::Ok => ui::ok_label(),
            DoctorStatus::Warn => ui::warn_label(),
            DoctorStatus::Fail => ui::fail_label(),
        };
        let mut lines = check.detail.lines();
        if let Some(first_line) = lines.next() {
            println!("{label} {}: {}", check.name, first_line);
        } else {
            println!("{label} {}:", check.name);
        }
        for line in lines {
            println!("  {line}");
        }
        if let Some(fix) = &check.fix {
            println!("  {} {fix}", ui::info_label());
        }
    }
}

fn config_error_fix(config_path: &Path, detail: &str) -> String {
    let mut fixes = Vec::new();

    if detail.contains("failed to read config") {
        fixes.push(format!(
            "Create the config file at {} or run the program once without --doctor to generate it.",
            config_path.display()
        ));
    }
    if detail.contains("failed to parse config") {
        fixes.push(format!(
            "Open {} and fix the TOML syntax. Check quotes, commas, and section headers.",
            config_path.display()
        ));
    }
    if detail.contains("watch_dir does not exist")
        || detail.contains("watch_dir is not a directory")
    {
        fixes.push(format!(
            "Set watch_dir in {} to an existing directory the service user can access.",
            config_path.display()
        ));
    }
    if detail.contains("processed_dir is not a directory")
        || detail.contains("processed_dir must be different from watch_dir")
    {
        fixes.push(format!(
            "Set processed_dir in {} to a different directory from watch_dir. The directory will be created automatically when needed.",
            config_path.display()
        ));
    }
    if detail.contains("file_types must contain at least one extension") {
        fixes.push(format!(
            "Set file_types in {} to a non-empty list such as [\"txt\", \"md\"].",
            config_path.display()
        ));
    }
    if detail.contains("telegram.bot_token is empty") || detail.contains("telegram.chat_id is 0") {
        fixes.push(format!(
            "Either disable [telegram], or set telegram.bot_token and telegram.chat_id in {}. After setting the bot token, you can run `note-embedding --telegram-discover-chat` to discover and save the chat ID.",
            config_path.display()
        ));
    }
    if detail.contains("embedding.preferred_provider")
        || detail.contains("embedding.fallback_provider")
        || detail.contains("embedding.ollama")
        || detail.contains("embedding.gemini")
    {
        fixes.push(format!(
            "Review the [embedding], [embedding.ollama], and [embedding.gemini] sections in {}. Use provider names 'ollama' or 'gemini' only.",
            config_path.display()
        ));
    }

    if fixes.is_empty() {
        fixes.push(format!(
            "Review {} and correct the reported configuration errors.",
            config_path.display()
        ));
    }

    fixes.join(" ")
}

fn run_doctor(config_override: Option<PathBuf>) -> Result<()> {
    let config_path = resolve_config_path(config_override)?;
    println!("{}", ui::heading("Running doctor checks..."));
    println!("Config path: {}", ui::path(config_path.display()));

    let loaded_config = match AppConfig::load_or_create(Some(config_path.clone())) {
        Ok(loaded_config) => loaded_config,
        Err(err) => {
            let detail = err.to_string();
            let check =
                DoctorCheck::fail("config", &detail, config_error_fix(&config_path, &detail));
            print_doctor_report(&[check]);
            bail!("doctor found configuration errors")
        }
    };

    if loaded_config.created {
        let check = DoctorCheck::warn(
            "config",
            format!(
                "created a new config template at {}",
                loaded_config.path.display()
            ),
            format!(
                "Edit {}, fill in watch_dir, embedding providers, and optional Telegram settings, then rerun --doctor.",
                loaded_config.path.display()
            ),
        );
        print_doctor_report(&[check]);
        bail!("doctor created a new config template")
    }

    let config = loaded_config.data;
    let populated_fields = populate_missing_config_fields(&loaded_config.path, &config)?;
    let mut checks = Vec::new();

    checks.push(DoctorCheck::ok(
        "config",
        format!("loaded {}", loaded_config.path.display()),
    ));
    if !populated_fields.is_empty() {
        checks.push(DoctorCheck::ok(
            "config.autofill",
            format!(
                "added missing config field(s): {}",
                populated_fields.join(", ")
            ),
        ));
    }
    checks.push(DoctorCheck::ok(
        "watch_dir",
        format!("found {}", config.watch_dir.display()),
    ));
    checks.push(DoctorCheck::ok(
        "file_types",
        format!("monitoring {}", config.file_types.join(", ")),
    ));
    checks.push(run_check(
        "processed_dir",
        format!(
            "Set processed_dir in {} to a writable directory that is different from watch_dir.",
            loaded_config.path.display()
        ),
        || {
            init_processed_dir(&config.processed_dir)?;
            Ok(format!("ready at {}", config.processed_dir.display()))
        },
    ));
    checks.push(run_check(
        "database",
        format!(
            "Set database_path in {} to a writable location for the service user.",
            loaded_config.path.display()
        ),
        || {
            let _db = init_db(&config.database_path)?;
            Ok(format!("ready at {}", config.database_path.display()))
        },
    ));
    checks.push(run_check(
        "watcher",
        format!(
            "Make sure the service user can read and watch {}.",
            config.watch_dir.display()
        ),
        || {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut watcher = build_watcher(tx)?;
            watcher.watch(&config.watch_dir, RecursiveMode::Recursive)?;
            Ok("watcher can subscribe to watch_dir".to_string())
        },
    ));
    checks.push(run_check(
        "embedding.primary",
        format!(
            "For Ollama: start the server, verify base_url, and run 'ollama list'. For Gemini: enable the section, set api_key, and verify the model name in {}.",
            loaded_config.path.display()
        ),
        || match config.embedding.preferred_provider.as_str() {
            "ollama" => check_ollama(&config),
            "gemini" => check_gemini(&config),
            other => bail!("unsupported provider {other}"),
        },
    ));
    if config.is_provider_enabled(&config.embedding.fallback_provider) {
        checks.push(run_check(
            "embedding.fallback",
            format!(
                "Check the fallback provider settings in {}. For Ollama, confirm the server and model. For Gemini, confirm enabled = true, api_key, and model.",
                loaded_config.path.display()
            ),
            || match config.embedding.fallback_provider.as_str() {
                "ollama" => check_ollama(&config),
                "gemini" => check_gemini(&config),
                other => bail!("unsupported provider {other}"),
            },
        ));
    } else {
        checks.push(DoctorCheck::warn(
            "embedding.fallback",
            format!("{} is configured but disabled", config.embedding.fallback_provider),
            format!(
                "Enable the fallback provider in {} if you want automatic failover, or leave it disabled to use only the primary provider.",
                loaded_config.path.display()
            ),
        ));
    }

    if config.telegram.enabled {
        checks.push(run_check(
            "telegram",
            format!(
                "Set telegram.bot_token in {} or disable [telegram]. Use --telegram-discover-chat if you want to save a default chat_id for proactive notifications.",
                loaded_config.path.display()
            ),
            || check_telegram(&config.telegram),
        ));
        if config.telegram.admin_user_ids.is_empty() && config.telegram.allowed_user_ids.is_empty()
        {
            checks.push(DoctorCheck::warn(
                "telegram.access",
                "admin_user_ids and allowed_user_ids are empty; any Telegram user who can message the bot may use it",
                format!(
                    "Set {} and {} in {} to restrict the bot and allow admin approvals.",
                    ui::value("telegram.admin_user_ids"),
                    ui::value("telegram.allowed_user_ids"),
                    loaded_config.path.display()
                ),
            ));
        } else if config.telegram.admin_user_ids.is_empty() {
            checks.push(DoctorCheck::warn(
                "telegram.access",
                "admin_user_ids is empty; access requests cannot be approved from Telegram",
                format!(
                    "Set {} in {} if you want to approve users with /approve or /join.",
                    ui::value("telegram.admin_user_ids"),
                    loaded_config.path.display()
                ),
            ));
        }
        if config.telegram.chat_id == 0 {
            checks.push(DoctorCheck::warn(
                "telegram.notifications",
                "chat_id is not configured; proactive document notifications are disabled",
                format!(
                    "Run {} after messaging the bot if you want the app to push new-document notifications to a default chat.",
                    ui::command("note-embedding --telegram-discover-chat")
                ),
            ));
        }
    } else {
        checks.push(DoctorCheck::warn(
            "telegram",
            "disabled",
            format!(
                "Enable [telegram] in {} if you want notifications.",
                loaded_config.path.display()
            ),
        ));
    }
    checks.push(run_check(
        "embedding.result",
        format!(
            "Check the configured embedding provider in {}. For Ollama, verify the server is running and the model supports embeddings. For Gemini, verify api_key and model access.",
            loaded_config.path.display()
        ),
        || {
            let sample = generate_embedding(&config, "doctor check sample embedding")?;
            Ok(format!(
                "{}; provider {}; {} dimensions",
                sample.status,
                sample.provider,
                sample.vector.len()
            ))
        },
    ));

    print_doctor_report(&checks);

    let failures = checks
        .iter()
        .filter(|check| matches!(check.status, DoctorStatus::Fail))
        .count();
    let warnings = checks
        .iter()
        .filter(|check| matches!(check.status, DoctorStatus::Warn))
        .count();

    println!(
        "Doctor summary: {} ok, {} warning(s), {} failure(s)",
        ui::value(checks.len() - warnings - failures),
        ui::value(warnings),
        ui::value(failures)
    );

    if failures == 0 {
        Ok(())
    } else {
        bail!("doctor found {failures} failing check(s)")
    }
}

fn process_new_file(db: &mut Connection, config: &AppConfig, path: &Path) -> Result<()> {
    if is_pdf_path(path) {
        return process_pdf_file(db, config, path);
    }

    if !config.supports_path(path) {
        return Ok(());
    }

    println!(
        "{} Processing document: {}",
        ui::info_label(),
        ui::path(path.display())
    );

    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let summary = summarize(&content, 100);
    let embedding = generate_embedding(config, &content)?;
    let embedding_vector = serialize_embedding(&embedding.vector)?;
    println!(
        "{} Embedding result: {}",
        ui::ok_label(),
        ui::value(&embedding.status)
    );
    println!(
        "{} Stored vector dimensions: {}",
        ui::ok_label(),
        ui::value(embedding.vector.len())
    );

    let processed_path = prepare_processed_destination(path, &config.processed_dir)?;
    let tx = db.transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO docs (path, content_summary, embedding_provider, embedding_dimensions, embedding_vector, embedding_status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            processed_path.to_string_lossy().as_ref(),
            summary.as_str(),
            embedding.provider.as_str(),
            embedding.vector.len() as i64,
            embedding_vector.as_str(),
            embedding.status.as_str(),
        ),
    )?;
    move_to_processed_destination(path, &processed_path)?;
    tx.commit()?;

    let message = format!(
        "Processed document:\nFile: {}\nMoved to: {}\nEmbedding provider: {}\nEmbedding result: {}\nVector dimensions: {}\nSummary: {}",
        processed_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unknown>"),
        processed_path.display(),
        embedding.provider,
        embedding.status,
        embedding.vector.len(),
        summary
    );
    if let Err(err) = send_to_telegram(&config.telegram, &message) {
        eprintln!("TG 发送失败: {err}");
    }

    Ok(())
}

fn process_pdf_file(db: &mut Connection, config: &AppConfig, path: &Path) -> Result<()> {
    if config.is_processed_path(path) || !is_pdf_path(path) {
        return Ok(());
    }

    if let Some(status) = find_embedding_status_by_path(db, path)? {
        if matches!(
            status.as_str(),
            "telegram-pdf-forwarded" | "telegram-pdf-uploading" | "telegram-pdf-empty"
        ) {
            return Ok(());
        }
    }

    if !config.telegram.enabled || config.telegram.chat_id == 0 {
        eprintln!(
            "Skipping PDF upload for {} because Telegram proactive notifications are not configured",
            path.display()
        );
        return Ok(());
    }

    let metadata =
        fs::metadata(path).with_context(|| format!("failed to read metadata for {}", path.display()))?;
    if metadata.len() == 0 {
        upsert_pdf_tracking_record(db, path, "telegram-pdf-empty")?;
        eprintln!(
            "Skipping PDF upload for {} because the file is empty",
            path.display()
        );
        return Ok(());
    }

    println!(
        "{} Forwarding PDF to Telegram: {}",
        ui::info_label(),
        ui::path(path.display())
    );

    upsert_pdf_tracking_record(db, path, "telegram-pdf-uploading")?;

    let caption = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("New PDF: {name}"))
        .unwrap_or_else(|| "New PDF".to_string());
    match send_document_to_chat(
        &config.telegram.bot_token,
        config.telegram.chat_id,
        path,
        &caption,
        None,
    ) {
        Ok(()) => upsert_pdf_tracking_record(db, path, "telegram-pdf-forwarded")?,
        Err(err) => {
            upsert_pdf_tracking_record(db, path, "telegram-pdf-forward-failed")?;
            return Err(err);
        }
    }

    Ok(())
}

// --- 核心业务逻辑 ---
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.uninstall {
        return handle_service_action(ServiceAction::Uninstall, cli.config_path());
    }
    if cli.telegram_discover_chat {
        return discover_telegram_chat(cli.config_path());
    }
    if let Some(action) = cli.service.clone() {
        return handle_service_action(action, cli.config_path());
    }
    if cli.doctor || cli.dry_run {
        return run_doctor(cli.config_path());
    }

    let loaded_config = AppConfig::load_or_create(cli.config_path())?;

    if loaded_config.created {
        println!(
            "{} Created config template: {}",
            ui::info_label(),
            ui::path(loaded_config.path.display())
        );
        println!("Edit the config values, then run the service again.");
        return Ok(());
    }

    let mut config = loaded_config.data;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut db = init_db(&config.database_path)?;
    init_processed_dir(&config.processed_dir)?;
    let mut telegram_state = TelegramPollingState::default();
    let mut telegram_interval =
        time::interval(Duration::from_secs(config.telegram.poll_interval_secs));
    telegram_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut watcher = build_watcher(tx)?;
    watcher.watch(&config.watch_dir, RecursiveMode::Recursive)?;
    println!("Using config: {}", ui::path(loaded_config.path.display()));
    println!(
        "Watching directory: {}",
        ui::path(config.watch_dir.display())
    );
    println!(
        "Processed files directory: {}",
        ui::path(config.processed_dir.display())
    );
    if config.telegram.enabled {
        println!(
            "Telegram polling: every {} second(s), match accuracy {}",
            ui::value(config.telegram.poll_interval_secs),
            ui::value(format!("{:.2}", config.telegram.match_accuracy))
        );
        println!(
            "Telegram audit log: {}",
            ui::path(config.telegram.audit_log_path.display())
        );
    }

    process_existing_documents(&mut db, &config)?;

    // 2. 事件处理循环
    loop {
        tokio::select! {
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };
                for path in event.paths {
                    if let Err(err) = process_new_file(&mut db, &config, &path) {
                        eprintln!("处理文件失败 ({}): {err}", path.display());
                    }
                }
            }
            _ = telegram_interval.tick(), if config.telegram.enabled => {
                if let Err(err) = poll_telegram_queries(
                    &mut db,
                    &mut config,
                    &loaded_config.path,
                    &mut telegram_state,
                ) {
                    eprintln!("Telegram polling failed: {err}");
                }
            }
        }
    }

    Ok(())
}
