use crate::config::{AppConfig, TelegramConfig};
use crate::ui;
use anyhow::{Context, Result};
use frankenstein::{
    BotCommand, BotCommandScope, BotCommandScopeChatMember,
    CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, KeyboardButton, Message, ParseMode,
    ReplyKeyboardMarkup, ReplyMarkup, SendDocumentParams, SendMessageParams, SetMyCommandsParams,
    TelegramApi,
};
use pulldown_cmark::{
    CodeBlockKind, Event as MarkdownEvent, Parser as MarkdownParser, Tag, TagEnd,
};
use std::convert::TryFrom;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const TELEGRAM_TEXT_MESSAGE_MAX_CHARS: usize = 3800;
const TELEGRAM_AUDIT_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

pub(crate) fn register_bot_commands(config: &TelegramConfig) -> Result<()> {
    if !config.enabled || config.bot_token.trim().is_empty() {
        return Ok(());
    }

    let api = frankenstein::Api::new(&config.bot_token);
    let default_params = SetMyCommandsParams::builder()
        .commands(default_bot_commands())
        .build();
    api.set_my_commands(&default_params)?;

    for admin_user_id in &config.admin_user_ids {
        let Ok(chat_id) = i64::try_from(*admin_user_id) else {
            continue;
        };
        let scope = BotCommandScope::ChatMember(
            BotCommandScopeChatMember::builder()
                .chat_id(chat_id)
                .user_id(*admin_user_id)
                .build(),
        );
        let admin_params = SetMyCommandsParams::builder()
            .commands(admin_bot_commands())
            .scope(scope)
            .build();
        let _ = api.set_my_commands(&admin_params);
    }

    Ok(())
}

fn default_bot_commands() -> Vec<BotCommand> {
    vec![
        BotCommand::builder()
            .command("start")
            .description("Show the latest 10 documents")
            .build(),
        BotCommand::builder()
            .command("show")
            .description("Show the latest 10 documents")
            .build(),
        BotCommand::builder()
            .command("new")
            .description("Store a new text or text file")
            .build(),
        BotCommand::builder()
            .command("s")
            .description("Search similar documents")
            .build(),
        BotCommand::builder()
            .command("clean")
            .description("Remove duplicate database rows")
            .build(),
        BotCommand::builder()
            .command("join")
            .description("Request access to the bot")
            .build(),
        BotCommand::builder()
            .command("help")
            .description("Show help")
            .build(),
    ]
}

fn admin_bot_commands() -> Vec<BotCommand> {
    let mut commands = default_bot_commands();
    commands.push(
        BotCommand::builder()
            .command("approve")
            .description("Approve a Telegram user ID")
            .build(),
    );
    commands
}

pub(crate) fn send_to_telegram(config: &TelegramConfig, message: &str) -> Result<()> {
    if !config.enabled || config.chat_id == 0 {
        return Ok(());
    }

    send_text_to_chat(&config.bot_token, config.chat_id, message, None)
}

pub(crate) fn send_to_telegram_with_document_button(
    config: &TelegramConfig,
    message: &str,
    document_header: &str,
    document_id: i64,
) -> Result<()> {
    if !config.enabled || config.chat_id == 0 {
        return Ok(());
    }

    send_text_to_chat_with_document_button(
        &config.bot_token,
        config.chat_id,
        message,
        document_header,
        document_id,
        None,
    )
}

pub(crate) fn send_text_to_chat(
    bot_token: &str,
    chat_id: i64,
    message: &str,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    send_text_to_chat_with_parse_mode_and_markup(
        bot_token,
        chat_id,
        message,
        reply_to_message_id,
        None,
        None,
    )
}

pub(crate) fn send_text_to_chat_with_parse_mode(
    bot_token: &str,
    chat_id: i64,
    message: &str,
    reply_to_message_id: Option<i32>,
    parse_mode: Option<ParseMode>,
) -> Result<()> {
    send_text_to_chat_with_parse_mode_and_markup(
        bot_token,
        chat_id,
        message,
        reply_to_message_id,
        parse_mode,
        None,
    )
}

fn send_text_to_chat_with_parse_mode_and_markup(
    bot_token: &str,
    chat_id: i64,
    message: &str,
    reply_to_message_id: Option<i32>,
    parse_mode: Option<ParseMode>,
    reply_markup: Option<ReplyMarkup>,
) -> Result<()> {
    let api = frankenstein::Api::new(bot_token);
    let send_message_params = match (reply_to_message_id, parse_mode, reply_markup) {
        (Some(reply_to_message_id), Some(parse_mode), Some(reply_markup)) => {
            SendMessageParams::builder()
                .chat_id(chat_id)
                .text(message)
                .parse_mode(parse_mode)
                .reply_to_message_id(reply_to_message_id)
                .reply_markup(reply_markup)
                .build()
        }
        (Some(reply_to_message_id), Some(parse_mode), None) => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .parse_mode(parse_mode)
            .reply_to_message_id(reply_to_message_id)
            .build(),
        (Some(reply_to_message_id), None, Some(reply_markup)) => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .reply_to_message_id(reply_to_message_id)
            .reply_markup(reply_markup)
            .build(),
        (Some(reply_to_message_id), None, None) => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .reply_to_message_id(reply_to_message_id)
            .build(),
        (None, Some(parse_mode), Some(reply_markup)) => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .parse_mode(parse_mode)
            .reply_markup(reply_markup)
            .build(),
        (None, Some(parse_mode), None) => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .parse_mode(parse_mode)
            .build(),
        (None, None, Some(reply_markup)) => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .reply_markup(reply_markup)
            .build(),
        (None, None, None) => SendMessageParams::builder()
            .chat_id(chat_id)
            .text(message)
            .build(),
    };
    api.send_message(&send_message_params)?;
    Ok(())
}

fn build_telegram_command_keyboard(is_allowed: bool, is_admin: bool) -> ReplyKeyboardMarkup {
    let keyboard = if !is_allowed {
        vec![vec![KeyboardButton::builder().text("/join").build()]]
    } else if is_admin {
        vec![
            vec![
                KeyboardButton::builder().text("/start").build(),
                KeyboardButton::builder().text("/show").build(),
            ],
            vec![
                KeyboardButton::builder().text("/new").build(),
                KeyboardButton::builder().text("/s").build(),
            ],
            vec![
                KeyboardButton::builder().text("/clean").build(),
                KeyboardButton::builder().text("/approve").build(),
            ],
            vec![KeyboardButton::builder().text("/help").build()],
        ]
    } else {
        vec![
            vec![
                KeyboardButton::builder().text("/start").build(),
                KeyboardButton::builder().text("/show").build(),
            ],
            vec![
                KeyboardButton::builder().text("/new").build(),
                KeyboardButton::builder().text("/s").build(),
            ],
            vec![
                KeyboardButton::builder().text("/clean").build(),
                KeyboardButton::builder().text("/help").build(),
            ],
        ]
    };

    ReplyKeyboardMarkup::builder()
        .keyboard(keyboard)
        .resize_keyboard(true)
        .input_field_placeholder("Choose a command")
        .build()
}

pub(crate) fn send_text_to_chat_with_command_keyboard(
    bot_token: &str,
    chat_id: i64,
    message: &str,
    reply_to_message_id: Option<i32>,
    is_allowed: bool,
    is_admin: bool,
) -> Result<()> {
    send_text_to_chat_with_parse_mode_and_markup(
        bot_token,
        chat_id,
        message,
        reply_to_message_id,
        None,
        Some(ReplyMarkup::ReplyKeyboardMarkup(
            build_telegram_command_keyboard(is_allowed, is_admin),
        )),
    )
}

pub(crate) fn send_text_to_chat_with_document_button(
    bot_token: &str,
    chat_id: i64,
    message: &str,
    document_header: &str,
    document_id: i64,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    let api = frankenstein::Api::new(bot_token);
    let keyboard = InlineKeyboardMarkup::builder()
        .inline_keyboard(vec![vec![
            InlineKeyboardButton::builder()
                .text(format_button_label(document_header))
                .callback_data(format!("doc:{document_id}"))
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

fn send_large_text_to_chat(
    bot_token: &str,
    chat_id: i64,
    message: &str,
    reply_to_message_id: Option<i32>,
    parse_mode: Option<ParseMode>,
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

fn split_text_for_telegram(text: &str, max_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;

    for line in text.lines() {
        let line_len = line.chars().count();
        let separator_len = usize::from(!current.is_empty());

        if !current.is_empty() && current_len + separator_len + line_len > max_chars {
            chunks.push(current);
            current = String::new();
            current_len = 0;
        }

        if line_len > max_chars {
            if !current.is_empty() {
                chunks.push(current);
                current = String::new();
                current_len = 0;
            }

            let chars: Vec<char> = line.chars().collect();
            let mut start = 0;
            while start < chars.len() {
                let end = usize::min(start + max_chars, chars.len());
                chunks.push(chars[start..end].iter().collect());
                start = end;
            }
            continue;
        }

        if !current.is_empty() {
            current.push('\n');
            current_len += 1;
        }
        current.push_str(line);
        current_len += line_len;
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    if chunks.is_empty() {
        chunks.push(text.chars().take(max_chars).collect());
    }

    chunks
}

fn escape_telegram_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_markdown_to_telegram_html(markdown: &str) -> String {
    #[derive(Clone, Copy)]
    struct ListState {
        ordered: bool,
        next_index: u64,
    }

    let mut output = String::new();
    let mut list_stack: Vec<ListState> = Vec::new();
    let mut blockquote_depth = 0usize;
    let mut in_code_block = false;
    let parser = MarkdownParser::new(markdown);

    for event in parser {
        match event {
            MarkdownEvent::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => output.push_str("<b>"),
                Tag::Emphasis => output.push_str("<i>"),
                Tag::Strong => output.push_str("<b>"),
                Tag::Strikethrough => output.push_str("<s>"),
                Tag::BlockQuote(_) => {
                    if !output.ends_with('\n') && !output.is_empty() {
                        output.push('\n');
                    }
                    blockquote_depth += 1;
                    output.push_str("&gt; ");
                }
                Tag::CodeBlock(kind) => {
                    in_code_block = true;
                    if !output.ends_with('\n') && !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str("<pre>");
                    if let CodeBlockKind::Fenced(language) = kind {
                        let language = escape_telegram_html(language.as_ref());
                        if !language.is_empty() {
                            output.push_str(&format!("{language}\n"));
                        }
                    }
                }
                Tag::List(first_number) => list_stack.push(ListState {
                    ordered: first_number.is_some(),
                    next_index: first_number.unwrap_or(1),
                }),
                Tag::Item => {
                    if !output.ends_with('\n') && !output.is_empty() {
                        output.push('\n');
                    }
                    let indent = "  ".repeat(list_stack.len().saturating_sub(1));
                    output.push_str(&indent);
                    if let Some(list) = list_stack.last_mut() {
                        if list.ordered {
                            output.push_str(&format!("{}. ", list.next_index));
                            list.next_index += 1;
                        } else {
                            output.push_str("• ");
                        }
                    } else {
                        output.push_str("• ");
                    }
                }
                Tag::Link { dest_url, .. } => {
                    let href = escape_telegram_html(dest_url.as_ref());
                    output.push_str(&format!("<a href=\"{href}\">"));
                }
                _ => {}
            },
            MarkdownEvent::End(tag) => match tag {
                TagEnd::Paragraph => output.push_str("\n\n"),
                TagEnd::Heading(_) => output.push_str("</b>\n\n"),
                TagEnd::Emphasis => output.push_str("</i>"),
                TagEnd::Strong => output.push_str("</b>"),
                TagEnd::Strikethrough => output.push_str("</s>"),
                TagEnd::BlockQuote(_) => {
                    blockquote_depth = blockquote_depth.saturating_sub(1);
                    output.push_str("\n\n");
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    output.push_str("</pre>\n\n");
                }
                TagEnd::List(_) => output.push('\n'),
                TagEnd::Item => output.push('\n'),
                TagEnd::Link => output.push_str("</a>"),
                _ => {}
            },
            MarkdownEvent::Text(text) => {
                let escaped = escape_telegram_html(text.as_ref());
                if blockquote_depth > 0 && output.ends_with('\n') {
                    output.push_str("&gt; ");
                }
                output.push_str(&escaped);
            }
            MarkdownEvent::Code(code) => {
                output.push_str("<code>");
                output.push_str(&escape_telegram_html(code.as_ref()));
                output.push_str("</code>");
            }
            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => {
                output.push('\n');
                if blockquote_depth > 0 && !in_code_block {
                    output.push_str("&gt; ");
                }
            }
            MarkdownEvent::Rule => output.push_str("\n────\n"),
            MarkdownEvent::Html(html) | MarkdownEvent::InlineHtml(html) => {
                output.push_str(&escape_telegram_html(html.as_ref()));
            }
            MarkdownEvent::FootnoteReference(text) => {
                output.push_str(&escape_telegram_html(text.as_ref()));
            }
            MarkdownEvent::InlineMath(text) | MarkdownEvent::DisplayMath(text) => {
                output.push_str(&escape_telegram_html(text.as_ref()));
            }
            MarkdownEvent::TaskListMarker(checked) => {
                output.push_str(if checked { "☑ " } else { "☐ " });
            }
        }
    }

    output.trim().to_string()
}

fn send_markdown_content_to_chat(
    bot_token: &str,
    chat_id: i64,
    content: &str,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    let chunks =
        split_text_for_telegram(content, TELEGRAM_TEXT_MESSAGE_MAX_CHARS.saturating_sub(64));
    for (index, chunk) in chunks.iter().enumerate() {
        let rendered = render_markdown_to_telegram_html(chunk);
        let message = if index == 0 {
            format!("<b>Content</b>\n\n{rendered}")
        } else {
            rendered
        };
        if send_text_to_chat_with_parse_mode(
            bot_token,
            chat_id,
            &message,
            reply_to_message_id,
            Some(ParseMode::Html),
        )
        .is_err()
        {
            let fallback = if index == 0 {
                format!("Content:\n\n{chunk}")
            } else {
                chunk.clone()
            };
            send_text_to_chat(bot_token, chat_id, &fallback, reply_to_message_id)?;
        }
    }
    Ok(())
}

pub(crate) fn send_document_content_to_chat(
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

    if looks_like_markdown(path, &content) {
        return send_markdown_content_to_chat(bot_token, chat_id, &content, reply_to_message_id);
    }

    let message = format!("Content:\n\n{content}");
    send_large_text_to_chat(bot_token, chat_id, &message, reply_to_message_id, None)
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

fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
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

pub(crate) fn log_telegram_message_usage(
    config: &AppConfig,
    message: &Message,
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

pub(crate) fn log_telegram_callback_usage(
    config: &AppConfig,
    callback_query: &CallbackQuery,
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

pub(crate) fn send_access_request_to_admins(
    bot_token: &str,
    admin_user_ids: &[u64],
    requester_user_id: u64,
) -> Result<usize> {
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

pub(crate) fn send_document_to_chat(
    bot_token: &str,
    chat_id: i64,
    document_path: &Path,
    caption: &str,
    reply_to_message_id: Option<i32>,
) -> Result<()> {
    let api = frankenstein::Api::new(bot_token);
    let send_document_params = match reply_to_message_id {
        Some(reply_to_message_id) => SendDocumentParams::builder()
            .chat_id(chat_id)
            .document(document_path.to_path_buf())
            .caption(caption.to_string())
            .reply_to_message_id(reply_to_message_id)
            .build(),
        None => SendDocumentParams::builder()
            .chat_id(chat_id)
            .document(document_path.to_path_buf())
            .caption(caption.to_string())
            .build(),
    };
    api.send_document(&send_document_params)?;
    Ok(())
}

pub(crate) fn answer_callback_query(
    bot_token: &str,
    callback_query_id: &str,
    text: Option<&str>,
) -> Result<()> {
    use frankenstein::AnswerCallbackQueryParams;

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

pub(crate) fn notify_user_access_approved(bot_token: &str, user_id: u64) -> Result<()> {
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
