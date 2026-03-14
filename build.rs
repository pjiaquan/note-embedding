use std::env;
use std::process::{Command, Stdio};

const SERVICE_UNIT_NAME: &str = "note-embedding.service";
const SKIP_ENV: &str = "NOTE_EMBEDDING_SKIP_SERVICE_STOP";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={SKIP_ENV}");

    if env::var_os(SKIP_ENV).is_some() {
        println!("cargo:warning=skipping service stop because {SKIP_ENV} is set");
        return;
    }

    if !systemctl_user_available() {
        return;
    }

    if !service_is_active() {
        return;
    }

    match Command::new("systemctl")
        .args(["--user", "stop", SERVICE_UNIT_NAME])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {
            println!("cargo:warning=stopped {SERVICE_UNIT_NAME} before build");
        }
        Ok(_) => {
            println!(
                "cargo:warning=failed to stop {SERVICE_UNIT_NAME} before build; continuing anyway"
            );
        }
        Err(err) => {
            println!("cargo:warning=failed to invoke systemctl for {SERVICE_UNIT_NAME}: {err}");
        }
    }
}

fn systemctl_user_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn service_is_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "--quiet", "is-active", SERVICE_UNIT_NAME])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
