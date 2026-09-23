//! Terminal output: colours, quiet mode, tables and secret scrubbing.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Clone)]
pub struct Ui {
    color: bool,
    quiet: bool,
    tty: bool,
    secrets: Arc<Mutex<Vec<String>>>,
}

impl Ui {
    /// Text is always styled unless colours are off; anstream strips the codes per stream
    /// (not a terminal, NO_COLOR, TERM=dumb), so piping stdout never leaks escape sequences.
    pub fn new(mode: ColorMode, quiet: bool) -> Ui {
        let choice = match mode {
            ColorMode::Auto => anstream::ColorChoice::Auto,
            ColorMode::Always => anstream::ColorChoice::Always,
            ColorMode::Never => anstream::ColorChoice::Never,
        };
        choice.write_global();
        Ui { color: mode != ColorMode::Never, quiet, tty: std::io::stderr().is_terminal(), secrets: Arc::new(Mutex::new(Vec::new())) }
    }

    /// Whether styled text written directly to stderr (bypassing anstream, e.g. progress bars) keeps its colours.
    pub fn stderr_color(&self) -> bool {
        self.color && anstream::AutoStream::choice(&std::io::stderr()) != anstream::ColorChoice::Never
    }

    pub fn quiet(&self) -> bool {
        self.quiet
    }
    pub fn is_tty(&self) -> bool {
        self.tty
    }

    /// Registers a value that must never be printed.
    pub fn protect(&self, secret: &str) {
        if secret.len() >= 4 {
            self.secrets.lock().unwrap().push(secret.to_string());
        }
    }

    pub fn scrub(&self, text: &str) -> String {
        let secrets = self.secrets.lock().unwrap();
        let mut out = text.to_string();
        for secret in secrets.iter() {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), "***");
            }
        }
        out
    }

    fn paint(&self, style: anstyle::Style, text: &str) -> String {
        if self.color { format!("{style}{text}{style:#}") } else { text.to_string() }
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint(anstyle::Style::new().bold(), text)
    }
    pub fn dim(&self, text: &str) -> String {
        self.paint(anstyle::Style::new().dimmed(), text)
    }
    pub fn green(&self, text: &str) -> String {
        self.paint(anstyle::Style::new().fg_color(Some(anstyle::AnsiColor::Green.into())), text)
    }
    pub fn red(&self, text: &str) -> String {
        self.paint(anstyle::Style::new().fg_color(Some(anstyle::AnsiColor::Red.into())).bold(), text)
    }
    pub fn yellow(&self, text: &str) -> String {
        self.paint(anstyle::Style::new().fg_color(Some(anstyle::AnsiColor::Yellow.into())), text)
    }
    pub fn cyan(&self, text: &str) -> String {
        self.paint(anstyle::Style::new().fg_color(Some(anstyle::AnsiColor::Cyan.into())), text)
    }

    /// Progress and informational messages (stderr, suppressed by --quiet).
    pub fn info(&self, text: &str) {
        if !self.quiet {
            let _ = writeln!(anstream::stderr(), "{}", self.scrub(text));
        }
    }

    /// Result lines that scripts may rely on (stdout, printed even with --quiet unless JSON output is active).
    pub fn out(&self, text: &str) {
        let _ = writeln!(anstream::stdout(), "{}", self.scrub(text));
    }

    pub fn success(&self, text: &str) {
        if !self.quiet {
            let _ = writeln!(anstream::stderr(), "{} {}", self.green("✓"), self.scrub(text));
        }
    }

    pub fn warn(&self, text: &str) {
        let _ = writeln!(anstream::stderr(), "{} {}", self.yellow("warning:"), self.scrub(text));
    }

    pub fn error(&self, message: &str, hint: Option<&str>) {
        let _ = writeln!(anstream::stderr(), "{} {}", self.red("ppr-tool:"), self.scrub(message));
        if let Some(hint) = hint {
            let _ = writeln!(anstream::stderr(), "  {} {}", self.dim("hint:"), self.scrub(hint));
        }
    }

    /// Renders rows as an aligned table. The first row is the header.
    pub fn table(&self, rows: &[Vec<String>]) -> String {
        if rows.is_empty() {
            return String::new();
        }
        let columns = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut widths = vec![0usize; columns];
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let mut out = String::new();
        for (index, row) in rows.iter().enumerate() {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                if i > 0 {
                    line.push_str("  ");
                }
                let pad = widths[i] - cell.chars().count();
                if i + 1 == row.len() {
                    line.push_str(cell);
                } else {
                    line.push_str(cell);
                    line.push_str(&" ".repeat(pad));
                }
            }
            let line = line.trim_end().to_string();
            if index == 0 {
                out.push_str(&self.bold(&line));
            } else {
                out.push_str(&line);
            }
            out.push('\n');
        }
        out
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}
