use std::fmt::Display;
use std::io::{IsTerminal, stderr, stdout};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const BLUE: &str = "\x1b[34m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const MAGENTA: &str = "\x1b[35m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";

pub fn ok_label() -> String {
    paint_stdout("[ok]", &[BOLD, GREEN])
}

pub fn warn_label() -> String {
    paint_stdout("[warn]", &[BOLD, YELLOW])
}

pub fn fail_label() -> String {
    paint_stdout("[fail]", &[BOLD, RED])
}

pub fn info_label() -> String {
    paint_stdout("[info]", &[BOLD, CYAN])
}

pub fn path(value: impl Display) -> String {
    paint_stdout(&value.to_string(), &[BOLD, BLUE])
}

pub fn command(value: impl Display) -> String {
    paint_stdout(&value.to_string(), &[BOLD, CYAN])
}

pub fn value(value: impl Display) -> String {
    paint_stdout(&value.to_string(), &[BOLD, MAGENTA])
}

pub fn heading(value: impl Display) -> String {
    paint_stdout(&value.to_string(), &[BOLD])
}

pub fn detail(value: impl Display) -> String {
    paint_stdout(&value.to_string(), &[DIM])
}

fn paint_stdout(text: &str, codes: &[&str]) -> String {
    paint(text, codes, stdout().is_terminal())
}

#[allow(dead_code)]
fn paint_stderr(text: &str, codes: &[&str]) -> String {
    paint(text, codes, stderr().is_terminal())
}

fn paint(text: &str, codes: &[&str], enabled: bool) -> String {
    if !enabled {
        return text.to_string();
    }

    format!("{}{}{}", codes.join(""), text, RESET)
}
