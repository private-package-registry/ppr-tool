//! GitHub Actions integration: masking, workflow commands, outputs and the step summary.
//! Workflow commands go to stderr (the runner reads both streams) so stdout stays clean for --json.

use std::fs::OpenOptions;
use std::io::Write;

pub fn active() -> bool {
    std::env::var("GITHUB_ACTIONS").map(|v| v == "true").unwrap_or(false)
}

fn escape_data(value: &str) -> String {
    value.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

fn escape_property(value: &str) -> String {
    escape_data(value).replace(':', "%3A").replace(',', "%2C")
}

/// Registers a secret with the runner so it is redacted from logs.
pub fn add_mask(value: &str) {
    if active() && !value.is_empty() {
        eprintln!("::add-mask::{}", escape_data(value));
    }
}

pub fn group(title: &str) {
    if active() {
        eprintln!("::group::{}", escape_data(title));
    }
}

pub fn end_group() {
    if active() {
        eprintln!("::endgroup::");
    }
}

pub fn error_annotation(title: &str, message: &str) {
    if active() {
        eprintln!("::error title={}::{}", escape_property(title), escape_data(message));
    }
}

pub fn notice(message: &str) {
    if active() {
        eprintln!("::notice title=ppr-tool::{}", escape_data(message));
    }
}

fn append(variable: &str, content: &str) -> std::io::Result<()> {
    if let Some(path) = std::env::var_os(variable).filter(|p| !p.is_empty()) {
        let mut file = OpenOptions::new().append(true).create(true).open(path)?;
        file.write_all(content.as_bytes())?;
    }
    Ok(())
}

pub fn set_output(name: &str, value: &str) -> std::io::Result<()> {
    append("GITHUB_OUTPUT", &format!("{name}={value}\n"))
}

pub fn export_env(name: &str, value: &str) -> std::io::Result<()> {
    append("GITHUB_ENV", &format!("{name}={value}\n"))
}

pub fn step_summary(markdown: &str) -> std::io::Result<()> {
    append("GITHUB_STEP_SUMMARY", markdown)
}
