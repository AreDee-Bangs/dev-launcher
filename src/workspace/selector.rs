use std::io::{self, Write};
use std::path::Path;

use crossterm::event::{self, Event, KeyCode};

use crate::config::read_line_or_interrupt;
use crate::tui::{
    drain_input_events, draw_ansi_lines, TuiGuard, BOLD, BUILD_VERSION, CYN, DIM, GRN, R, RED, YLW,
};
use crate::workspace::env::parse_env_file;
use crate::workspace::git::{
    branch_to_slug, parse_commit_ref, worktree_delete_blockers, worktree_dirty_reasons,
};
use crate::workspace::{WorkspaceConfig, PRODUCTS};

pub enum WorkspaceAction {
    Open(WorkspaceConfig),
    Delete(WorkspaceConfig),
    CreateNew,
    Quit,
}

fn build_workspace_selector_lines(workspaces: &[WorkspaceConfig], cursor: usize) -> Vec<String> {
    let sep = "─".repeat(72);
    let mut out = Vec::new();
    out.push(format!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}\n"));
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!(
        "  {BOLD}Workspaces{R}  {DIM}— select one to start or create a new one{R}"
    ));
    out.push(format!("  {DIM}{sep}{R}\n"));

    let total = workspaces.len() + 1;
    for (i, ws) in workspaces.iter().enumerate() {
        let marker = if i == cursor {
            format!("{CYN}{BOLD}▶{R} ")
        } else {
            "  ".to_string()
        };
        let hash = format!("{DIM}[{}]{R}", ws.hash);
        let summary = ws.summary();
        let date = format!("{DIM}{}{R}", ws.created);
        let summary_display = if summary.len() > 52 {
            format!("{}…", &summary[..51])
        } else {
            summary.clone()
        };
        out.push(format!("  {marker}{hash}  {:<54}{date}", summary_display));
    }

    let new_idx = workspaces.len();
    let marker = if new_idx == cursor {
        format!("{CYN}{BOLD}▶{R} ")
    } else {
        "  ".to_string()
    };
    out.push(format!("  {marker}{GRN}[+] Create new workspace{R}"));

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    if cursor < total - 1 {
        out.push(format!(
            "  {DIM}↑↓ navigate   Enter open   d delete   q quit{R}"
        ));
    } else {
        out.push(format!("  {DIM}↑↓ navigate   Enter create   q quit{R}"));
    }
    out.push(String::new());
    out
}

pub fn run_workspace_selector(workspaces: &[WorkspaceConfig]) -> WorkspaceAction {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return WorkspaceAction::CreateNew;
    }
    let mut raw = TuiGuard::enter();
    let total = workspaces.len() + 1;
    let mut cursor = 0usize;
    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(tui, &build_workspace_selector_lines(workspaces, cursor));
        }
        if event::poll(std::time::Duration::from_millis(20)).unwrap_or(false) {
            let Ok(Event::Key(ke)) = event::read() else {
                continue;
            };
            if ke.kind != crossterm::event::KeyEventKind::Press {
                continue;
            }
            match ke.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if cursor + 1 < total => {
                    cursor += 1;
                }
                KeyCode::Enter => {
                    drain_input_events();
                    drop(raw.take());
                    if cursor == workspaces.len() {
                        return WorkspaceAction::CreateNew;
                    } else {
                        return WorkspaceAction::Open(workspaces[cursor].clone());
                    }
                }
                KeyCode::Char('d') | KeyCode::Char('D') if cursor < workspaces.len() => {
                    drain_input_events();
                    drop(raw.take());
                    return WorkspaceAction::Delete(workspaces[cursor].clone());
                }
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                    drain_input_events();
                    drop(raw.take());
                    print!("\x1b[H\x1b[2J");
                    let _ = io::stdout().flush();
                    return WorkspaceAction::Quit;
                }
                _ => {}
            }
        }
    }
}

/// Full workspace removal flow.
pub fn run_workspace_delete(config: &WorkspaceConfig, workspace_root: &Path, ws_dir: &Path) {
    use crate::services::docker::{docker_kill_by_name_fragment, write_compose_override};
    use std::fs;

    let sep = "─".repeat(56);
    println!(
        "\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}remove workspace {}{R}",
        config.hash
    );
    println!("\n  {DIM}{sep}{R}");

    struct DirtyEntry {
        repo: String,
        worktree: std::path::PathBuf,
        reasons: Vec<String>,
    }
    struct BlockedEntry {
        repo: String,
        worktree: std::path::PathBuf,
        reasons: Vec<String>,
    }
    let mut blocked: Vec<BlockedEntry> = Vec::new();
    let mut dirty: Vec<DirtyEntry> = Vec::new();
    let mut worktrees_to_remove: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut main_checkouts: Vec<(String, std::path::PathBuf)> = Vec::new();

    for entry in &config.entries {
        if !entry.enabled {
            continue;
        }
        let main = workspace_root.join(&entry.repo);

        if entry.branch.is_empty() {
            if main.is_dir() {
                main_checkouts.push((entry.repo.clone(), main));
            }
            continue;
        }

        let slug = branch_to_slug(&entry.branch);
        let wt = workspace_root.join(format!("{}-{}", entry.repo, slug));

        if !wt.is_dir() || wt == main {
            if main.is_dir() {
                main_checkouts.push((entry.repo.clone(), main));
            }
        } else {
            let blockers = worktree_delete_blockers(&main, &wt, &entry.branch);
            if !blockers.is_empty() {
                blocked.push(BlockedEntry {
                    repo: entry.repo.clone(),
                    worktree: wt.clone(),
                    reasons: blockers,
                });
            }
            let reasons = worktree_dirty_reasons(&wt);
            if !reasons.is_empty() {
                dirty.push(DirtyEntry {
                    repo: entry.repo.clone(),
                    worktree: wt.clone(),
                    reasons,
                });
            }
            worktrees_to_remove.push((entry.repo.clone(), wt));
        }
    }

    if !worktrees_to_remove.is_empty() {
        println!("  Worktrees to be removed:");
        for (_, wt) in &worktrees_to_remove {
            println!("    {RED}–{R}  {}", wt.display());
        }
    }
    if !main_checkouts.is_empty() {
        println!("  Main checkouts preserved (shared across all workspaces):");
        for (repo, dir) in &main_checkouts {
            println!("    {DIM}·{R}  {}  {DIM}({}){R}", dir.display(), repo);
        }
    }
    println!();

    if !blocked.is_empty() {
        println!("  {RED}{BOLD}Warning: the following worktrees have unresolved Git blockers:{R}\n");
        for b in &blocked {
            println!(
                "  {RED}▶{R}  {BOLD}{}{R}  ({})",
                b.repo,
                b.reasons.join(", ")
            );
            println!("     {DIM}{}{R}", b.worktree.display());
        }
        println!();
        println!("  {DIM}Forcing removal may permanently lose uncommitted work or unpushed branches.{R}");
        println!();
        print!("  Type {BOLD}YES{R} to force removal despite these blockers: ");
        let _ = io::stdout().flush();
        match read_line_or_interrupt() {
            Some(l) if l.trim() == "YES" => {}
            _ => {
                println!("  Cancelled.");
                return;
            }
        }
        print!("  This cannot be undone.  Type {BOLD}YES{R} again to proceed: ");
        let _ = io::stdout().flush();
        match read_line_or_interrupt() {
            Some(l) if l.trim() == "YES" => {}
            _ => {
                println!("  Cancelled.");
                return;
            }
        }
    } else if !dirty.is_empty() {
        println!("  {YLW}{BOLD}Warning: the following worktrees have ongoing work:{R}\n");
        for d in &dirty {
            println!(
                "  {YLW}▶{R}  {BOLD}{}{R}  ({})",
                d.repo,
                d.reasons.join(", ")
            );
            println!("     {DIM}{}{R}", d.worktree.display());
        }
        println!();
        print!("  Type {BOLD}YES{R} to confirm removal despite uncommitted work: ");
        let _ = io::stdout().flush();
        match read_line_or_interrupt() {
            Some(l) if l.trim() == "YES" => {}
            _ => {
                println!("  Cancelled.");
                return;
            }
        }
        print!("  This cannot be undone.  Type {BOLD}YES{R} again to proceed: ");
        let _ = io::stdout().flush();
        match read_line_or_interrupt() {
            Some(l) if l.trim() == "YES" => {}
            _ => {
                println!("  Cancelled.");
                return;
            }
        }
    } else {
        print!("  Remove workspace {BOLD}{}{R}? [y/N] ", config.hash);
        let _ = io::stdout().flush();
        match read_line_or_interrupt() {
            Some(l) if l.trim().eq_ignore_ascii_case("y") => {}
            _ => {
                println!("  Cancelled.");
                return;
            }
        }
    }

    println!();

    let ws_hash = config.hash.as_str();
    for entry in &config.entries {
        if !entry.enabled {
            continue;
        }
        if entry.repo == "connectors" {
            continue;
        }

        let slug = branch_to_slug(&entry.branch);
        let wt = workspace_root.join(format!("{}-{}", entry.repo, slug));
        let dir_buf = if !entry.branch.is_empty() && wt.is_dir() {
            wt
        } else {
            workspace_root.join(&entry.repo)
        };
        let dir = dir_buf.as_path();
        if !dir.is_dir() {
            continue;
        }

        let Some((ws_project, base_project, compose_file)) =
            crate::services::docker::resolve_product_docker_for_down(&entry.repo, dir, ws_hash)
        else {
            continue;
        };

        if !compose_file.exists() {
            continue;
        }
        let file_str = compose_file.to_str().unwrap_or("");

        print!("  Stopping {} Docker containers… ", entry.repo);
        let _ = io::stdout().flush();

        let ws_override = write_compose_override(&compose_file, ws_hash);
        let mut argv_ws: Vec<&str> = vec!["compose", "-p", &ws_project, "-f", file_str];
        let ov_str: String;
        if let Some(ref ov) = ws_override {
            ov_str = ov.to_string_lossy().into_owned();
            argv_ws.extend_from_slice(&["-f", &ov_str]);
        }
        argv_ws.extend_from_slice(&["down", "-v"]);
        let _ = crate::services::docker::run_blocking("docker", &argv_ws, dir);

        let _ = crate::services::docker::run_blocking(
            "docker",
            &["compose", "-p", &base_project, "-f", file_str, "down", "-v"],
            dir,
        );

        let container_prefix = base_project.split('-').next().unwrap_or(&base_project);
        docker_kill_by_name_fragment(container_prefix);

        println!("{GRN}done{R}");
    }

    for (repo, wt) in &worktrees_to_remove {
        let main_repo = workspace_root.join(repo);
        let wt_str = wt.to_str().unwrap_or("");

        print!("  Removing worktree {}… ", wt.display());
        let _ = io::stdout().flush();

        let git_ok = std::process::Command::new("git")
            .args(["worktree", "remove", "--force", wt_str])
            .current_dir(&main_repo)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !git_ok && wt.is_dir() {
            let _ = fs::remove_dir_all(wt);
        }

        let _ = std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&main_repo)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let slug = wt
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(&format!("{}-", repo)))
            .unwrap_or("")
            .to_string();
        if !slug.is_empty() {
            let _ = std::process::Command::new("git")
                .args(["branch", "-D", &slug])
                .current_dir(&main_repo)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }

        if !wt.is_dir() {
            println!("{GRN}done{R}");
        } else {
            println!("{YLW}could not remove — delete manually:{R}");
            println!("    rm -rf {}", wt.display());
        }
    }

    crate::workspace::tombstone_workspace(ws_dir, &config.hash);

    println!("\n  {GRN}✓{R}  Workspace {BOLD}{}{R} deleted.", config.hash);
    if !worktrees_to_remove.is_empty() {
        println!("  {DIM}Worktree directories removed.{R}");
    }
    if !main_checkouts.is_empty() {
        println!("  {DIM}Main repo directories kept — they are shared across all workspaces.{R}");
        println!(
            "  {DIM}To fully reset, delete {} manually.{R}",
            workspace_root.display()
        );
    }
    println!();
}

// ── Product selector ─────────────────────────────────────────────────────────

pub struct ProductChoice {
    pub label: &'static str,
    pub desc: &'static str,
    pub repo: &'static str,
    pub enabled: bool,
    pub available: bool,
    pub branch: String,
}

/// What the user chose to do from the product selector.
pub enum LaunchMode {
    Normal,
    Clean,
    Quit,
}

pub fn build_product_selector_lines(
    slug: &str,
    choices: &[ProductChoice],
    cursor: usize,
) -> Vec<String> {
    let sep = "─".repeat(72);
    let mut out = Vec::new();
    out.push(format!(
        "\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}\n"
    ));
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!(
        "  {BOLD}Launch configuration{R}  {DIM}— pick what to start{R}"
    ));
    out.push(format!("  {DIM}{sep}{R}\n"));

    for (i, c) in choices.iter().enumerate() {
        let marker = if i == cursor {
            format!("{CYN}{BOLD}▶{R} ")
        } else {
            "  ".to_string()
        };

        let checkbox = if !c.available && c.branch.is_empty() {
            format!("{DIM}[–]{R}")
        } else if c.enabled {
            format!("{GRN}[{BOLD}✓{R}{GRN}]{R}")
        } else {
            format!("{DIM}[ ]{R}")
        };

        let name = if !c.available && c.branch.is_empty() {
            format!("{DIM}{}{R}", c.label)
        } else if i == cursor {
            format!("{BOLD}{}{R}", c.label)
        } else {
            c.label.to_string()
        };

        let desc = if !c.available && c.branch.is_empty() {
            format!("{DIM}not found{R}")
        } else {
            format!("{DIM}{}{R}", c.desc)
        };

        let branch_col = if c.branch.is_empty() {
            String::new()
        } else if let Some(hash) = parse_commit_ref(&c.branch) {
            format!("  {DIM}@{hash} (detached){R}")
        } else {
            format!("  {DIM}{}{R}", c.branch)
        };

        out.push(format!(
            "  {marker}{checkbox}  {:<22}{:<26}{branch_col}",
            name, desc
        ));
    }

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!("  {DIM}↑↓ / j k  navigate   Space toggle   b branch   Enter start   c clean start   q quit{R}"));
    out.push(String::new());
    out
}

/// Interactive product-selection screen.
pub fn run_product_selector(slug: &str, choices: &mut [ProductChoice]) -> LaunchMode {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return LaunchMode::Normal;
    }

    let mut raw = TuiGuard::enter();
    let mut cursor = 0usize;
    if let Some(first) = choices
        .iter()
        .position(|c| c.available || !c.branch.is_empty())
    {
        cursor = first;
    }

    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(tui, &build_product_selector_lines(slug, choices, cursor));
        }

        if !event::poll(std::time::Duration::from_millis(20)).unwrap_or(false) {
            continue;
        }
        let Ok(Event::Key(ke)) = event::read() else {
            continue;
        };
        if ke.kind != crossterm::event::KeyEventKind::Press {
            continue;
        }

        match ke.code {
            KeyCode::Up | KeyCode::Char('k') => {
                cursor = cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if cursor + 1 < choices.len() => {
                cursor += 1;
            }
            KeyCode::Char(' ') => {
                if choices[cursor].available || !choices[cursor].branch.is_empty() {
                    choices[cursor].enabled = !choices[cursor].enabled;
                } else {
                    drop(raw.take());
                    print!("\n  Branch for {} : ", choices[cursor].label);
                    let _ = io::stdout().flush();
                    if let Some(input) = read_line_or_interrupt() {
                        let trimmed = input.trim().to_string();
                        if !trimmed.is_empty() {
                            choices[cursor].branch = trimmed;
                            choices[cursor].enabled = true;
                            choices[cursor].available = true;
                        }
                    }
                    raw = TuiGuard::enter();
                }
            }
            KeyCode::Char('b') | KeyCode::Char('B') => {
                drop(raw.take());
                let current = &choices[cursor].branch;
                if current.is_empty() {
                    print!("\n  Branch for {} : ", choices[cursor].label);
                } else {
                    print!(
                        "\n  Branch for {} (Enter to keep {current}): ",
                        choices[cursor].label
                    );
                }
                let _ = io::stdout().flush();
                if let Some(input) = read_line_or_interrupt() {
                    let trimmed = input.trim().to_string();
                    if !trimmed.is_empty() {
                        choices[cursor].branch = trimmed;
                        choices[cursor].enabled = true;
                        choices[cursor].available = true;
                    }
                }
                raw = TuiGuard::enter();
            }
            KeyCode::Enter => {
                return LaunchMode::Normal;
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                return LaunchMode::Clean;
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                print!("\x1b[H\x1b[2J");
                let _ = io::stdout().flush();
                return LaunchMode::Quit;
            }
            _ => {}
        }
    }
}

// ── Workspace ↔ ProductChoice conversions ────────────────────────────────────

pub fn workspace_to_choices(config: &WorkspaceConfig, workspace_root: &Path) -> Vec<ProductChoice> {
    PRODUCTS
        .iter()
        .map(|(repo, label, _, desc)| {
            let saved = config.entries.iter().find(|e| e.repo.as_str() == *repo);
            let branch = saved.map(|e| e.branch.clone()).unwrap_or_default();
            let enabled = saved.map(|e| e.enabled).unwrap_or(false);
            let path = if branch.is_empty() {
                workspace_root.join(repo)
            } else {
                let slug = branch_to_slug(&branch);
                let wt = workspace_root.join(format!("{}-{}", repo, slug));
                if wt.is_dir() {
                    wt
                } else {
                    workspace_root.join(repo)
                }
            };
            ProductChoice {
                label,
                desc,
                repo,
                enabled,
                available: path.is_dir() || workspace_root.join(repo).is_dir(),
                branch,
            }
        })
        .collect()
}

pub fn choices_to_workspace(choices: &[ProductChoice]) -> WorkspaceConfig {
    use crate::workspace::{compute_workspace_hash, today, WorkspaceEntry};
    let entries: Vec<WorkspaceEntry> = choices
        .iter()
        .map(|c| WorkspaceEntry {
            repo: c.repo.to_string(),
            enabled: c.enabled,
            branch: c.branch.clone(),
        })
        .collect();
    let hash = compute_workspace_hash(&entries);
    WorkspaceConfig {
        hash,
        created: today(),
        entries,
    }
}

pub fn default_product_choices(workspace_root: &Path) -> Vec<ProductChoice> {
    PRODUCTS
        .iter()
        .map(|(repo, label, _, desc)| {
            let main_dir = workspace_root.join(repo);
            let available = main_dir.is_dir();
            ProductChoice {
                label,
                desc,
                repo,
                enabled: false,
                available,
                branch: String::new(),
            }
        })
        .collect()
}

// ── Flag selector ─────────────────────────────────────────────────────────────

pub struct FlagChoice {
    pub name: String,
    pub enabled: bool,
}

pub fn build_flag_selector_lines(
    slug: &str,
    product: &str,
    choices: &[FlagChoice],
    cursor: usize,
) -> Vec<String> {
    let sep = "─".repeat(56);
    let mut out = Vec::new();
    out.push(format!(
        "\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}\n"
    ));
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!("  {BOLD}Feature flags{R}  {DIM}— {product}{R}"));
    out.push(format!("  {DIM}{sep}{R}\n"));

    for (i, c) in choices.iter().enumerate() {
        let marker = if i == cursor {
            format!("{CYN}{BOLD}▶{R} ")
        } else {
            "  ".to_string()
        };
        let checkbox = if c.enabled {
            format!("{GRN}[{BOLD}✓{R}{GRN}]{R}")
        } else {
            format!("{DIM}[ ]{R}")
        };
        let name = if i == cursor {
            format!("{BOLD}{}{R}", c.name)
        } else {
            c.name.clone()
        };
        out.push(format!("  {marker}{checkbox}  {name}"));
    }

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!(
        "  {DIM}↑↓ / j k  navigate   Space  toggle   Enter  confirm   q  skip{R}"
    ));
    out.push(String::new());
    out
}

/// Interactive feature-flag selector for one product.
pub fn run_flag_selector(slug: &str, product: &str, choices: &mut [FlagChoice]) {
    if choices.is_empty() || unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return;
    }
    let mut raw = TuiGuard::enter();
    let mut cursor = 0usize;
    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(
                tui,
                &build_flag_selector_lines(slug, product, choices, cursor),
            );
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
                KeyCode::Char(' ') => {
                    choices[cursor].enabled = !choices[cursor].enabled;
                }
                KeyCode::Enter => {
                    return;
                }
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                    return;
                }
                _ => {}
            }
        }
    }
}

// ── Feature flag file discovery ───────────────────────────────────────────────

pub fn discover_flags_in_dir(dir: &Path, out: &mut std::collections::BTreeSet<String>) {
    use std::fs;
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "node_modules" || n == ".git")
            {
                continue;
            }
            discover_flags_in_dir(&path, out);
        } else {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "ts" | "js" | "tsx" | "jsx") {
                discover_flags_in_file(&path, out);
            }
        }
    }
}

fn discover_flags_in_file(path: &Path, out: &mut std::collections::BTreeSet<String>) {
    use std::fs;
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    for needle in &["isFeatureEnabled(", "isFeatureEnable("] {
        extract_flag_calls(&content, needle, out);
    }
}

fn extract_flag_calls(content: &str, needle: &str, out: &mut std::collections::BTreeSet<String>) {
    let mut search = content;
    while let Some(idx) = search.find(needle) {
        search = &search[idx + needle.len()..];
        if needle == "isFeatureEnable(" && search.starts_with('d') {
            continue;
        }
        let rest = search.trim_start_matches(' ');
        let quote = match rest.chars().next() {
            Some('\'') => '\'',
            Some('"') => '"',
            _ => continue,
        };
        let inner = &rest[1..];
        if let Some(end) = inner.find(quote) {
            let flag = &inner[..end];
            if !flag.is_empty() && flag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                out.insert(flag.to_string());
            }
        }
    }
}

/// Parse the `APP__ENABLED_DEV_FEATURES` JSON array from an env file.
pub fn read_active_flags(env_file: &Path) -> Vec<String> {
    let map = parse_env_file(env_file);
    let raw = map
        .get("APP__ENABLED_DEV_FEATURES")
        .cloned()
        .unwrap_or_default();
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.is_empty() {
        return vec![];
    }
    trimmed
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Write `flags` back into `APP__ENABLED_DEV_FEATURES` in the env file.
pub fn write_active_flags(env_file: &Path, flags: &[String]) {
    use crate::workspace::env::write_env_file;
    let mut map = parse_env_file(env_file);
    let val = if flags.is_empty() {
        "[]".to_string()
    } else {
        let inner = flags
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!("[{inner}]")
    };
    map.insert("APP__ENABLED_DEV_FEATURES".to_string(), val);
    write_env_file(env_file, &map);
}
