use std::collections::HashSet;
use std::io::{self, Write};
use std::path::Path;

use crossterm::event::{self, Event, KeyCode};

use crate::config::read_line_or_interrupt;
use crate::services::{workspace_run_status, WorkspaceRunStatus};
use crate::tui::{
    drain_input_events, draw_ansi_lines, TuiGuard, BOLD, BUILD_VERSION, CYN, DIM, GRN, R, RED, YLW,
};
use crate::workspace::env::parse_env_file;
use crate::workspace::git::{
    branch_to_slug, parse_commit_ref, worktree_delete_blockers, worktree_dirty_reasons,
};
use crate::workspace::{is_infra_product, WorkspaceConfig, PRODUCTS};

pub enum WorkspaceAction {
    Open(WorkspaceConfig),
    Delete(WorkspaceConfig),
    CreateNew,
    Reattach(WorkspaceConfig),
    StopSession(WorkspaceConfig),
    OpenInCode(WorkspaceConfig),
    Quit,
}

fn build_workspace_selector_lines(
    workspaces: &[WorkspaceConfig],
    cursor: usize,
    stopped_hashes: &HashSet<String>,
) -> Vec<String> {
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
        // Port offset is chosen dynamically per launch; surface it here only
        // when a session is running (read from the runtime snapshot).
        let offset_tag = match crate::control::read_snapshot(&ws.hash) {
            Some(s) if s.port_offset > 0 => format!("  {DIM}+{}{R}", s.port_offset),
            _ => String::new(),
        };
        let summary_display = if summary.len() > 52 {
            format!("{}…", &summary[..51])
        } else {
            summary.clone()
        };
        let status_dot = if stopped_hashes.contains(&ws.hash) {
            format!(" {CYN}●{R}")
        } else {
            match workspace_run_status(&ws.hash) {
                WorkspaceRunStatus::Running => format!(" {GRN}●{R}"),
                WorkspaceRunStatus::Degraded => format!(" {YLW}●{R}"),
                WorkspaceRunStatus::Failed => format!(" {RED}●{R}"),
                WorkspaceRunStatus::NotRunning => "  ".to_string(),
            }
        };
        out.push(format!(
            "  {marker}{hash}{status_dot}  {:<54}{date}{offset_tag}",
            summary_display
        ));
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
        let selected_stopped =
            cursor < workspaces.len() && stopped_hashes.contains(&workspaces[cursor].hash);
        if selected_stopped {
            out.push(format!(
                "  {DIM}↑↓ navigate   r reattach   s stop   o code   d delete   q quit{R}"
            ));
        } else {
            out.push(format!(
                "  {DIM}↑↓ navigate   Enter open   o code   d delete   q quit{R}"
            ));
        }
    } else {
        out.push(format!("  {DIM}↑↓ navigate   Enter create   q quit{R}"));
    }
    out.push(String::new());
    out
}

pub fn run_workspace_selector(
    workspaces: &[WorkspaceConfig],
    stopped_hashes: &HashSet<String>,
) -> WorkspaceAction {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return WorkspaceAction::CreateNew;
    }
    let mut raw = TuiGuard::enter();
    let total = workspaces.len() + 1;
    let mut cursor = 0usize;
    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(
                tui,
                &build_workspace_selector_lines(workspaces, cursor, stopped_hashes),
            );
        }
        if event::poll(std::time::Duration::from_millis(20)).unwrap_or(false) {
            let Ok(Event::Key(ke)) = event::read() else {
                continue;
            };
            if ke.kind != crossterm::event::KeyEventKind::Press {
                continue;
            }
            let selected_stopped =
                cursor < workspaces.len() && stopped_hashes.contains(&workspaces[cursor].hash);
            match ke.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if cursor + 1 < total => {
                    cursor += 1;
                }
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                    drain_input_events();
                    drop(raw.take());
                    if cursor == workspaces.len() {
                        return WorkspaceAction::CreateNew;
                    } else if selected_stopped {
                        return WorkspaceAction::Reattach(workspaces[cursor].clone());
                    } else {
                        return WorkspaceAction::Open(workspaces[cursor].clone());
                    }
                }
                KeyCode::Char('r') | KeyCode::Char('R') if selected_stopped => {
                    drain_input_events();
                    drop(raw.take());
                    return WorkspaceAction::Reattach(workspaces[cursor].clone());
                }
                KeyCode::Char('s') | KeyCode::Char('S') if selected_stopped => {
                    drain_input_events();
                    drop(raw.take());
                    return WorkspaceAction::StopSession(workspaces[cursor].clone());
                }
                KeyCode::Char('o') | KeyCode::Char('O') if cursor < workspaces.len() => {
                    drain_input_events();
                    drop(raw.take());
                    return WorkspaceAction::OpenInCode(workspaces[cursor].clone());
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
        println!(
            "  {RED}{BOLD}Warning: the following worktrees have unresolved Git blockers:{R}\n"
        );
        for b in &blocked {
            println!(
                "  {RED}▶{R}  {BOLD}{}{R}  ({})",
                b.repo,
                b.reasons.join(", ")
            );
            println!("     {DIM}{}{R}", b.worktree.display());
        }
        println!();
        println!(
            "  {DIM}Forcing removal may permanently lose uncommitted work or unpushed branches.{R}"
        );
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

        let ws_override = write_compose_override(&compose_file, ws_hash, 0);
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
            if c.enabled && c.available && !crate::workspace::is_infra_product(c.repo) {
                format!("  {DIM}→ main{R}")
            } else {
                String::new()
            }
        } else if crate::workspace::is_main_tracking_slug(&c.branch) {
            format!("  {DIM}→ main{R}")
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
                let c = &choices[cursor];
                if c.repo == "infinity" {
                    if c.enabled {
                        choices[cursor].enabled = false;
                    } else {
                        let current = if c.branch.is_empty() {
                            "nomic-ai/nomic-embed-text-v1.5"
                        } else {
                            &c.branch
                        };
                        drop(raw.take());
                        let model = run_model_selector(current);
                        choices[cursor].branch = model;
                        choices[cursor].enabled = true;
                        raw = TuiGuard::enter();
                    }
                } else if c.available || !c.branch.is_empty() || is_infra_product(c.repo) {
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
                if choices[cursor].repo == "infinity" {
                    let current = if choices[cursor].branch.is_empty() {
                        "nomic-ai/nomic-embed-text-v1.5"
                    } else {
                        &choices[cursor].branch
                    };
                    drop(raw.take());
                    let model = run_model_selector(current);
                    if !model.is_empty() {
                        choices[cursor].branch = model;
                        choices[cursor].enabled = true;
                    }
                    raw = TuiGuard::enter();
                } else if is_infra_product(choices[cursor].repo) {
                    // other infra products have no branches
                } else {
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
        .map(|(repo, label, key, desc)| {
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
            // Infra products are always available — directories are bootstrapped on launch.
            let available =
                is_infra_product(key) || path.is_dir() || workspace_root.join(repo).is_dir();
            ProductChoice {
                label,
                desc,
                repo,
                enabled,
                available,
                branch,
            }
        })
        .collect()
}

pub fn choices_to_workspace(choices: &[ProductChoice]) -> WorkspaceConfig {
    use crate::workspace::{
        compute_workspace_hash, generate_user_slug, is_infra_product, today, WorkspaceEntry,
    };

    // One slug shared across all products in this workspace session — any enabled
    // non-infra product without an explicit branch gets this slug so each launch
    // creates a distinct worktree that tracks origin/main.
    let auto_slug: Option<String> = {
        let needs_slug = choices
            .iter()
            .any(|c| c.enabled && c.branch.is_empty() && !is_infra_product(c.repo));
        if needs_slug {
            Some(generate_user_slug())
        } else {
            None
        }
    };

    let entries: Vec<WorkspaceEntry> = choices
        .iter()
        .map(|c| {
            let branch = if c.branch.is_empty() && c.enabled && !is_infra_product(c.repo) {
                auto_slug.clone().unwrap_or_default()
            } else {
                c.branch.clone()
            };
            WorkspaceEntry {
                repo: c.repo.to_string(),
                enabled: c.enabled,
                branch,
            }
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
        .map(|(repo, label, key, desc)| {
            let main_dir = workspace_root.join(repo);
            // Infra products are always available — directories are bootstrapped on launch.
            let available = is_infra_product(key) || main_dir.is_dir();
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
        "  {DIM}↑↓ / j k  navigate   Space  toggle   Enter  confirm   q  skip   Esc back to menu{R}"
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
                KeyCode::Char('q') | KeyCode::Char('Q') => {
                    return;
                }
                KeyCode::Esc => {
                    drop(raw.take());
                    crate::tui::exit_to_selector_menu();
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

// ── AutoResearch helpers ──────────────────────────────────────────────────────

/// Parse the `branch` field of an autoresearch WorkspaceEntry.
/// Format: `"{model}|{backend}"`, e.g., `"Qwen/Qwen2.5-0.5B|metal"`.
pub fn parse_autoresearch_branch(branch: &str) -> (String, String) {
    if let Some((model, backend)) = branch.split_once('|') {
        (model.to_string(), backend.to_string())
    } else if branch.is_empty() {
        (String::new(), String::new())
    } else {
        (branch.to_string(), "cpu".to_string())
    }
}

// ── Infinity model selector ───────────────────────────────────────────────────

/// Well-known embedding models compatible with infinity-emb.
const INFINITY_MODELS: &[(&str, &str)] = &[
    (
        "nomic-ai/nomic-embed-text-v1.5",
        "best quality/speed tradeoff",
    ),
    ("BAAI/bge-small-en-v1.5", "fast · small English"),
    ("BAAI/bge-base-en-v1.5", "balanced English"),
    ("BAAI/bge-large-en-v1.5", "high quality English, slower"),
    (
        "sentence-transformers/all-MiniLM-L6-v2",
        "classic general-purpose",
    ),
    ("intfloat/e5-small-v2", "small multilingual"),
    ("intfloat/e5-base-v2", "base multilingual"),
    ("intfloat/e5-large-v2", "large multilingual"),
    ("thenlper/gte-small", "small GTE"),
    ("thenlper/gte-base", "base GTE"),
    ("thenlper/gte-large", "large GTE"),
];

struct ModelEntry {
    id: String,
    desc: &'static str,
    cached: bool,
}

/// Scan the HuggingFace hub cache and return model IDs that are already downloaded.
fn find_cached_hf_models() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let cache = std::path::PathBuf::from(&home).join(".cache/huggingface/hub");
    if !cache.is_dir() {
        return vec![];
    }
    let mut models = Vec::new();
    for entry in std::fs::read_dir(&cache).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Cache dirs are named `models--{org}--{repo}` with `--` as separator.
        if let Some(rest) = name.strip_prefix("models--") {
            let id = rest.replace("--", "/");
            if !id.is_empty() {
                models.push(id);
            }
        }
    }
    models.sort();
    models
}

fn build_model_entries(current: &str) -> Vec<ModelEntry> {
    let cached = find_cached_hf_models();
    let mut entries: Vec<ModelEntry> = Vec::new();

    // Cached models first (already on disk).
    for id in &cached {
        let desc = INFINITY_MODELS
            .iter()
            .find(|(m, _)| *m == id.as_str())
            .map(|(_, d)| *d)
            .unwrap_or("locally cached");
        entries.push(ModelEntry {
            id: id.clone(),
            desc,
            cached: true,
        });
    }
    // Popular models not yet downloaded.
    for (id, desc) in INFINITY_MODELS {
        if !cached.iter().any(|c| c == id) {
            entries.push(ModelEntry {
                id: id.to_string(),
                desc,
                cached: false,
            });
        }
    }
    // If the current model isn't in the list at all, prepend it.
    if !current.is_empty() && !entries.iter().any(|e| e.id == current) {
        entries.insert(
            0,
            ModelEntry {
                id: current.to_string(),
                desc: "custom",
                cached: false,
            },
        );
    }
    entries
}

fn build_model_selector_lines(entries: &[ModelEntry], cursor: usize, current: &str) -> Vec<String> {
    let sep = "─".repeat(72);
    let mut out = Vec::new();
    out.push(format!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}\n"));
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!(
        "  {BOLD}Infinity Emb — embedding model{R}  \
         {DIM}↓ cached locally  · will download on first use{R}"
    ));
    out.push(format!("  {DIM}{sep}{R}\n"));

    for (i, e) in entries.iter().enumerate() {
        let marker = if i == cursor {
            format!("{CYN}{BOLD}▶{R} ")
        } else {
            "  ".to_string()
        };
        let active = if e.id == current {
            format!("{GRN}[{BOLD}✓{R}{GRN}]{R}")
        } else {
            format!("{DIM}[ ]{R}")
        };
        let cache_tag = if e.cached {
            format!("  {GRN}{DIM}↓{R}")
        } else {
            String::new()
        };
        let id_col = if i == cursor {
            format!("{BOLD}{}{R}", e.id)
        } else {
            e.id.clone()
        };
        out.push(format!(
            "  {marker}{active}  {id_col:<54}{DIM}{}{R}{cache_tag}",
            e.desc
        ));
    }

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!(
        "  {DIM}↑↓ navigate   Enter select   m custom model ID   q cancel{R}"
    ));
    out.push(String::new());
    out
}

/// Show the infinity embedding-model picker.  Returns the chosen model ID, or
/// `current` unchanged if the user cancels.
pub fn run_model_selector(current: &str) -> String {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return if current.is_empty() {
            "nomic-ai/nomic-embed-text-v1.5".to_string()
        } else {
            current.to_string()
        };
    }

    let entries = build_model_entries(current);
    let mut cursor = entries.iter().position(|e| e.id == current).unwrap_or(0);
    let mut raw = TuiGuard::enter();

    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(tui, &build_model_selector_lines(&entries, cursor, current));
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
            KeyCode::Down | KeyCode::Char('j') if cursor + 1 < entries.len() => {
                cursor += 1;
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                drain_input_events();
                drop(raw.take());
                return entries[cursor].id.clone();
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                drain_input_events();
                drop(raw.take());
                print!("\n  Custom HuggingFace model ID: ");
                let _ = io::stdout().flush();
                let result = if let Some(input) = read_line_or_interrupt() {
                    let t = input.trim().to_string();
                    if t.is_empty() {
                        current.to_string()
                    } else {
                        t
                    }
                } else {
                    current.to_string()
                };
                return result;
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                drain_input_events();
                drop(raw.take());
                return current.to_string();
            }
            _ => {}
        }
    }
}

// ── AutoResearch SLM selector ─────────────────────────────────────────────────

/// Well-known small language models suited for local autoresearch experimentation.
const AUTORESEARCH_SLM_MODELS: &[(&str, &str)] = &[
    ("Qwen/Qwen2.5-0.5B", "0.5 B params — fastest, least RAM"),
    ("Qwen/Qwen2.5-1.5B", "1.5 B params — good balance"),
    ("Qwen/Qwen2.5-3B", "3 B params — higher quality"),
    ("Qwen/Qwen2.5-7B", "7 B params — strong reasoning"),
    (
        "HuggingFaceTB/SmolLM2-1.7B-Instruct",
        "1.7 B · instruction-tuned",
    ),
    ("microsoft/Phi-3-mini-4k-instruct", "3.8 B · Phi-3 Mini"),
    ("google/gemma-3-1b-it", "1 B · Gemma 3 instruction"),
    ("google/gemma-3-4b-it", "4 B · Gemma 3 instruction"),
    (
        "meta-llama/Llama-3.2-1B-Instruct",
        "1 B · Llama 3.2 instruction",
    ),
    (
        "meta-llama/Llama-3.2-3B-Instruct",
        "3 B · Llama 3.2 instruction",
    ),
];

fn build_slm_entries(current: &str) -> Vec<ModelEntry> {
    let cached = find_cached_hf_models();
    let mut entries: Vec<ModelEntry> = Vec::new();

    for id in &cached {
        if AUTORESEARCH_SLM_MODELS
            .iter()
            .any(|(m, _)| *m == id.as_str())
        {
            let desc = AUTORESEARCH_SLM_MODELS
                .iter()
                .find(|(m, _)| *m == id.as_str())
                .map(|(_, d)| *d)
                .unwrap_or("locally cached");
            entries.push(ModelEntry {
                id: id.clone(),
                desc,
                cached: true,
            });
        }
    }
    for (id, desc) in AUTORESEARCH_SLM_MODELS {
        if !cached.iter().any(|c| c == id) {
            entries.push(ModelEntry {
                id: id.to_string(),
                desc,
                cached: false,
            });
        }
    }
    if !current.is_empty() && !entries.iter().any(|e| e.id == current) {
        entries.insert(
            0,
            ModelEntry {
                id: current.to_string(),
                desc: "custom",
                cached: false,
            },
        );
    }
    entries
}

fn build_slm_selector_lines(entries: &[ModelEntry], cursor: usize, current: &str) -> Vec<String> {
    let sep = "─".repeat(72);
    let mut out = Vec::new();
    out.push(format!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}\n"));
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!(
        "  {BOLD}AutoResearch — SLM model{R}  \
         {DIM}↓ cached locally  · will download on first use{R}"
    ));
    out.push(format!("  {DIM}{sep}{R}\n"));

    for (i, e) in entries.iter().enumerate() {
        let marker = if i == cursor {
            format!("{CYN}{BOLD}▶{R} ")
        } else {
            "  ".to_string()
        };
        let active = if e.id == current {
            format!("{GRN}[{BOLD}✓{R}{GRN}]{R}")
        } else {
            format!("{DIM}[ ]{R}")
        };
        let cache_tag = if e.cached {
            format!("  {GRN}{DIM}↓{R}")
        } else {
            String::new()
        };
        out.push(format!(
            "  {marker}{active}  {:<42}  {DIM}{}{R}{cache_tag}",
            e.id, e.desc
        ));
    }

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!(
        "  {DIM}↑↓ / j k  navigate   Enter select   m custom HF model id   q back{R}"
    ));
    out.push(String::new());
    out
}

/// Show the SLM model picker for AutoResearch.  Returns the chosen model ID.
pub fn run_slm_selector(current: &str) -> String {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return if current.is_empty() {
            "Qwen/Qwen2.5-0.5B".to_string()
        } else {
            current.to_string()
        };
    }

    let entries = build_slm_entries(current);
    let mut cursor = entries.iter().position(|e| e.id == current).unwrap_or(0);
    let mut raw = TuiGuard::enter();

    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(tui, &build_slm_selector_lines(&entries, cursor, current));
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
            KeyCode::Down | KeyCode::Char('j') if cursor + 1 < entries.len() => {
                cursor += 1;
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                drain_input_events();
                drop(raw.take());
                return entries[cursor].id.clone();
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                drain_input_events();
                drop(raw.take());
                print!("\n  Custom HuggingFace model ID: ");
                let _ = io::stdout().flush();
                let result = if let Some(input) = read_line_or_interrupt() {
                    let t = input.trim().to_string();
                    if t.is_empty() {
                        current.to_string()
                    } else {
                        t
                    }
                } else {
                    current.to_string()
                };
                return result;
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                drain_input_events();
                drop(raw.take());
                return current.to_string();
            }
            _ => {}
        }
    }
}

// ── AutoResearch GPU backend selector ─────────────────────────────────────────

const GPU_BACKENDS: &[(&str, &str)] = &[
    ("metal", "Metal   (Apple Silicon — MPS)"),
    ("cuda", "CUDA    (NVIDIA GPU)"),
    ("cpu", "CPU     (fallback — slow for large models)"),
];

fn build_gpu_selector_lines(cursor: usize, current: &str) -> Vec<String> {
    let sep = "─".repeat(72);
    let mut out = Vec::new();
    out.push(format!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}\n"));
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!("  {BOLD}AutoResearch — torch backend{R}"));
    out.push(format!("  {DIM}{sep}{R}\n"));

    for (i, (id, label)) in GPU_BACKENDS.iter().enumerate() {
        let marker = if i == cursor {
            format!("{CYN}{BOLD}▶{R} ")
        } else {
            "  ".to_string()
        };
        let active = if *id == current {
            format!("{GRN}[{BOLD}✓{R}{GRN}]{R}")
        } else {
            format!("{DIM}[ ]{R}")
        };
        out.push(format!("  {marker}{active}  {}", label));
    }

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!(
        "  {DIM}↑↓ / j k  navigate   Enter select   q back{R}"
    ));
    out.push(String::new());
    out
}

/// Show the GPU backend picker for AutoResearch.  Returns "metal", "cuda", or "cpu".
pub fn run_gpu_backend_selector(current: &str) -> String {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return if current.is_empty() {
            "cpu".to_string()
        } else {
            current.to_string()
        };
    }

    let mut cursor = GPU_BACKENDS
        .iter()
        .position(|(id, _)| *id == current)
        .unwrap_or(0);
    let mut raw = TuiGuard::enter();

    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(tui, &build_gpu_selector_lines(cursor, current));
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
            KeyCode::Down | KeyCode::Char('j') if cursor + 1 < GPU_BACKENDS.len() => {
                cursor += 1;
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                drain_input_events();
                drop(raw.take());
                return GPU_BACKENDS[cursor].0.to_string();
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                drain_input_events();
                drop(raw.take());
                return current.to_string();
            }
            _ => {}
        }
    }
}
