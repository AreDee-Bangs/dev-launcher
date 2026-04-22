use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crossterm::event::{self, Event, KeyCode};

use crate::tui::{draw_ansi_lines, TuiGuard, BOLD, BUILD_VERSION, CYN, DIM, GRN, R, RED};

/// Default registry embedded at compile time from `repos.conf` next to Cargo.toml.
/// Users can override by placing their own copy at `~/.dev-launcher/repos.conf`.
pub const DEFAULT_REPOS_CONF: &str = include_str!("../../repos.conf");

#[derive(Clone)]
pub struct RepoEntry {
    /// Local directory name (used as clone destination and worktree base).
    pub dir: String,
    pub label: String,
    pub url: String,
    pub group: String,
}

pub fn repos_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".dev-launcher/repos.conf")
}

/// Parse an INI-style repos.conf into a list of `RepoEntry` values.
pub fn parse_repos_conf(content: &str) -> Vec<RepoEntry> {
    let mut entries: Vec<RepoEntry> = Vec::new();
    let mut dir = String::new();
    let mut label = String::new();
    let mut url = String::new();
    let mut group = String::new();

    let flush = |dir: &str, label: &str, url: &str, group: &str, out: &mut Vec<RepoEntry>| {
        if !dir.is_empty() && !url.is_empty() {
            out.push(RepoEntry {
                dir: dir.to_string(),
                label: if label.is_empty() {
                    dir.to_string()
                } else {
                    label.to_string()
                },
                url: url.to_string(),
                group: group.to_string(),
            });
        }
    };

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            flush(&dir, &label, &url, &group, &mut entries);
            dir = line[1..line.len() - 1].trim().to_string();
            label = String::new();
            url = String::new();
            group = String::new();
        } else if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "label" => label = v.trim().to_string(),
                "url" => url = v.trim().to_string(),
                "group" => group = v.trim().to_string(),
                _ => {}
            }
        }
    }
    flush(&dir, &label, &url, &group, &mut entries);
    entries
}

/// Load repo registry: user override or embedded default.
pub fn load_repos() -> Vec<RepoEntry> {
    use std::fs;
    let user_path = repos_config_path();
    if user_path.exists() {
        if let Ok(content) = fs::read_to_string(&user_path) {
            let entries = parse_repos_conf(&content);
            if !entries.is_empty() {
                return entries;
            }
        }
    }
    parse_repos_conf(DEFAULT_REPOS_CONF)
}

// ── Clone selector ────────────────────────────────────────────────────────────

pub struct CloneChoice {
    pub entry: RepoEntry,
    pub enabled: bool,
    pub present: bool,
}

pub fn build_clone_selector_lines(
    dest: &Path,
    choices: &[CloneChoice],
    cursor: usize,
) -> Vec<String> {
    let sep = "─".repeat(70);
    let mut out = Vec::new();
    out.push(format!(
        "\n  {BOLD}{CYN}{BUILD_VERSION}  —  clone repositories{R}\n"
    ));
    out.push(format!("  {DIM}Destination: {}{R}", dest.display()));
    out.push(format!("\n  {DIM}{sep}{R}"));
    out.push(format!("  {BOLD}Select repositories to clone{R}"));
    out.push(format!("  {DIM}{sep}{R}\n"));

    let mut last_group = "";
    for (i, c) in choices.iter().enumerate() {
        if c.entry.group != last_group && !c.entry.group.is_empty() {
            if i > 0 {
                out.push(String::new());
            }
            out.push(format!("  {DIM}{}{R}", c.entry.group));
            last_group = &c.entry.group;
        }

        let marker = if i == cursor {
            format!("{CYN}{BOLD}▶{R} ")
        } else {
            "  ".to_string()
        };

        let checkbox = if c.present {
            format!("{DIM}[✓ cloned]{R}  ")
        } else if c.enabled {
            format!("{GRN}[{BOLD}✓{R}{GRN}]{R}        ")
        } else {
            format!("{DIM}[ ]{R}        ")
        };

        let name = if c.present {
            format!("{DIM}{:<28}{R}", c.entry.label)
        } else if i == cursor {
            format!("{BOLD}{:<28}{R}", c.entry.label)
        } else {
            format!("{:<28}", c.entry.label)
        };

        out.push(format!(
            "  {marker}{checkbox}  {name}  {DIM}{}{R}",
            c.entry.url
        ));
    }

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!(
        "  {DIM}↑↓ / j k  navigate   Space toggle   a all   n none   Enter clone   q skip{R}"
    ));
    out.push(String::new());
    out
}

/// Interactive clone selector. Returns `true` if the user confirmed, `false` if skipped.
pub fn run_clone_selector(dest: &Path, choices: &mut [CloneChoice]) -> bool {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return false;
    }
    let mut raw = TuiGuard::enter();
    let mut cursor = 0usize;
    if let Some(first) = choices.iter().position(|c| !c.present) {
        cursor = first;
    }
    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(tui, &build_clone_selector_lines(dest, choices, cursor));
        }
        if event::poll(std::time::Duration::from_millis(20)).unwrap_or(false) {
            let Ok(Event::Key(ke)) = event::read() else {
                continue;
            };
            match ke.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if cursor + 1 < choices.len() => {
                    cursor += 1;
                }
                KeyCode::Char(' ') if !choices[cursor].present => {
                    choices[cursor].enabled = !choices[cursor].enabled;
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    for c in choices.iter_mut() {
                        if !c.present {
                            c.enabled = true;
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    for c in choices.iter_mut() {
                        if !c.present {
                            c.enabled = false;
                        }
                    }
                }
                KeyCode::Enter => {
                    drop(raw.take());
                    return true;
                }
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                    drop(raw.take());
                    return false;
                }
                _ => {}
            }
        }
    }
}

/// Clone selected repos. Git output streams directly to the terminal.
pub fn clone_repos(dest: &Path, choices: &[CloneChoice]) {
    let sep = "─".repeat(60);
    println!("\n  {BOLD}Cloning into {}{R}", dest.display());
    println!("  {DIM}{sep}{R}\n");
    for c in choices.iter().filter(|c| c.enabled && !c.present) {
        println!("  {CYN}▶{R}  {} — {DIM}{}{R}", c.entry.label, c.entry.url);
        let target = dest.join(&c.entry.dir).to_string_lossy().into_owned();
        let status = Command::new("git")
            .args(["clone", &c.entry.url, &target])
            .current_dir(dest)
            .stdin(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => println!("  {GRN}✓{R}  {} cloned\n", c.entry.label),
            Ok(s) => println!(
                "  {RED}✗{R}  {} failed (exit {})\n",
                c.entry.label,
                s.code().unwrap_or(-1)
            ),
            Err(e) => println!("  {RED}✗{R}  {} error: {e}\n", c.entry.label),
        }
    }
    println!("  {DIM}{sep}{R}\n");
}
