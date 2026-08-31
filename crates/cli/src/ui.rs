//! Terminal output: colors when the terminal wants them, questions, progress.

use std::io::{self, IsTerminal, Write};

use anyhow::{bail, Result};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
/// The single accent color, used for nothing but emphasis.
const ACCENT: &str = "\x1b[36m";

pub struct Ui {
    color: bool,
    interactive: bool,
    assume_yes: bool,
}

impl Ui {
    pub fn new(no_color_flag: bool, assume_yes: bool) -> Self {
        let no_color = no_color_flag
            || std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
            || std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);

        Self {
            color: !no_color && io::stdout().is_terminal(),
            interactive: io::stdin().is_terminal() && io::stderr().is_terminal(),
            assume_yes,
        }
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    pub fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint(BOLD, text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint(DIM, text)
    }

    pub fn accent(&self, text: &str) -> String {
        self.paint(ACCENT, text)
    }

    /// Writes a line to stdout. A closed pipe is not an error worth
    /// reporting, `appimg list | head` must stay quiet.
    pub fn info(&self, message: &str) {
        let _ = writeln!(io::stdout(), "{message}");
    }

    /// Writes bytes to stdout unchanged, for machine-readable output.
    pub fn raw(&self, bytes: &[u8]) {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(bytes);
        let _ = stdout.flush();
    }

    pub fn warn(&self, message: &str) {
        let _ = writeln!(io::stderr(), "{} {message}", self.bold("warning:"));
    }

    /// Asks a yes/no question. `--yes` answers it, a pipe makes it an error
    /// rather than a silent guess.
    pub fn confirm(&self, question: &str, default_yes: bool) -> Result<bool> {
        if self.assume_yes {
            return Ok(true);
        }
        if !self.interactive {
            bail!("{question} — nothing to ask on a pipe, pass --yes to answer it");
        }

        let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
        loop {
            eprint!("{question} {hint} ");
            io::stderr().flush().ok();

            let mut answer = String::new();
            if io::stdin().read_line(&mut answer)? == 0 {
                return Ok(false);
            }
            match answer.trim().to_lowercase().as_str() {
                "" => return Ok(default_yes),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => eprintln!("Please answer y or n."),
            }
        }
    }

    /// A progress reporter for downloads. Silent unless a terminal is
    /// watching, so pipes stay clean.
    pub fn progress(&self) -> Progress {
        Progress { enabled: self.color && self.interactive, last_line: String::new() }
    }
}

pub struct Progress {
    enabled: bool,
    last_line: String,
}

impl Progress {
    pub fn update(&mut self, done: u64, total: Option<u64>) {
        if !self.enabled {
            return;
        }
        let line = match total {
            Some(total) if total > 0 => {
                let percent = (done as f64 / total as f64 * 100.0).min(100.0);
                format!("  {} / {} ({percent:.0}%)", human_size(done), human_size(total))
            }
            _ => format!("  {}", human_size(done)),
        };
        if line == self.last_line {
            return;
        }
        eprint!("\r\x1b[K{line}");
        io::stderr().flush().ok();
        self.last_line = line;
    }

    pub fn finish(&mut self) {
        if self.enabled && !self.last_line.is_empty() {
            eprint!("\r\x1b[K");
            io::stderr().flush().ok();
            self.last_line.clear();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.finish();
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Renders a table with a header, padded to the widest cell per column. The
/// last column is never padded, so nothing trails into the void.
pub fn table(ui: &Ui, header: &[&str], rows: &[Vec<String>]) -> String {
    let columns = header.len();
    let mut widths = header.iter().map(|h| h.chars().count()).collect::<Vec<_>>();
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(columns) {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    out.push_str(&ui.bold(&pad_row(header.iter().map(|h| h.to_string()).collect(), &widths)));
    for row in rows {
        out.push('\n');
        out.push_str(&pad_row(row.clone(), &widths));
    }
    out
}

fn pad_row(cells: Vec<String>, widths: &[usize]) -> String {
    let last = cells.len().saturating_sub(1);
    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            if index == last {
                cell.clone()
            } else {
                let pad = widths[index].saturating_sub(cell.chars().count());
                format!("{cell}{}", " ".repeat(pad))
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}
