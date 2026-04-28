use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::{
    cursor,
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};

use crate::tui::{BOLD, BUILD_VERSION, CYN, DIM, GRN, R};

const EMBEDDED_RECIPES: &str = env!("RECIPES_EMBEDDED_DIR");
const GIT_ORIGIN: &str = env!("GIT_ORIGIN_URL");
const TIMEOUT_SECS: u64 = 15;

enum SyncMsg {
    Step { progress: f32, status: &'static str },
    Done(PathBuf),
    Error,
}

/// Run the recipe-update splash screen and return the recipes directory to use.
/// Falls back to the compile-time bundled path on timeout or failure.
pub fn run() -> PathBuf {
    if GIT_ORIGIN.is_empty() {
        return PathBuf::from(EMBEDDED_RECIPES);
    }

    let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } != 0;
    let (tx, rx) = mpsc::channel::<SyncMsg>();
    let origin = GIT_ORIGIN.to_string();

    thread::spawn(move || do_sync(&origin, tx));

    if !is_tty {
        return recv_headless(rx);
    }

    run_animated(rx)
}

// ── Headless (non-TTY) path ───────────────────────────────────────────────────

fn recv_headless(rx: mpsc::Receiver<SyncMsg>) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(TIMEOUT_SECS);
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(SyncMsg::Done(p)) => return p,
            Ok(SyncMsg::Error) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return PathBuf::from(EMBEDDED_RECIPES);
            }
            Ok(SyncMsg::Step { .. }) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    return PathBuf::from(EMBEDDED_RECIPES);
                }
            }
        }
    }
}

// ── Animated TUI path ─────────────────────────────────────────────────────────

fn run_animated(rx: mpsc::Receiver<SyncMsg>) -> PathBuf {
    if enable_raw_mode().is_err() {
        return recv_headless(rx);
    }
    let mut stdout = io::stdout();
    if execute!(stdout, EnterAlternateScreen, cursor::Hide).is_err() {
        let _ = disable_raw_mode();
        return recv_headless(rx);
    }

    let result = animate_loop(rx);

    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
    let _ = disable_raw_mode();
    result
}

fn animate_loop(rx: mpsc::Receiver<SyncMsg>) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(TIMEOUT_SECS);
    let mut progress: f32 = 0.0;
    let mut target: f32 = 0.05;
    let mut status: &str = "Initializing…";
    let mut result: Option<PathBuf> = None;
    let origin_label = display_origin(GIT_ORIGIN);

    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                SyncMsg::Step { progress: p, status: s } => {
                    target = p;
                    status = s;
                }
                SyncMsg::Done(path) => {
                    target = 1.0;
                    status = "Ready";
                    result = Some(path);
                }
                SyncMsg::Error => {
                    target = 1.0;
                    status = "Using bundled recipes";
                    result = Some(PathBuf::from(EMBEDDED_RECIPES));
                }
            }
        }

        if result.is_none() && Instant::now() >= deadline {
            target = 1.0;
            status = "Timed out — using bundled recipes";
            result = Some(PathBuf::from(EMBEDDED_RECIPES));
        }

        let gap = target - progress;
        if gap > 0.001 {
            progress += gap * 0.08;
        } else {
            progress = target;
        }

        render(progress, status, &origin_label);

        if result.is_some() && progress >= 0.999 {
            thread::sleep(Duration::from_millis(280));
            return result.unwrap();
        }

        thread::sleep(Duration::from_millis(16));
    }
}

// ── Render ─────────────────────────────────────────────────────────────────────

fn render(progress: f32, status: &str, origin_label: &str) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (cols, rows) = (cols as usize, rows as usize);

    let mut out = io::stdout();
    let _ = execute!(out, Clear(ClearType::All), cursor::MoveTo(0, 0));

    // Content: header + blank + status + blank + bar + blank + origin = 7 lines
    let content_lines: usize = 7;
    let pad_top = rows.saturating_sub(content_lines) / 2;
    for _ in 0..pad_top {
        let _ = write!(out, "\r\n");
    }

    // Header
    let _ = writeln!(out, "  {BOLD}{CYN}{BUILD_VERSION}{R}\r");
    let _ = writeln!(out, "\r");

    // Status — dim while in-progress, green when done
    if status == "Ready" {
        let _ = writeln!(out, "  {GRN}{status}{R}\r");
    } else {
        let _ = writeln!(out, "  {DIM}{status}{R}\r");
    }
    let _ = writeln!(out, "\r");

    // Progress bar: ████░░░░  38%
    let bar_width = cols.saturating_sub(14).min(56);
    let filled = ((progress * bar_width as f32).round() as usize).min(bar_width);
    let empty = bar_width - filled;
    let pct = (progress * 100.0).round() as u32;
    let _ = writeln!(
        out,
        "  {CYN}{}{R}{DIM}{}{R}  {DIM}{pct}%{R}\r",
        "█".repeat(filled),
        "░".repeat(empty),
    );

    // Origin label
    if !origin_label.is_empty() && status != "Ready" {
        let _ = writeln!(out, "\r");
        let _ = writeln!(out, "  {DIM}{origin_label}{R}\r");
    }

    let _ = writeln!(out, "\r");

    // Hint: user can press any key to skip (future) – for now just ensure flush
    let _ = out.flush();
}

fn display_origin(url: &str) -> String {
    let s = url.trim();
    let s = s.strip_prefix("https://").unwrap_or(s);
    let s = s.strip_prefix("http://").unwrap_or(s);
    let s = s.strip_prefix("git@").unwrap_or(s);
    s.trim_end_matches(".git").to_string()
}

// ── Sync worker (background thread) ──────────────────────────────────────────

fn do_sync(origin: &str, tx: mpsc::Sender<SyncMsg>) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let store_dir = PathBuf::from(&home).join(".dev-launcher").join("recipe-store");
    let recipes_dir = store_dir.join("recipes");

    let _ = tx.send(SyncMsg::Step { progress: 0.10, status: "Checking recipe cache…" });

    let git_dir = store_dir.join(".git");
    if !git_dir.exists() {
        let _ = tx.send(SyncMsg::Step { progress: 0.20, status: "Cloning recipe database…" });

        // Remove any leftover partial clone dir before cloning
        if store_dir.exists() {
            let _ = std::fs::remove_dir_all(&store_dir);
        }

        if !sparse_clone(origin, &store_dir) {
            // Sparse checkout failed — try a plain shallow clone
            let _ = tx.send(SyncMsg::Step { progress: 0.30, status: "Cloning (shallow)…" });
            let ok = Command::new("git")
                .env("GIT_TERMINAL_PROMPT", "0")
                .args([
                    "clone",
                    "--depth", "1",
                    "--quiet",
                    origin,
                    store_dir.to_str().unwrap_or("."),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                let _ = tx.send(SyncMsg::Error);
                return;
            }
        }
    } else {
        let _ = tx.send(SyncMsg::Step { progress: 0.40, status: "Pulling latest recipes…" });
        let _ = Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-C", store_dir.to_str().unwrap_or("."),
                "pull", "--ff-only", "--quiet",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
    }

    let _ = tx.send(SyncMsg::Step { progress: 0.90, status: "Loading recipes…" });
    let _ = tx.send(SyncMsg::Done(recipes_dir));
}

/// Git sparse-checkout clone — only fetches the `recipes/` subtree.
fn sparse_clone(origin: &str, dest: &std::path::Path) -> bool {
    let dest_str = match dest.to_str() {
        Some(s) => s,
        None => return false,
    };

    // Step 1: clone without checking out
    let ok = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "clone",
            "--filter=blob:none",
            "--no-checkout",
            "--depth", "1",
            "--quiet",
            origin,
            dest_str,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return false;
    }

    // Step 2: enable cone sparse-checkout
    let ok2 = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-C", dest_str, "sparse-checkout", "init", "--cone"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok2 {
        return false;
    }

    // Step 3: limit to recipes/
    let ok3 = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-C", dest_str, "sparse-checkout", "set", "recipes"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok3 {
        return false;
    }

    // Step 4: checkout
    Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-C", dest_str, "checkout"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
