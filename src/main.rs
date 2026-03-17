mod config;
mod service;
mod telegram;
mod ui;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode, Uri},
    routing::post,
};
use clap::Parser;
use config::{
    AppConfig, TelegramConfig, TelegramMode, create_default_config_if_missing,
    populate_missing_config_fields, resolve_config_path,
};
use ipnet::IpNet;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use rand::{Rng, distributions::Alphanumeric, rngs::OsRng};
use rusqlite::Connection;
use service::{ServiceAction, handle_service_action};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use telegram::{
    answer_callback_query, delete_registered_webhook, install_telegram_audit_log_path,
    log_telegram_callback_usage, log_telegram_message_usage, log_telegram_outbound_result,
    log_telegram_webhook_request, notify_user_access_approved, register_bot_commands,
    send_access_request_to_admins, send_document_content_to_chat, send_document_to_chat,
    send_text_to_chat, send_text_to_chat_with_command_keyboard,
    send_text_to_chat_with_document_button, send_to_telegram,
    send_to_telegram_with_document_button, with_telegram_api_retry,
};
use tokio::sync::mpsc;
use tokio::time::{self, Duration, MissedTickBehavior};
use walkdir::WalkDir;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Watch a directory and index new notes for embedding workflows.",
    long_about = "Watch a directory and index new notes for embedding workflows.\n\nOn first run, the program creates a config template and exits.\nEdit the config file, then start the program again.\n\nDefault config path:\n  ~/.config/note-embedding/config",
    after_long_help = "Examples:\n  note-embedding\n  note-embedding --doctor\n  note-embedding --telegram-discover-chat\n  note-embedding --telegram-delete-webhook\n  note-embedding --service start\n  note-embedding --service status\n  note-embedding --service log\n  note-embedding --config ~/.config/note-embedding/config\n  NOTE_EMBEDDING_CONFIG=~/.config/note-embedding/config note-embedding\n\nBuild:\n  cargo build --release\n\nFirst-run setup:\n  1. Run the program once to create the config template.\n  2. Edit watch_dir, file_types, Telegram settings, and embedding providers.\n  3. Run --telegram-discover-chat after sending a message to the bot.\n  4. Run --doctor to verify the setup.\n\nService install:\n  1. Build the binary: cargo build --release\n  2. Run the binary once: ./target/release/note-embedding\n  3. Edit ~/.config/note-embedding/config\n  4. Verify setup: ./target/release/note-embedding --doctor\n  5. Install and start the user service: ./target/release/note-embedding --service start\n\nService commands:\n  --service start      install/update and start the user service\n  --service status     show systemd user service status\n  --service log        show recent journald logs\n  --service restart    restart the user service\n  --service stop       stop the user service\n  --service uninstall  remove the user service, symlink, config, and data\n\nNotes:\n  - service management uses systemd --user\n  - the installed command symlink is ~/.local/bin/note-embedding\n  - the unit file is ~/.config/systemd/user/note-embedding.service"
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

    /// Delete the Telegram webhook registered for telegram.bot_token in config.
    #[arg(long)]
    telegram_delete_webhook: bool,

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

fn send_text_to_chat_with_search_results(
    bot_token: &str,
    chat_id: i64,
    session_id: u64,
    session: &SearchSession,
    page: usize,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    use frankenstein::{ReplyMarkup, SendMessageParams, TelegramApi};

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
    let result = with_telegram_api_retry(bot_token, "sendMessage", |api| {
        api.send_message(&send_message_params)
    });
    match result {
        Ok(response) => {
            log_telegram_outbound_result(
                "sendMessage",
                Some(chat_id),
                reply_to_message_id,
                &message,
                None,
            );
            Ok(response)
        }
        Err(err) => {
            log_telegram_outbound_result(
                "sendMessage",
                Some(chat_id),
                reply_to_message_id,
                &message,
                Some(&err.to_string()),
            );
            Err(err)
        }
    }?;
    Ok(())
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

    let params = EditMessageTextParams::builder()
        .chat_id(chat_id)
        .message_id(message_id)
        .text(render_search_results_message(session, page))
        .reply_markup(build_search_results_keyboard(session_id, session, page))
        .build();
    let detail = format!("message_id={message_id} session_id={session_id} page={page}");
    let result = with_telegram_api_retry(bot_token, "editMessageText", |api| {
        api.edit_message_text(&params)
    });
    match result {
        Ok(response) => {
            log_telegram_outbound_result(
                "editMessageText",
                Some(chat_id),
                Some(message_id),
                &detail,
                None,
            );
            Ok(response)
        }
        Err(err) => {
            log_telegram_outbound_result(
                "editMessageText",
                Some(chat_id),
                Some(message_id),
                &detail,
                Some(&err.to_string()),
            );
            Err(err)
        }
    }?;
    Ok(())
}

fn write_telegram_allowed_user_ids(config_path: &Path, allowed_user_ids: &[u64]) -> Result<()> {
    update_telegram_config(config_path, |telegram_table| {
        let values = allowed_user_ids
            .iter()
            .map(|user_id| toml::Value::Integer(*user_id as i64))
            .collect();
        telegram_table.insert("allowed_user_ids".to_string(), toml::Value::Array(values));
        Ok(())
    })
}

fn update_telegram_config<F>(config_path: &Path, update: F) -> Result<()>
where
    F: FnOnce(&mut toml::value::Table) -> Result<()>,
{
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
    update(telegram_table)?;
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
struct TelegramRuntimeState {
    next_offset: Option<u32>,
    next_search_session_id: u64,
    awaiting_new_chats: HashSet<i64>,
    search_sessions: HashMap<u64, SearchSession>,
}

#[derive(Clone)]
struct TelegramWebhookAppState {
    secret_token: Option<String>,
    tx: mpsc::UnboundedSender<frankenstein::Update>,
    audit_log_path: PathBuf,
    cloudflare_ip_ranges: Arc<[IpNet]>,
    trusted_proxy_cidrs: Arc<[IpNet]>,
}

#[derive(Debug, Clone)]
struct SearchSession {
    matches: Vec<QueryMatch>,
}

fn check_telegram(config: &TelegramConfig) -> Result<String> {
    use frankenstein::TelegramApi;

    if !config.enabled {
        return Ok("disabled".to_string());
    }

    let me = with_telegram_api_retry(&config.bot_token, "getMe", |api| api.get_me())
        .context("Telegram getMe request failed")?;
    let username = me
        .result
        .username
        .unwrap_or_else(|| "<no-username>".to_string());
    let transport = match config.mode {
        TelegramMode::Polling => "polling".to_string(),
        TelegramMode::Webhook => {
            let webhook = with_telegram_api_retry(&config.bot_token, "getWebhookInfo", |api| {
                api.get_webhook_info()
            })
            .context("Telegram getWebhookInfo request failed")?;
            if webhook.result.url.is_empty() {
                "webhook not registered".to_string()
            } else {
                format!("webhook registered at {}", webhook.result.url)
            }
        }
    };
    Ok(format!("authenticated as @{username}; {transport}"))
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

const CLOUDFLARE_IPS_V4_URL: &str = "https://www.cloudflare.com/ips-v4";
const CLOUDFLARE_IPS_V6_URL: &str = "https://www.cloudflare.com/ips-v6";

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
    if telegram.mode == TelegramMode::Webhook {
        bail!(
            "--telegram-discover-chat requires telegram.mode = \"polling\" because Telegram disables getUpdates while a webhook is active"
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

    let updates = with_telegram_api_retry(bot_token, "getUpdates", |api| api.get_updates(&params))
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

fn telegram_runtime_allowed_updates() -> Vec<frankenstein::AllowedUpdate> {
    use frankenstein::AllowedUpdate;

    vec![
        AllowedUpdate::Message,
        AllowedUpdate::EditedMessage,
        AllowedUpdate::ChannelPost,
        AllowedUpdate::EditedChannelPost,
        AllowedUpdate::CallbackQuery,
    ]
}

fn fetch_cloudflare_ip_ranges() -> Result<Vec<IpNet>> {
    let mut ranges = Vec::new();
    for url in [CLOUDFLARE_IPS_V4_URL, CLOUDFLARE_IPS_V6_URL] {
        let response = ureq::get(url)
            .call()
            .with_context(|| format!("failed to fetch Cloudflare IP ranges from {url}"))?;
        let body = response
            .into_string()
            .with_context(|| format!("failed to read Cloudflare IP ranges from {url}"))?;
        for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let network = line
                .parse::<IpNet>()
                .with_context(|| format!("invalid Cloudflare IP range '{line}' from {url}"))?;
            ranges.push(network);
        }
    }

    if ranges.is_empty() {
        bail!("Cloudflare IP range list is empty");
    }

    Ok(ranges)
}

fn can_bind_telegram_webhook(bind_addr: SocketAddr) -> Result<String> {
    let listener = StdTcpListener::bind(bind_addr)
        .with_context(|| format!("failed to bind Telegram webhook listener at {bind_addr}"))?;
    drop(listener);
    Ok(format!("{bind_addr} is available for the webhook listener"))
}

fn prompt_input(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().context("failed to flush stdout")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read input from stdin")?;
    Ok(input.trim().to_string())
}

fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    loop {
        let suffix = if default_yes { " [Y/n]: " } else { " [y/N]: " };
        let answer = prompt_input(&format!("{prompt}{suffix}"))?;
        if answer.is_empty() {
            return Ok(default_yes);
        }

        match answer.to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("{} Enter y or n.", ui::warn_label()),
        }
    }
}

fn prompt_required_https_url(prompt: &str) -> Result<String> {
    loop {
        let value = prompt_input(prompt)?;
        if value.is_empty() {
            println!("{} Value cannot be empty.", ui::warn_label());
            continue;
        }
        if !value.starts_with("https://") {
            println!(
                "{} Webhook URL must start with https:// for Telegram webhook mode.",
                ui::warn_label()
            );
            continue;
        }
        return Ok(value);
    }
}

fn generate_webhook_secret_token() -> String {
    OsRng
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect()
}

fn prompt_webhook_secret_token() -> Result<String> {
    if prompt_yes_no("Generate a secure webhook secret token now?", true)? {
        return Ok(generate_webhook_secret_token());
    }

    loop {
        let value = prompt_input("Enter telegram.webhook_secret_token: ")?;
        if value.is_empty() {
            println!("{} Value cannot be empty.", ui::warn_label());
            continue;
        }
        if !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            println!(
                "{} Secret token may only contain letters, numbers, underscores, or hyphens.",
                ui::warn_label()
            );
            continue;
        }
        return Ok(value);
    }
}

fn interactive_doctor_prepare_config(config_path: &Path) -> Result<()> {
    let config_text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config at {}", config_path.display()))?;
    let discovery_config: TelegramDiscoveryConfig = toml::from_str(&config_text)
        .with_context(|| format!("failed to parse config at {}", config_path.display()))?;
    let telegram = discovery_config.telegram;

    if !telegram.enabled || telegram.mode != TelegramMode::Webhook {
        return Ok(());
    }

    let mut updated_fields = Vec::new();

    let token_missing = telegram.webhook_secret_token.trim().is_empty();
    let token_invalid = !token_missing
        && !telegram
            .webhook_secret_token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if token_missing || token_invalid {
        println!(
            "{} webhook mode requires a valid {}.",
            ui::info_label(),
            ui::value("telegram.webhook_secret_token")
        );
        let token = prompt_webhook_secret_token()?;
        update_telegram_config(config_path, |telegram_table| {
            telegram_table.insert(
                "webhook_secret_token".to_string(),
                toml::Value::String(token.clone()),
            );
            Ok(())
        })?;
        updated_fields.push("telegram.webhook_secret_token");
    }

    let url_missing = telegram.webhook_url.trim().is_empty();
    let url_invalid = !url_missing && !telegram.webhook_url.starts_with("https://");
    if url_missing || url_invalid {
        println!(
            "{} webhook mode requires a valid {}.",
            ui::info_label(),
            ui::value("telegram.webhook_url")
        );
        let webhook_url =
            prompt_required_https_url("Enter telegram.webhook_url (public https URL): ")?;
        update_telegram_config(config_path, |telegram_table| {
            telegram_table.insert(
                "webhook_url".to_string(),
                toml::Value::String(webhook_url.clone()),
            );
            Ok(())
        })?;
        updated_fields.push("telegram.webhook_url");
    }

    if !updated_fields.is_empty() {
        println!(
            "{} Updated {} in {}",
            ui::ok_label(),
            updated_fields.join(", "),
            ui::path(config_path.display())
        );
    }

    Ok(())
}

fn is_cloudflare_ip(peer_ip: IpAddr, cloudflare_ip_ranges: &[IpNet]) -> bool {
    cloudflare_ip_ranges
        .iter()
        .any(|network| network.contains(&peer_ip))
}

fn is_trusted_proxy_ip(peer_ip: IpAddr, trusted_proxy_cidrs: &[IpNet]) -> bool {
    trusted_proxy_cidrs
        .iter()
        .any(|network| network.contains(&peer_ip))
}

fn parse_trusted_proxy_cidrs(cidrs: &[String]) -> Result<Vec<IpNet>> {
    cidrs
        .iter()
        .map(|cidr| {
            cidr.parse::<IpNet>()
                .with_context(|| format!("invalid trusted proxy CIDR {cidr}"))
        })
        .collect()
}

fn header_value<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
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

fn delete_telegram_webhook_command(config_override: Option<PathBuf>) -> Result<()> {
    let loaded_config = AppConfig::load_or_create(config_override)?;

    if loaded_config.created {
        println!(
            "{} Created config template: {}",
            ui::info_label(),
            ui::path(loaded_config.path.display())
        );
        println!(
            "Set {} then run {} again.",
            ui::value("telegram.bot_token"),
            ui::command("note-embedding --telegram-delete-webhook")
        );
        return Ok(());
    }

    let config = loaded_config.data;
    if config.telegram.bot_token.trim().is_empty() {
        bail!(
            "telegram.bot_token is not configured in {}; set it before deleting the webhook",
            loaded_config.path.display()
        );
    }

    delete_registered_webhook(&config.telegram).context("failed to delete Telegram webhook")?;
    println!(
        "{} Deleted Telegram webhook for bot configured in {}",
        ui::ok_label(),
        ui::path(loaded_config.path.display())
    );
    Ok(())
}

fn configure_telegram_transport(config: &TelegramConfig) -> Result<()> {
    use frankenstein::{DeleteWebhookParams, SetWebhookParams, TelegramApi};

    match config.mode {
        TelegramMode::Polling => {
            let params = DeleteWebhookParams::builder().build();
            with_telegram_api_retry(&config.bot_token, "deleteWebhook", |api| {
                api.delete_webhook(&params)
            })
            .context("Telegram deleteWebhook request failed")?;
        }
        TelegramMode::Webhook => {
            let webhook_info =
                with_telegram_api_retry(&config.bot_token, "getWebhookInfo", |api| {
                    api.get_webhook_info()
                })
                .context("Telegram getWebhookInfo request failed")?;
            let current = webhook_info.result;
            let webhook_is_current = current.url == config.webhook_url
                && current.last_error_date.is_none()
                && current.last_error_message.is_none()
                && current.last_synchronization_error_date.is_none();
            if webhook_is_current {
                return Ok(());
            }

            let builder = SetWebhookParams::builder()
                .url(&config.webhook_url)
                .allowed_updates(telegram_runtime_allowed_updates());
            let params = if config.webhook_secret_token.is_empty() {
                builder.build()
            } else {
                builder.secret_token(&config.webhook_secret_token).build()
            };
            with_telegram_api_retry(&config.bot_token, "setWebhook", |api| {
                api.set_webhook(&params)
            })
            .context("Telegram setWebhook request failed")?;
        }
    }
    Ok(())
}

fn process_telegram_update(
    db: &mut Connection,
    config: &mut AppConfig,
    config_path: &Path,
    state: &mut TelegramRuntimeState,
    update: frankenstein::Update,
) {
    use frankenstein::UpdateContent;

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
            if let Err(err) =
                handle_telegram_callback_query(db, config, config_path, state, &callback_query)
            {
                eprintln!("Telegram callback handling failed: {err}");
            }
        }
        _ => {}
    }
}

fn poll_telegram_queries(
    db: &mut Connection,
    config: &mut AppConfig,
    config_path: &Path,
    state: &mut TelegramRuntimeState,
) -> Result<()> {
    if !config.telegram.enabled {
        return Ok(());
    }

    use frankenstein::{GetUpdatesParams, TelegramApi};

    let params = GetUpdatesParams::builder()
        .limit(100_u32)
        .allowed_updates(telegram_runtime_allowed_updates());
    let params = match state.next_offset {
        Some(offset) => params.offset(offset).build(),
        None => params.build(),
    };

    let response = with_telegram_api_retry(&config.telegram.bot_token, "getUpdates", |api| {
        api.get_updates(&params)
    })
    .context("Telegram getUpdates request failed")?;

    for update in response.result {
        state.next_offset = Some(update.update_id + 1);
        process_telegram_update(db, config, config_path, state, update);
    }

    Ok(())
}

async fn telegram_webhook_handler(
    State(state): State<TelegramWebhookAppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> StatusCode {
    const TELEGRAM_SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";
    const CF_CONNECTING_IP_HEADER: &str = "cf-connecting-ip";
    const CF_RAY_HEADER: &str = "cf-ray";
    const USER_AGENT_HEADER: &str = "user-agent";

    let peer_ip = peer_addr.ip();
    let cf_connecting_ip = header_value(&headers, CF_CONNECTING_IP_HEADER);
    let cf_ray = header_value(&headers, CF_RAY_HEADER);
    let user_agent = header_value(&headers, USER_AGENT_HEADER);
    let source_is_cloudflare = is_cloudflare_ip(peer_ip, &state.cloudflare_ip_ranges);
    let source_is_trusted_proxy = is_trusted_proxy_ip(peer_ip, &state.trusted_proxy_cidrs);
    let has_cloudflare_headers = cf_connecting_ip.is_some() && cf_ray.is_some();

    if !source_is_cloudflare && !(source_is_trusted_proxy && has_cloudflare_headers) {
        log_telegram_webhook_request(
            &state.audit_log_path,
            peer_ip,
            cf_connecting_ip,
            cf_ray,
            user_agent,
            false,
            &format!(
                "rejected sender path={} reason=peer not in Cloudflare ranges or trusted proxy allowlist",
                uri.path()
            ),
        );
        return StatusCode::FORBIDDEN;
    }

    if source_is_trusted_proxy && !has_cloudflare_headers {
        log_telegram_webhook_request(
            &state.audit_log_path,
            peer_ip,
            cf_connecting_ip,
            cf_ray,
            user_agent,
            false,
            &format!(
                "rejected sender path={} reason=trusted proxy request missing Cloudflare headers",
                uri.path()
            ),
        );
        return StatusCode::FORBIDDEN;
    }

    if let Some(expected) = state.secret_token.as_deref() {
        let provided = header_value(&headers, TELEGRAM_SECRET_HEADER);
        if provided != Some(expected) {
            log_telegram_webhook_request(
                &state.audit_log_path,
                peer_ip,
                cf_connecting_ip,
                cf_ray,
                user_agent,
                false,
                &format!("rejected invalid secret token path={}", uri.path()),
            );
            return StatusCode::UNAUTHORIZED;
        }
    }

    let update = match serde_json::from_slice::<frankenstein::Update>(&body) {
        Ok(update) => update,
        Err(err) => {
            log_telegram_webhook_request(
                &state.audit_log_path,
                peer_ip,
                cf_connecting_ip,
                cf_ray,
                user_agent,
                false,
                &format!("rejected invalid json path={} error={err}", uri.path()),
            );
            return StatusCode::BAD_REQUEST;
        }
    };

    if state.tx.send(update).is_err() {
        log_telegram_webhook_request(
            &state.audit_log_path,
            peer_ip,
            cf_connecting_ip,
            cf_ray,
            user_agent,
            false,
            &format!("rejected unavailable processor path={}", uri.path()),
        );
        return StatusCode::SERVICE_UNAVAILABLE;
    }

    log_telegram_webhook_request(
        &state.audit_log_path,
        peer_ip,
        cf_connecting_ip,
        cf_ray,
        user_agent,
        true,
        &format!("accepted path={} bytes={}", uri.path(), body.len()),
    );

    StatusCode::OK
}

async fn run_telegram_webhook_server(
    listener: tokio::net::TcpListener,
    secret_token: Option<String>,
    tx: mpsc::UnboundedSender<frankenstein::Update>,
    audit_log_path: PathBuf,
    cloudflare_ip_ranges: Arc<[IpNet]>,
    trusted_proxy_cidrs: Arc<[IpNet]>,
) -> Result<()> {
    let app_state = TelegramWebhookAppState {
        secret_token,
        tx,
        audit_log_path,
        cloudflare_ip_ranges,
        trusted_proxy_cidrs,
    };
    let app = Router::new()
        .route("/", post(telegram_webhook_handler))
        .route("/{*path}", post(telegram_webhook_handler))
        .with_state(app_state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("Telegram webhook server exited unexpectedly")
}

fn handle_telegram_query_message(
    db: &mut Connection,
    config: &mut AppConfig,
    config_path: &Path,
    state: &mut TelegramRuntimeState,
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
    } else if is_help_command(text) {
        "/help"
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
            let delivered = send_access_request_to_admins(
                &config.telegram.bot_token,
                &config.telegram.admin_user_ids,
                from_user.id,
            )?;
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
            send_text_to_chat_with_command_keyboard(
                &config.telegram.bot_token,
                chat_id,
                &response,
                reply_to_message_id,
                false,
                false,
            )?;
            return Ok(());
        }
        send_text_to_chat_with_command_keyboard(
            &config.telegram.bot_token,
            chat_id,
            &format!(
                "Unauthorized user.\nSend /join to request access.\nYour Telegram user ID: {}",
                from_user.id
            ),
            reply_to_message_id,
            false,
            false,
        )?;
        return Ok(());
    }

    log_telegram_message_usage(config, message, true, action, detail);

    if is_join_command(text) {
        send_text_to_chat_with_command_keyboard(
            &config.telegram.bot_token,
            chat_id,
            "You already have access.",
            reply_to_message_id,
            true,
            is_admin,
        )?;
        return Ok(());
    }

    if let Some(requested_user_id) = parse_approve_command(text) {
        if !is_admin {
            send_text_to_chat_with_command_keyboard(
                &config.telegram.bot_token,
                chat_id,
                "Admin only command.",
                reply_to_message_id,
                true,
                is_admin,
            )?;
            return Ok(());
        }
        let added = grant_telegram_user_access(config_path, config, requested_user_id)?;
        let response = if added {
            format!("Approved Telegram user ID {requested_user_id}.")
        } else {
            format!("Telegram user ID {requested_user_id} already has access.")
        };
        send_text_to_chat_with_command_keyboard(
            &config.telegram.bot_token,
            chat_id,
            &response,
            reply_to_message_id,
            true,
            is_admin,
        )?;
        if added {
            notify_user_access_approved(&config.telegram.bot_token, requested_user_id)?;
        }
        return Ok(());
    }
    if is_approve_command(text) {
        send_text_to_chat_with_command_keyboard(
            &config.telegram.bot_token,
            chat_id,
            "Usage: /approve <telegram_user_id>",
            reply_to_message_id,
            true,
            is_admin,
        )?;
        return Ok(());
    }

    if is_new_command(text) {
        state.awaiting_new_chats.insert(chat_id);
        send_text_to_chat_with_command_keyboard(
            &config.telegram.bot_token,
            chat_id,
            "Ready to receive new content. Send plain text or upload a .txt/.md file in your next message.",
            reply_to_message_id,
            true,
            is_admin,
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
                &stored_doc.header,
                stored_doc.id,
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
                &stored_doc.header,
                stored_doc.id,
                reply_to_message_id,
            )?;
            return Ok(());
        }
    }

    if text.is_empty() {
        if state.awaiting_new_chats.contains(&chat_id) {
            send_text_to_chat_with_command_keyboard(
                &config.telegram.bot_token,
                chat_id,
                "Send plain text or upload a .txt/.md file.",
                reply_to_message_id,
                true,
                is_admin,
            )?;
        }
        return Ok(());
    }

    if is_recent_documents_command(text) {
        let latest_docs = find_latest_documents(db, 10)?;
        if latest_docs.is_empty() {
            send_text_to_chat_with_command_keyboard(
                &config.telegram.bot_token,
                chat_id,
                "No documents are stored yet.",
                reply_to_message_id,
                true,
                is_admin,
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
        send_text_to_chat_with_command_keyboard(
            &config.telegram.bot_token,
            chat_id,
            &response,
            reply_to_message_id,
            true,
            is_admin,
        )?;
        return Ok(());
    }
    let Some(query) = parse_search_command(text) else {
        let help = telegram_help_message(text, is_admin);
        send_text_to_chat_with_command_keyboard(
            &config.telegram.bot_token,
            chat_id,
            &help,
            reply_to_message_id,
            true,
            is_admin,
        )?;
        return Ok(());
    };

    if query.is_empty() {
        send_text_to_chat_with_command_keyboard(
            &config.telegram.bot_token,
            chat_id,
            "Usage: /s <keywords>",
            reply_to_message_id,
            true,
            is_admin,
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
            send_text_to_chat_with_command_keyboard(
                &config.telegram.bot_token,
                chat_id,
                "No documents matched that query above the configured threshold. Try more specific keywords or lower telegram.match_accuracy.",
                reply_to_message_id,
                true,
                is_admin,
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

    let params = GetFileParams::builder().file_id(&document.file_id).build();
    let file = with_telegram_api_retry(bot_token, "getFile", |api| api.get_file(&params))
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
    state: &mut TelegramRuntimeState,
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
        answer_callback_query_if_possible(
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
            answer_callback_query_if_possible(
                &config.telegram.bot_token,
                &callback_query.id,
                Some("Admin only."),
            )?;
            return Ok(());
        }
        let added = grant_telegram_user_access(config_path, config, requested_user_id)?;
        answer_callback_query_if_possible(
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
            answer_callback_query_if_possible(
                &config.telegram.bot_token,
                &callback_query.id,
                Some("This page is no longer available."),
            )?;
            return Ok(());
        };
        let Some(session) = state.search_sessions.get(&session_id) else {
            answer_callback_query_if_possible(
                &config.telegram.bot_token,
                &callback_query.id,
                Some("Search results expired. Run /s again."),
            )?;
            return Ok(());
        };
        let max_page = search_results_page_count(session).saturating_sub(1);
        let page = page.min(max_page);
        answer_callback_query_if_possible(&config.telegram.bot_token, &callback_query.id, None)?;
        edit_search_results_message(
            &config.telegram.bot_token,
            message.chat.id,
            message.message_id,
            session_id,
            session,
            page,
        )?;
        return Ok(());
    }
    let Some(doc_id) = parse_document_callback(data) else {
        return Ok(());
    };
    let Some(message) = callback_query.message.as_ref() else {
        answer_callback_query_if_possible(
            &config.telegram.bot_token,
            &callback_query.id,
            Some("This selection is no longer available."),
        )?;
        return Ok(());
    };

    let Some(doc_match) = find_document_by_id(db, doc_id)? else {
        answer_callback_query_if_possible(
            &config.telegram.bot_token,
            &callback_query.id,
            Some("Document not found."),
        )?;
        return Ok(());
    };

    if !doc_match.path.exists() {
        answer_callback_query_if_possible(
            &config.telegram.bot_token,
            &callback_query.id,
            Some("The file is no longer available."),
        )?;
        return Ok(());
    }

    answer_callback_query_if_possible(
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

fn answer_callback_query_if_possible(
    bot_token: &str,
    callback_query_id: &str,
    text: Option<&str>,
) -> Result<()> {
    match answer_callback_query(bot_token, callback_query_id, text) {
        Ok(()) => Ok(()),
        Err(err) if is_expired_callback_query_error(&err) => {
            eprintln!("Telegram callback query expired before it could be answered: {err}");
            Ok(())
        }
        Err(err) => {
            eprintln!("Telegram callback query could not be answered, continuing anyway: {err}");
            Ok(())
        }
    }
}

fn is_expired_callback_query_error(err: &anyhow::Error) -> bool {
    let message = err.to_string().to_ascii_lowercase();
    message.contains("query is too old")
        || message.contains("response timeout expired")
        || message.contains("query id is invalid")
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
        format!(
            "Commands:\n/start show the latest 10 documents\n/show show the latest 10 documents\n/new receive plain text or a .txt/.md file and store it\n/s <keywords> search similar documents\n/clean remove duplicate rows from the database\n/join request access\n/help show this help{admin_line}"
        )
    } else if trimmed.starts_with('/') {
        format!(
            "Unknown command.\n\nCommands:\n/start show the latest 10 documents\n/show show the latest 10 documents\n/new receive plain text or a .txt/.md file and store it\n/s <keywords> search similar documents\n/clean remove duplicate rows from the database\n/join request access\n/help show this help{admin_line}"
        )
    } else {
        format!(
            "Send a command to continue.\n\nCommands:\n/start show the latest 10 documents\n/show show the latest 10 documents\n/new receive plain text or a .txt/.md file and store it\n/s <keywords> search similar documents\n/clean remove duplicate rows from the database\n/join request access\n/help show this help{admin_line}"
        )
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
        Err(err) => DoctorCheck::fail(name, format!("{err:#}"), fix),
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
    if detail.contains("telegram.mode is webhook")
        || detail.contains("telegram.webhook_bind_addr")
        || detail.contains("telegram.webhook_secret_token")
        || detail.contains("telegram.webhook_url must start with https://")
        || detail.contains("telegram.webhook_trusted_proxy_cidrs")
    {
        fixes.push(format!(
            "If you use webhook mode in {}, set telegram.webhook_url to the public HTTPS endpoint, telegram.webhook_bind_addr to the local listen address, telegram.webhook_secret_token to a random token, and telegram.webhook_trusted_proxy_cidrs only to proxies you control. The webhook listener only accepts direct Cloudflare senders or configured trusted proxies carrying Cloudflare headers.",
            config_path.display()
        ));
    }
    if detail.contains("Cloudflare IP range") {
        fixes.push(
            "Make sure this host can reach https://www.cloudflare.com/ips-v4 and https://www.cloudflare.com/ips-v6 so webhook security checks can load Cloudflare's current IP ranges."
                .to_string(),
        );
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

    let created = create_default_config_if_missing(&config_path)?;
    if created {
        let check = DoctorCheck::warn(
            "config",
            format!("created a new config template at {}", config_path.display()),
            format!(
                "Edit {}, fill in watch_dir, embedding providers, and optional Telegram settings, then rerun --doctor.",
                config_path.display()
            ),
        );
        print_doctor_report(&[check]);
        bail!("doctor created a new config template")
    }

    interactive_doctor_prepare_config(&config_path)?;

    let loaded_config = match AppConfig::load_or_create(Some(config_path.clone())) {
        Ok(loaded_config) => loaded_config,
        Err(err) => {
            let detail = format!("{err:#}");
            let check =
                DoctorCheck::fail("config", &detail, config_error_fix(&config_path, &detail));
            print_doctor_report(&[check]);
            bail!("doctor found configuration errors")
        }
    };

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
        checks.push(DoctorCheck::ok(
            "telegram.mode",
            format!("using {}", config.telegram.mode.as_str()),
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
        if config.telegram.mode == TelegramMode::Webhook {
            checks.push(DoctorCheck::ok(
                "telegram.webhook",
                format!(
                    "listening on {} and registering {}",
                    config.telegram.webhook_bind_addr, config.telegram.webhook_url
                ),
            ));
            checks.push(run_check(
                "telegram.webhook.bind",
                format!(
                    "Free the port in {} if another process is already listening on {}.",
                    loaded_config.path.display(),
                    config.telegram.webhook_bind_addr
                ),
                || {
                    let bind_addr = config
                        .telegram
                        .webhook_bind_addr
                        .parse::<SocketAddr>()
                        .with_context(|| {
                            format!(
                                "failed to parse telegram.webhook_bind_addr {}",
                                config.telegram.webhook_bind_addr
                            )
                        })?;
                    can_bind_telegram_webhook(bind_addr)
                },
            ));
            checks.push(run_check(
                "telegram.webhook.cloudflare",
                "Allow outbound HTTPS to Cloudflare so the app can verify Cloudflare edge IP ranges before accepting webhook traffic.",
                || {
                    let ranges = fetch_cloudflare_ip_ranges()?;
                    Ok(format!(
                        "loaded {} Cloudflare edge IP ranges; webhook will only accept those senders",
                        ranges.len()
                    ))
                },
            ));
            checks.push(run_check(
                "telegram.webhook.trusted_proxy_cidrs",
                format!(
                    "Set {} in {} to only loopback or specific reverse proxies you control.",
                    ui::value("telegram.webhook_trusted_proxy_cidrs"),
                    loaded_config.path.display()
                ),
                || {
                    let cidrs =
                        parse_trusted_proxy_cidrs(&config.telegram.webhook_trusted_proxy_cidrs)?;
                    Ok(format!(
                        "trusted proxy CIDRs: {}",
                        cidrs
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                },
            ));
            checks.push(DoctorCheck::ok(
                "telegram.webhook_secret_token",
                "configured".to_string(),
            ));
            checks.push(DoctorCheck::ok(
                "telegram.webhook.audit",
                format!(
                    "webhook sender audit lines will be written to {}",
                    config.telegram.audit_log_path.display()
                ),
            ));
        }
        if config.telegram.chat_id == 0 {
            let fix = if config.telegram.mode == TelegramMode::Polling {
                format!(
                    "Run {} after messaging the bot if you want the app to push new-document notifications to a default chat.",
                    ui::command("note-embedding --telegram-discover-chat")
                )
            } else {
                format!(
                    "Set {} manually in {} if you want the app to push new-document notifications to a default chat. --telegram-discover-chat only works in polling mode.",
                    ui::value("telegram.chat_id"),
                    loaded_config.path.display()
                )
            };
            checks.push(DoctorCheck::warn(
                "telegram.notifications",
                "chat_id is not configured; proactive document notifications are disabled",
                fix,
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
    match find_document_by_path(db, &processed_path)? {
        Some(stored_doc) => {
            if let Err(err) = send_to_telegram_with_document_button(
                &config.telegram,
                &message,
                &stored_doc.header,
                stored_doc.id,
            ) {
                eprintln!("TG 发送失败: {err}");
            }
        }
        None => {
            if let Err(err) = send_to_telegram(&config.telegram, &message) {
                eprintln!("TG 发送失败: {err}");
            }
        }
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

    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
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
    if cli.telegram_delete_webhook {
        return delete_telegram_webhook_command(cli.config_path());
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
    install_telegram_audit_log_path(&config.telegram.audit_log_path);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (telegram_update_tx, mut telegram_update_rx) = mpsc::unbounded_channel();
    let mut db = init_db(&config.database_path)?;
    init_processed_dir(&config.processed_dir)?;
    let mut telegram_state = TelegramRuntimeState::default();
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
        if config.telegram.mode == TelegramMode::Webhook {
            let bind_addr = config
                .telegram
                .webhook_bind_addr
                .parse::<SocketAddr>()
                .with_context(|| {
                    format!(
                        "failed to parse telegram.webhook_bind_addr {}",
                        config.telegram.webhook_bind_addr
                    )
                })?;
            let cloudflare_ip_ranges: Arc<[IpNet]> =
                fetch_cloudflare_ip_ranges()?.into_boxed_slice().into();
            let trusted_proxy_cidrs: Arc<[IpNet]> =
                parse_trusted_proxy_cidrs(&config.telegram.webhook_trusted_proxy_cidrs)?
                    .into_boxed_slice()
                    .into();
            let listener = tokio::net::TcpListener::bind(bind_addr)
                .await
                .with_context(|| {
                    format!("failed to bind Telegram webhook listener to {bind_addr}")
                })?;
            let secret_token = (!config.telegram.webhook_secret_token.is_empty())
                .then(|| config.telegram.webhook_secret_token.clone());
            let audit_log_path = config.telegram.audit_log_path.clone();
            tokio::spawn(async move {
                if let Err(err) = run_telegram_webhook_server(
                    listener,
                    secret_token,
                    telegram_update_tx,
                    audit_log_path,
                    cloudflare_ip_ranges,
                    trusted_proxy_cidrs,
                )
                .await
                {
                    eprintln!("Telegram webhook server failed: {err}");
                }
            });
        }
        configure_telegram_transport(&config.telegram)?;
        if let Err(err) = register_bot_commands(&config.telegram) {
            eprintln!("Failed to register Telegram bot commands: {err}");
        }
        match config.telegram.mode {
            TelegramMode::Polling => {
                println!(
                    "Telegram transport: polling every {} second(s), match accuracy {}",
                    ui::value(config.telegram.poll_interval_secs),
                    ui::value(format!("{:.2}", config.telegram.match_accuracy))
                );
            }
            TelegramMode::Webhook => {
                println!(
                    "Telegram transport: webhook {} -> {}, match accuracy {}",
                    ui::value(config.telegram.webhook_bind_addr.as_str()),
                    ui::value(config.telegram.webhook_url.as_str()),
                    ui::value(format!("{:.2}", config.telegram.match_accuracy))
                );
                println!(
                    "Telegram webhook sender filter: {}",
                    ui::detail("Cloudflare edge IPs or trusted proxies with Cloudflare headers")
                );
                println!(
                    "Telegram trusted proxy CIDRs: {}",
                    ui::detail(config.telegram.webhook_trusted_proxy_cidrs.join(", "))
                );
                println!(
                    "Telegram webhook secret token: {}",
                    ui::detail(if config.telegram.webhook_secret_token.is_empty() {
                        "not configured"
                    } else {
                        "configured"
                    })
                );
            }
        }
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
            maybe_update = telegram_update_rx.recv(), if config.telegram.enabled && config.telegram.mode == TelegramMode::Webhook => {
                let Some(update) = maybe_update else {
                    break;
                };
                process_telegram_update(
                    &mut db,
                    &mut config,
                    &loaded_config.path,
                    &mut telegram_state,
                    update,
                );
            }
            _ = telegram_interval.tick(), if config.telegram.enabled && config.telegram.mode == TelegramMode::Polling => {
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
