use crate::config::{AppConfig, resolve_config_path};
use crate::telegram::delete_registered_webhook;
use crate::ui;
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use directories::{BaseDirs, ProjectDirs};
use std::env;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SERVICE_NAME: &str = "note-embedding";
const SERVICE_UNIT_NAME: &str = "note-embedding.service";

#[derive(Clone, Debug, ValueEnum)]
pub enum ServiceAction {
    Start,
    Status,
    Log,
    Restart,
    Stop,
    Uninstall,
}

pub fn handle_service_action(
    action: ServiceAction,
    config_override: Option<PathBuf>,
) -> Result<()> {
    match action {
        ServiceAction::Start => start_service(config_override),
        ServiceAction::Status => status_service(),
        ServiceAction::Log => log_service(),
        ServiceAction::Restart => restart_service(config_override),
        ServiceAction::Stop => stop_service(),
        ServiceAction::Uninstall => uninstall_service(config_override),
    }
}

fn start_service(config_override: Option<PathBuf>) -> Result<()> {
    ensure_systemd_available()?;
    let paths = ServicePaths::resolve(config_override)?;
    let loaded_config = AppConfig::load_or_create(Some(paths.config_path.clone()))?;

    if loaded_config.created {
        println!(
            "{} Created config template: {}",
            ui::info_label(),
            ui::path(loaded_config.path.display())
        );
        println!(
            "Edit the config values, then run {} again.",
            ui::command("note-embedding --service start")
        );
        return Ok(());
    }

    install_service_files(&paths)?;
    run_command("systemctl", &["--user", "daemon-reload"])?;
    run_command(
        "systemctl",
        &["--user", "enable", "--now", SERVICE_UNIT_NAME],
    )?;
    println!(
        "{} Started user service: {}",
        ui::ok_label(),
        ui::value(SERVICE_UNIT_NAME)
    );
    Ok(())
}

fn restart_service(config_override: Option<PathBuf>) -> Result<()> {
    ensure_systemd_available()?;
    let paths = ServicePaths::resolve(config_override)?;
    let loaded_config = AppConfig::load_or_create(Some(paths.config_path.clone()))?;

    if loaded_config.created {
        println!(
            "{} Created config template: {}",
            ui::info_label(),
            ui::path(loaded_config.path.display())
        );
        println!(
            "Edit the config values, then run {} again.",
            ui::command("note-embedding --service restart")
        );
        return Ok(());
    }

    install_service_files(&paths)?;
    run_command("systemctl", &["--user", "daemon-reload"])?;
    run_command("systemctl", &["--user", "enable", SERVICE_UNIT_NAME])?;
    run_command("systemctl", &["--user", "restart", SERVICE_UNIT_NAME])?;
    println!(
        "{} Restarted user service: {}",
        ui::ok_label(),
        ui::value(SERVICE_UNIT_NAME)
    );
    Ok(())
}

fn status_service() -> Result<()> {
    ensure_systemd_available()?;
    run_command(
        "systemctl",
        &["--user", "status", "--no-pager", SERVICE_UNIT_NAME],
    )
}

fn log_service() -> Result<()> {
    ensure_systemd_available()?;
    run_command(
        "journalctl",
        &["--user", "-u", SERVICE_UNIT_NAME, "-n", "100", "--no-pager"],
    )
}

fn stop_service() -> Result<()> {
    ensure_systemd_available()?;
    run_command("systemctl", &["--user", "stop", SERVICE_UNIT_NAME])?;
    println!(
        "{} Stopped user service: {}",
        ui::ok_label(),
        ui::value(SERVICE_UNIT_NAME)
    );
    Ok(())
}

fn uninstall_service(config_override: Option<PathBuf>) -> Result<()> {
    let paths = ServicePaths::resolve(config_override)?;
    cleanup_telegram_webhook(&paths.config_path);

    if systemd_available() {
        let _ = run_command(
            "systemctl",
            &["--user", "disable", "--now", SERVICE_UNIT_NAME],
        );
        let _ = run_command("systemctl", &["--user", "daemon-reload"]);
    }

    remove_if_exists(&paths.service_unit_path)?;
    remove_symlink_if_exists(&paths.user_bin_path)?;

    remove_if_exists(&paths.config_path)?;
    remove_dir_if_empty(&paths.config_dir)?;

    if paths.data_dir.exists() {
        fs::remove_dir_all(&paths.data_dir).with_context(|| {
            format!(
                "failed to remove data directory {}",
                paths.data_dir.display()
            )
        })?;
    }

    remove_dir_if_empty(&paths.user_systemd_dir)?;
    remove_dir_if_empty(&paths.user_bin_dir)?;

    println!(
        "{} Uninstalled user service assets for {}",
        ui::ok_label(),
        ui::value(SERVICE_NAME)
    );
    Ok(())
}

fn cleanup_telegram_webhook(config_path: &Path) {
    if !config_path.exists() {
        return;
    }

    match AppConfig::load_or_create(Some(config_path.to_path_buf())) {
        Ok(loaded) if !loaded.created => {
            if loaded.data.telegram.bot_token.trim().is_empty() {
                return;
            }
            if let Err(err) = delete_registered_webhook(&loaded.data.telegram) {
                eprintln!(
                    "Failed to delete Telegram webhook during uninstall using {}: {err:#}",
                    config_path.display()
                );
            }
        }
        Ok(_) => {}
        Err(err) => {
            eprintln!(
                "Failed to load config {} for Telegram webhook cleanup during uninstall: {err:#}",
                config_path.display()
            );
        }
    }
}

fn install_service_files(paths: &ServicePaths) -> Result<()> {
    fs::create_dir_all(&paths.user_systemd_dir).with_context(|| {
        format!(
            "failed to create systemd user directory {}",
            paths.user_systemd_dir.display()
        )
    })?;
    fs::create_dir_all(&paths.user_bin_dir).with_context(|| {
        format!(
            "failed to create user bin directory {}",
            paths.user_bin_dir.display()
        )
    })?;

    refresh_executable_symlink(&paths.current_exe, &paths.user_bin_path)?;
    fs::write(&paths.service_unit_path, render_unit_file(paths)).with_context(|| {
        format!(
            "failed to write systemd unit file {}",
            paths.service_unit_path.display()
        )
    })?;

    Ok(())
}

fn refresh_executable_symlink(current_exe: &Path, symlink_path: &Path) -> Result<()> {
    if symlink_path.exists() && !symlink_path.is_symlink() {
        bail!(
            "{} already exists and is not a symlink; remove it manually or choose a different install path",
            symlink_path.display()
        );
    }

    if symlink_path.is_symlink() {
        fs::remove_file(symlink_path).with_context(|| {
            format!(
                "failed to replace executable symlink {}",
                symlink_path.display()
            )
        })?;
    }

    symlink(current_exe, symlink_path).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            symlink_path.display(),
            current_exe.display()
        )
    })?;

    Ok(())
}

fn render_unit_file(paths: &ServicePaths) -> String {
    let exec_start = systemd_quote(&paths.user_bin_path.to_string_lossy());
    let config_env = systemd_quote(&format!(
        "NOTE_EMBEDDING_CONFIG={}",
        paths.config_path.display()
    ));

    format!(
        "[Unit]\nDescription=note-embedding background service\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={exec_start}\nEnvironment={config_env}\nRestart=on-failure\nRestartSec=5\nStandardOutput=journal\nStandardError=journal\n\n[Install]\nWantedBy=default.target\n"
    )
}

fn systemd_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn ensure_systemd_available() -> Result<()> {
    if systemd_available() {
        Ok(())
    } else {
        bail!(
            "systemd user services are not available; make sure `systemctl --user` works in this session"
        )
    }
}

fn systemd_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run_command(command: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(command)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to run `{command}`"))?;

    if status.success() {
        Ok(())
    } else {
        bail!("`{command} {}` exited with status {status}", args.join(" "))
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove file {}", path.display()))?;
    }
    Ok(())
}

fn remove_symlink_if_exists(path: &Path) -> Result<()> {
    if path.is_symlink() {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove symlink {}", path.display()))?;
    }
    Ok(())
}

fn remove_dir_if_empty(path: &Path) -> Result<()> {
    if path.exists() && fs::read_dir(path)?.next().is_none() {
        fs::remove_dir(path)
            .with_context(|| format!("failed to remove directory {}", path.display()))?;
    }
    Ok(())
}

struct ServicePaths {
    config_path: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    current_exe: PathBuf,
    user_bin_dir: PathBuf,
    user_bin_path: PathBuf,
    user_systemd_dir: PathBuf,
    service_unit_path: PathBuf,
}

impl ServicePaths {
    fn resolve(config_override: Option<PathBuf>) -> Result<Self> {
        let base_dirs = BaseDirs::new().context("failed to resolve user home directory")?;
        let project_dirs = ProjectDirs::from("", "", SERVICE_NAME)
            .context("failed to resolve project directories")?;

        let config_path = match config_override {
            Some(path) => resolve_config_path(Some(path))?,
            None => resolve_config_path(None)?,
        };
        let current_exe = env::current_exe().context("failed to locate current executable")?;
        let user_bin_dir = base_dirs.home_dir().join(".local/bin");
        let user_systemd_dir = base_dirs.home_dir().join(".config/systemd/user");

        Ok(Self {
            config_dir: config_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| base_dirs.home_dir().join(".config")),
            data_dir: project_dirs.data_local_dir().to_path_buf(),
            current_exe,
            user_bin_path: user_bin_dir.join(SERVICE_NAME),
            user_bin_dir,
            user_systemd_dir: user_systemd_dir.clone(),
            service_unit_path: user_systemd_dir.join(SERVICE_UNIT_NAME),
            config_path,
        })
    }
}
