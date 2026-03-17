use anyhow::{Context, Result, bail};
use directories::{BaseDirs, ProjectDirs};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

const APP_DIR_NAME: &str = "note-embedding";
const DEFAULT_CONFIG_NAME: &str = "config";

#[derive(Debug)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub data: AppConfig,
    pub created: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_watch_dir")]
    pub watch_dir: PathBuf,
    #[serde(default = "default_database_path")]
    pub database_path: PathBuf,
    #[serde(default = "default_processed_dir")]
    pub processed_dir: PathBuf,
    #[serde(default = "default_file_types")]
    pub file_types: Vec<String>,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub admin_user_ids: Vec<u64>,
    #[serde(default)]
    pub allowed_user_ids: Vec<u64>,
    #[serde(default = "default_telegram_audit_log_path")]
    pub audit_log_path: PathBuf,
    #[serde(default)]
    pub chat_id: i64,
    #[serde(default)]
    pub mode: TelegramMode,
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default = "default_telegram_webhook_bind_addr")]
    pub webhook_bind_addr: String,
    #[serde(default)]
    pub webhook_secret_token: String,
    #[serde(default = "default_telegram_webhook_trusted_proxy_cidrs")]
    pub webhook_trusted_proxy_cidrs: Vec<String>,
    #[serde(default = "default_match_accuracy")]
    pub match_accuracy: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TelegramMode {
    #[default]
    Polling,
    Webhook,
}

impl TelegramMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Polling => "polling",
            Self::Webhook => "webhook",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    #[serde(default = "default_preferred_provider")]
    pub preferred_provider: String,
    #[serde(default = "default_fallback_provider")]
    pub fallback_provider: String,
    #[serde(default)]
    pub ollama: OllamaConfig,
    #[serde(default)]
    pub gemini: GeminiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_ollama_base_url")]
    pub base_url: String,
    #[serde(default = "default_ollama_model")]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_gemini_model")]
    pub model: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        let watch_dir = default_watch_dir();
        Self {
            watch_dir: watch_dir.clone(),
            database_path: default_database_path(),
            processed_dir: watch_dir.join(default_processed_dir_name()),
            file_types: default_file_types(),
            telegram: TelegramConfig::default(),
            embedding: EmbeddingConfig::default(),
        }
    }
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            admin_user_ids: Vec::new(),
            allowed_user_ids: Vec::new(),
            audit_log_path: default_telegram_audit_log_path(),
            chat_id: 0,
            mode: TelegramMode::default(),
            poll_interval_secs: default_poll_interval_secs(),
            webhook_url: String::new(),
            webhook_bind_addr: default_telegram_webhook_bind_addr(),
            webhook_secret_token: String::new(),
            webhook_trusted_proxy_cidrs: default_telegram_webhook_trusted_proxy_cidrs(),
            match_accuracy: default_match_accuracy(),
        }
    }
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            preferred_provider: default_preferred_provider(),
            fallback_provider: default_fallback_provider(),
            ollama: OllamaConfig::default(),
            gemini: GeminiConfig::default(),
        }
    }
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: default_ollama_base_url(),
            model: default_ollama_model(),
        }
    }
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
            model: default_gemini_model(),
        }
    }
}

impl AppConfig {
    pub fn load_or_create(config_override: Option<PathBuf>) -> Result<LoadedConfig> {
        let config_path = resolve_config_path(config_override)?;
        let created = ensure_default_config(&config_path)?;
        if created {
            return Ok(LoadedConfig {
                path: config_path,
                data: AppConfig::default(),
                created: true,
            });
        }

        let config_text = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read config at {}", config_path.display()))?;
        let mut data: AppConfig = toml::from_str(&config_text)
            .with_context(|| format!("failed to parse config at {}", config_path.display()))?;

        data.resolve_paths(&config_path);
        data.normalize();
        data.validate()?;

        Ok(LoadedConfig {
            path: config_path,
            data,
            created,
        })
    }

    fn resolve_paths(&mut self, config_path: &Path) {
        let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

        self.watch_dir = resolve_path(config_dir, &self.watch_dir);
        self.database_path = resolve_path(config_dir, &self.database_path);
        self.processed_dir =
            resolve_processed_dir(&self.watch_dir, config_dir, &self.processed_dir);
        self.telegram.audit_log_path = resolve_path(config_dir, &self.telegram.audit_log_path);
    }

    fn normalize(&mut self) {
        self.file_types = self
            .file_types
            .iter()
            .map(|file_type| normalize_file_type(file_type))
            .filter(|file_type| !file_type.is_empty())
            .collect();

        self.embedding.preferred_provider = self
            .embedding
            .preferred_provider
            .trim()
            .to_ascii_lowercase();
        self.embedding.fallback_provider =
            self.embedding.fallback_provider.trim().to_ascii_lowercase();
        self.telegram.bot_token = self.telegram.bot_token.trim().to_string();
        self.telegram.webhook_url = self.telegram.webhook_url.trim().to_string();
        self.telegram.webhook_bind_addr = self.telegram.webhook_bind_addr.trim().to_string();
        self.telegram.webhook_secret_token = self.telegram.webhook_secret_token.trim().to_string();
        self.telegram.webhook_trusted_proxy_cidrs = self
            .telegram
            .webhook_trusted_proxy_cidrs
            .iter()
            .map(|cidr| cidr.trim().to_string())
            .filter(|cidr| !cidr.is_empty())
            .collect();
        self.telegram.admin_user_ids.sort_unstable();
        self.telegram.admin_user_ids.dedup();
        self.telegram.allowed_user_ids.sort_unstable();
        self.telegram.allowed_user_ids.dedup();
    }

    fn validate(&self) -> Result<()> {
        let mut errors = Vec::new();

        if !self.watch_dir.exists() {
            errors.push(format!(
                "watch_dir does not exist: {}",
                self.watch_dir.display()
            ));
        }
        if !self.watch_dir.is_dir() {
            errors.push(format!(
                "watch_dir is not a directory: {}",
                self.watch_dir.display()
            ));
        }
        if self.processed_dir.exists() && !self.processed_dir.is_dir() {
            errors.push(format!(
                "processed_dir is not a directory: {}",
                self.processed_dir.display()
            ));
        }
        if self.processed_dir == self.watch_dir {
            errors.push("processed_dir must be different from watch_dir".to_string());
        }
        if self.file_types.is_empty() {
            errors.push("file_types must contain at least one extension".to_string());
        }
        if self.telegram.enabled && is_blank_or_placeholder(&self.telegram.bot_token) {
            errors.push("telegram.enabled is true but telegram.bot_token is empty".to_string());
        }
        match self.telegram.mode {
            TelegramMode::Polling => {
                if self.telegram.poll_interval_secs == 0 {
                    errors.push("telegram.poll_interval_secs must be greater than 0".to_string());
                }
            }
            TelegramMode::Webhook => {
                if self.telegram.enabled && is_blank_or_placeholder(&self.telegram.webhook_url) {
                    errors.push(
                        "telegram.mode is webhook but telegram.webhook_url is empty".to_string(),
                    );
                } else if !self.telegram.webhook_url.starts_with("https://") {
                    errors.push(
                        "telegram.webhook_url must start with https:// in webhook mode".to_string(),
                    );
                }
                if self.telegram.webhook_bind_addr.is_empty() {
                    errors.push(
                        "telegram.mode is webhook but telegram.webhook_bind_addr is empty"
                            .to_string(),
                    );
                } else if self
                    .telegram
                    .webhook_bind_addr
                    .parse::<SocketAddr>()
                    .is_err()
                {
                    errors.push(
                        "telegram.webhook_bind_addr must be a valid socket address like 127.0.0.1:8080"
                            .to_string(),
                    );
                }
                if self.telegram.enabled
                    && is_blank_or_placeholder(&self.telegram.webhook_secret_token)
                {
                    errors.push(
                        "telegram.mode is webhook but telegram.webhook_secret_token is empty"
                            .to_string(),
                    );
                } else if !self
                    .telegram
                    .webhook_secret_token
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
                {
                    errors.push(
                        "telegram.webhook_secret_token must contain only letters, numbers, underscores, or hyphens"
                            .to_string(),
                    );
                }
                for cidr in &self.telegram.webhook_trusted_proxy_cidrs {
                    if cidr.parse::<IpNet>().is_err() {
                        errors.push(format!(
                            "telegram.webhook_trusted_proxy_cidrs contains invalid CIDR {cidr}"
                        ));
                    }
                }
            }
        }
        if !(0.0..=1.0).contains(&self.telegram.match_accuracy) {
            errors.push("telegram.match_accuracy must be between 0.0 and 1.0".to_string());
        }
        if self
            .telegram
            .admin_user_ids
            .iter()
            .any(|user_id| *user_id == 0)
        {
            errors.push(
                "telegram.admin_user_ids must contain only positive Telegram user IDs".to_string(),
            );
        }
        if self
            .telegram
            .allowed_user_ids
            .iter()
            .any(|user_id| *user_id == 0)
        {
            errors.push(
                "telegram.allowed_user_ids must contain only positive Telegram user IDs"
                    .to_string(),
            );
        }
        if let Err(err) = validate_provider_name(
            "embedding.preferred_provider",
            &self.embedding.preferred_provider,
        ) {
            errors.push(err.to_string());
        }
        if let Err(err) = validate_provider_name(
            "embedding.fallback_provider",
            &self.embedding.fallback_provider,
        ) {
            errors.push(err.to_string());
        }
        let fallback_enabled = self.is_provider_enabled(&self.embedding.fallback_provider);
        if fallback_enabled && self.embedding.preferred_provider == self.embedding.fallback_provider
        {
            errors.push(
                "embedding.preferred_provider and embedding.fallback_provider must differ"
                    .to_string(),
            );
        }
        if let Err(err) = self.validate_provider_config(
            "embedding.preferred_provider",
            &self.embedding.preferred_provider,
        ) {
            errors.push(err.to_string());
        }
        if fallback_enabled {
            if let Err(err) = self.validate_provider_config(
                "embedding.fallback_provider",
                &self.embedding.fallback_provider,
            ) {
                errors.push(err.to_string());
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            bail!(errors.join("\n"))
        }
    }

    pub fn supports_path(&self, path: &Path) -> bool {
        if self.is_processed_path(path) {
            return false;
        }

        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            return false;
        };
        let normalized = normalize_file_type(extension);
        self.file_types.iter().any(|item| item == &normalized)
    }

    pub fn is_processed_path(&self, path: &Path) -> bool {
        path.starts_with(&self.processed_dir)
    }

    pub fn is_provider_enabled(&self, provider: &str) -> bool {
        match provider {
            "ollama" => self.embedding.ollama.enabled,
            "gemini" => self.embedding.gemini.enabled,
            _ => false,
        }
    }

    fn validate_provider_config(&self, field_name: &str, provider: &str) -> Result<()> {
        match provider {
            "ollama" => {
                if !self.embedding.ollama.enabled {
                    bail!("{field_name} is ollama but embedding.ollama.enabled is false");
                }
                if is_blank_or_placeholder(&self.embedding.ollama.base_url) {
                    bail!("{field_name} is ollama but embedding.ollama.base_url is empty");
                }
                if is_blank_or_placeholder(&self.embedding.ollama.model) {
                    bail!("{field_name} is ollama but embedding.ollama.model is empty");
                }
            }
            "gemini" => {
                if !self.embedding.gemini.enabled {
                    bail!("{field_name} is gemini but embedding.gemini.enabled is false");
                }
                if is_blank_or_placeholder(&self.embedding.gemini.api_key) {
                    bail!("{field_name} is gemini but embedding.gemini.api_key is empty");
                }
                if is_blank_or_placeholder(&self.embedding.gemini.model) {
                    bail!("{field_name} is gemini but embedding.gemini.model is empty");
                }
            }
            _ => bail!("{field_name} must be either 'ollama' or 'gemini'"),
        }
        Ok(())
    }
}

pub fn default_config_path() -> Result<PathBuf> {
    let project_dirs = project_dirs().context("failed to resolve project directories")?;
    Ok(project_dirs.config_dir().join(DEFAULT_CONFIG_NAME))
}

pub fn resolve_config_path(config_override: Option<PathBuf>) -> Result<PathBuf> {
    match config_override {
        Some(path) => Ok(expand_tilde(&path)),
        None => default_config_path().context("failed to resolve config path"),
    }
}

pub fn create_default_config_if_missing(path: &Path) -> Result<bool> {
    ensure_default_config(path)
}

pub fn populate_missing_config_fields(path: &Path, config: &AppConfig) -> Result<Vec<String>> {
    let original = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let mut current: toml::Value = toml::from_str(&original)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;
    let defaults =
        toml::Value::try_from(config.clone()).context("failed to serialize config defaults")?;
    let mut added = Vec::new();
    merge_missing_toml(&mut current, &defaults, "", &mut added);

    if added.is_empty() {
        return Ok(added);
    }

    let rewritten =
        toml::to_string_pretty(&current).context("failed to render updated config TOML")?;
    fs::write(path, rewritten)
        .with_context(|| format!("failed to write updated config at {}", path.display()))?;

    Ok(added)
}

fn ensure_default_config(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }

    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(config_dir)
        .with_context(|| format!("failed to create config directory {}", config_dir.display()))?;

    let default_config = render_default_config_template();
    fs::write(path, default_config)
        .with_context(|| format!("failed to write default config to {}", path.display()))?;

    Ok(true)
}

fn merge_missing_toml(
    current: &mut toml::Value,
    defaults: &toml::Value,
    prefix: &str,
    added: &mut Vec<String>,
) {
    let Some(current_table) = current.as_table_mut() else {
        return;
    };
    let Some(default_table) = defaults.as_table() else {
        return;
    };

    for (key, default_value) in default_table {
        let full_key = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };

        match current_table.get_mut(key) {
            Some(current_value) if current_value.is_table() && default_value.is_table() => {
                merge_missing_toml(current_value, default_value, &full_key, added);
            }
            Some(_) => {}
            None => {
                current_table.insert(key.clone(), default_value.clone());
                added.push(full_key);
            }
        }
    }
}

fn resolve_path(base_dir: &Path, path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    let path_text = path.to_string_lossy();
    if path_text == "~" {
        if let Some(base_dirs) = BaseDirs::new() {
            return base_dirs.home_dir().to_path_buf();
        }
        return path.to_path_buf();
    }

    if let Some(stripped) = path_text.strip_prefix("~/") {
        if let Some(base_dirs) = BaseDirs::new() {
            return base_dirs.home_dir().join(stripped);
        }
    }

    path.to_path_buf()
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", APP_DIR_NAME)
}

fn render_default_config_template() -> String {
    let config = AppConfig::default();
    format!(
        concat!(
            "# note-embedding configuration\n",
            "# First run created this file.\n",
            "# Edit this file before starting the service again.\n",
            "#\n",
            "# Required setup:\n",
            "# - set watch_dir to the folder you want to monitor\n",
            "# - choose file_types you want to index\n",
            "# - processed_dir should normally be a folder under watch_dir\n",
            "# - configure [embedding.ollama] for your primary provider\n",
            "# - if fallback_provider is \"gemini\", enable [embedding.gemini] and set api_key\n",
            "# - if telegram.enabled = true, set bot_token\n",
            "# - set telegram.admin_user_ids for users who can approve access requests\n",
            "# - set telegram.allowed_user_ids to restrict who can use the bot\n",
            "# - telegram.audit_log_path stores Telegram usage logs\n",
            "# - telegram.mode defaults to polling; set it to webhook to receive Telegram webhooks\n",
            "# - when using webhook mode, set webhook_url, webhook_bind_addr, and webhook_secret_token\n",
            "# - webhook mode only accepts requests from Cloudflare IP ranges and logs every sender to telegram.audit_log_path\n",
            "# - if you run cloudflared or another local reverse proxy, set webhook_trusted_proxy_cidrs to only that proxy\n",
            "# - tip: run `note-embedding --doctor` after enabling webhook mode; doctor can generate webhook_secret_token\n",
            "#   and prompt for webhook_url when those values are missing or invalid\n",
            "# - tip: to generate webhook_secret_token yourself, you can run: openssl rand -hex 24\n",
            "# - optionally set chat_id for proactive notifications or use --telegram-discover-chat\n",
            "\n",
            "watch_dir = \"{watch_dir}\"\n",
            "database_path = \"{database_path}\"\n",
            "processed_dir = \"{processed_dir}\"\n",
            "file_types = [\"txt\", \"md\"]\n",
            "\n",
            "[telegram]\n",
            "enabled = false\n",
            "bot_token = \"PASTE_TELEGRAM_BOT_TOKEN_HERE\"\n",
            "admin_user_ids = [123456789]\n",
            "allowed_user_ids = [123456789]\n",
            "audit_log_path = \"{telegram_audit_log_path}\"\n",
            "chat_id = 0\n",
            "mode = \"{telegram_mode}\"\n",
            "poll_interval_secs = {poll_interval_secs}\n",
            "webhook_url = \"\"\n",
            "webhook_bind_addr = \"{telegram_webhook_bind_addr}\"\n",
            "webhook_secret_token = \"\"\n",
            "webhook_trusted_proxy_cidrs = [\"127.0.0.1/32\", \"::1/128\"]\n",
            "match_accuracy = {match_accuracy}\n",
            "\n",
            "[embedding]\n",
            "preferred_provider = \"ollama\"\n",
            "fallback_provider = \"gemini\"\n",
            "\n",
            "[embedding.ollama]\n",
            "enabled = true\n",
            "base_url = \"{ollama_base_url}\"\n",
            "model = \"{ollama_model}\"\n",
            "\n",
            "[embedding.gemini]\n",
            "enabled = false\n",
            "api_key = \"PASTE_GEMINI_API_KEY_HERE\"\n",
            "model = \"{gemini_model}\"\n"
        ),
        watch_dir = config.watch_dir.display(),
        database_path = config.database_path.display(),
        processed_dir = config.processed_dir.display(),
        telegram_audit_log_path = config.telegram.audit_log_path.display(),
        telegram_mode = config.telegram.mode.as_str(),
        poll_interval_secs = config.telegram.poll_interval_secs,
        telegram_webhook_bind_addr = config.telegram.webhook_bind_addr,
        match_accuracy = config.telegram.match_accuracy,
        ollama_base_url = config.embedding.ollama.base_url,
        ollama_model = config.embedding.ollama.model,
        gemini_model = config.embedding.gemini.model,
    )
}

fn default_file_types() -> Vec<String> {
    vec!["txt".to_string(), "md".to_string()]
}

fn default_watch_dir() -> PathBuf {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().join("Documents"))
        .unwrap_or_else(|| PathBuf::from("./Documents"))
}

fn default_database_path() -> PathBuf {
    project_dirs()
        .map(|dirs| dirs.data_local_dir().join("embeddings.db"))
        .unwrap_or_else(|| PathBuf::from("embeddings.db"))
}

fn default_processed_dir() -> PathBuf {
    PathBuf::from(default_processed_dir_name())
}

fn default_processed_dir_name() -> &'static str {
    "processed"
}

fn default_telegram_audit_log_path() -> PathBuf {
    project_dirs()
        .map(|dirs| dirs.data_local_dir().join("telegram-activity.log"))
        .unwrap_or_else(|| PathBuf::from("telegram-activity.log"))
}

fn default_preferred_provider() -> String {
    "ollama".to_string()
}

fn default_fallback_provider() -> String {
    "gemini".to_string()
}

fn default_true() -> bool {
    true
}

fn default_ollama_base_url() -> String {
    "http://127.0.0.1:11434".to_string()
}

fn default_ollama_model() -> String {
    "nomic-embed-text".to_string()
}

fn default_gemini_model() -> String {
    "text-embedding-004".to_string()
}

fn default_poll_interval_secs() -> u64 {
    5
}

fn default_telegram_webhook_bind_addr() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_telegram_webhook_trusted_proxy_cidrs() -> Vec<String> {
    vec!["127.0.0.1/32".to_string(), "::1/128".to_string()]
}

fn default_match_accuracy() -> f32 {
    0.35
}

fn resolve_processed_dir(watch_dir: &Path, config_dir: &Path, path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else if expanded == PathBuf::from(default_processed_dir_name()) || expanded.parent().is_none()
    {
        watch_dir.join(expanded)
    } else {
        config_dir.join(expanded)
    }
}

fn normalize_file_type(value: &str) -> String {
    value.trim().trim_start_matches('.').to_ascii_lowercase()
}

fn validate_provider_name(field_name: &str, provider: &str) -> Result<()> {
    if provider == "ollama" || provider == "gemini" {
        Ok(())
    } else {
        bail!("{field_name} must be either 'ollama' or 'gemini'")
    }
}

fn is_blank_or_placeholder(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.is_empty() || trimmed.starts_with("PASTE_")
}
