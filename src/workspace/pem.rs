use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::tui::{BOLD, CYN, DIM, GRN, R, YLW};
use super::env::{parse_env_file, write_env_file, ws_env_path};

// ── Product descriptors ───────────────────────────────────────────────────────

pub struct ProductPemDef {
    pub label: &'static str,
    pub env_key: &'static str,
    /// Lowercase substrings that identify a PEM file as belonging to this product.
    pub name_hints: &'static [&'static str],
}

pub static PRODUCT_PEMS: &[ProductPemDef] = &[
    ProductPemDef {
        label: "Copilot",
        env_key: "ENTERPRISE_LICENSE",
        name_hints: &["copilot", "xtmone", "xtm-one", "xtm_one", "xtm"],
    },
    ProductPemDef {
        label: "OpenCTI",
        env_key: "APP__ENTERPRISE_EDITION_LICENSE",
        name_hints: &["opencti", "octi"],
    },
    ProductPemDef {
        label: "OpenAEV",
        env_key: "OPENAEV_APPLICATION_LICENSE",
        name_hints: &["openaev", "oaev"],
    },
];

// ── Candidate ────────────────────────────────────────────────────────────────

pub struct PemCandidate {
    pub product_label: &'static str,
    pub env_key: &'static str,
    pub path: PathBuf,
    pub display: String,
    pub selected: bool,
    pub already_set: bool,
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Scan `search_dirs` for `.pem` files and classify them by product.
/// Only includes products that are enabled and whose env_key is not already
/// set to a non-placeholder value in their workspace env file.
pub fn find_pem_candidates(
    search_dirs: &[PathBuf],
    enabled: &HashMap<&'static str, PathBuf>, // label → ws_env_path
) -> Vec<PemCandidate> {
    let mut candidates: Vec<PemCandidate> = Vec::new();

    for dir in search_dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pem") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();

            for def in PRODUCT_PEMS {
                if !enabled.contains_key(def.label) {
                    continue;
                }
                if !def.name_hints.iter().any(|hint| stem.contains(hint)) {
                    continue;
                }
                let ws_env = &enabled[def.label];
                let already_set = parse_env_file(ws_env)
                    .get(def.env_key)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false);

                let display = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();

                // Avoid duplicate paths (same file matched by multiple hints).
                if candidates.iter().any(|c| c.path == path && c.env_key == def.env_key) {
                    continue;
                }

                candidates.push(PemCandidate {
                    product_label: def.label,
                    env_key: def.env_key,
                    path: path.clone(),
                    display,
                    selected: !already_set,
                    already_set,
                });
            }
        }
    }

    candidates
}

// ── Interactive selector ──────────────────────────────────────────────────────

pub fn run_pem_selector(candidates: &mut Vec<PemCandidate>, stopping: &Arc<AtomicBool>) {
    if candidates.is_empty() {
        return;
    }
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return;
    }
    crate::tui::ensure_cooked_output();

    let render = |items: &Vec<PemCandidate>, cursor: usize| {
        print!(
            "  {BOLD}License PEM files found{R}  {DIM}↑↓ navigate   Space toggle   Enter confirm   Esc skip all{R}\r\n\r\n"
        );
        let mut last_product = "";
        for (i, c) in items.iter().enumerate() {
            if c.product_label != last_product {
                last_product = c.product_label;
                print!("  {DIM}{}{R}\r\n", c.product_label);
            }
            let check = if c.selected {
                format!("{GRN}[x]{R}")
            } else {
                format!("{DIM}[ ]{R}")
            };
            let arrow = if i == cursor {
                format!("{CYN}{BOLD}▶{R}")
            } else {
                " ".into()
            };
            let already = if c.already_set {
                format!("  {YLW}already set{R}")
            } else {
                String::new()
            };
            print!("  {} {} {}{}\r\n", arrow, check, c.display, already);
        }
        print!("\r\n");
        let _ = io::stdout().flush();
    };

    // Initial render (no raw mode yet — use println for proper newlines).
    println!("  {BOLD}License PEM files found{R}  {DIM}↑↓ navigate   Space toggle   Enter confirm   Esc skip all{R}");
    println!();
    let mut last_product = "";
    for (i, c) in candidates.iter().enumerate() {
        if c.product_label != last_product {
            last_product = c.product_label;
            println!("  {DIM}{}{R}", c.product_label);
        }
        let check = if c.selected { format!("{GRN}[x]{R}") } else { format!("{DIM}[ ]{R}") };
        let arrow = if i == 0 { format!("{CYN}{BOLD}▶{R}") } else { " ".into() };
        let already = if c.already_set { format!("  {YLW}already set{R}") } else { String::new() };
        println!("  {} {} {}{already}", arrow, check, c.display);
    }
    println!();

    let block_lines = candidates.len()
        + candidates
            .windows(2)
            .filter(|w| w[0].product_label != w[1].product_label)
            .count()
        + 4; // header + blank + trailing blank + first product label

    let mut cursor: usize = 0;
    let _ = enable_raw_mode();

    let confirmed = loop {
        if stopping.load(Ordering::Relaxed) {
            break false;
        }
        if !event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
            continue;
        }
        if let Ok(Event::Key(k)) = event::read() {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = cursor.saturating_sub(1);
                    print!("\x1b[{}A\x1b[0J", block_lines);
                    render(candidates, cursor);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if cursor + 1 < candidates.len() {
                        cursor += 1;
                    }
                    print!("\x1b[{}A\x1b[0J", block_lines);
                    render(candidates, cursor);
                }
                KeyCode::Char(' ') => {
                    candidates[cursor].selected = !candidates[cursor].selected;
                    print!("\x1b[{}A\x1b[0J", block_lines);
                    render(candidates, cursor);
                }
                KeyCode::Enter => break true,
                KeyCode::Esc => break false,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    stopping.store(true, Ordering::Relaxed);
                    break false;
                }
                _ => {}
            }
        }
    };

    let _ = disable_raw_mode();
    crate::tui::ensure_cooked_output();

    if !confirmed {
        println!("  {DIM}Skipped PEM injection.{R}");
        candidates.iter_mut().for_each(|c| c.selected = false);
    }
    println!();
}

// ── Injection ─────────────────────────────────────────────────────────────────

/// Write selected PEMs into their workspace env files.
pub fn inject_selected_pems(candidates: &[PemCandidate], ws_env_dir: &Path) {
    for c in candidates.iter().filter(|c| c.selected) {
        let Ok(pem_content) = fs::read_to_string(&c.path) else {
            eprintln!("  [pem] Could not read {:?}", c.path);
            continue;
        };
        let env_path = ws_env_path(ws_env_dir, product_ws_name(c.product_label));
        let mut map = parse_env_file(&env_path);
        map.insert(c.env_key.to_string(), pem_content.trim().to_string());
        write_env_file(&env_path, &map);
        println!(
            "  {GRN}✓{R}  {} → {BOLD}{}{R}  {DIM}({}){R}",
            c.display,
            c.env_key,
            c.product_label,
        );
    }
}

fn product_ws_name(label: &str) -> &str {
    match label {
        "Copilot" => "copilot",
        "OpenCTI" => "opencti",
        "OpenAEV" => "openaev",
        _ => label,
    }
}

/// Candidate dirs to scan for PEM files, relative to the workspace root.
pub fn pem_search_dirs(workspace_root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // <workspace_root>/docs/
    let d = workspace_root.join("docs");
    if d.is_dir() { dirs.push(d); }
    // <workspace_root>/../docs/ (e.g. if workspace_root is dev-stack/)
    if let Some(parent) = workspace_root.parent() {
        let d = parent.join("docs");
        if d.is_dir() { dirs.push(d); }
    }
    dirs
}
