pub mod credentials;
pub mod diagnose;
pub mod logview;
pub mod overview;
pub mod splash;

pub use credentials::{build_credentials_lines, gather_credentials, CredEntry};
pub use diagnose::build_diagnose_lines;
pub use logview::{build_log_view_lines, tail_file};
pub use overview::build_overview_lines;

use std::io::{self, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread;
use std::time::Duration;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

// ── ANSI ──────────────────────────────────────────────────────────────────────

pub const R: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const GRN: &str = "\x1b[32m";
pub const YLW: &str = "\x1b[33m";
pub const RED: &str = "\x1b[31m";
pub const CYN: &str = "\x1b[36m";

// ── Build version ─────────────────────────────────────────────────────────────
pub const BUILD_VERSION: &str = concat!(
    "dev-launcher v",
    env!("CARGO_PKG_VERSION"),
    "-",
    env!("GIT_SHA")
);

// ── Warm-gradient "Enter run fix" label ───────────────────────────────────────
pub const ENTER_RUN_FIX: &str = concat!(
    "\x1b[1m",         // bold on
    "\x1b[38;5;202mE", // orange-red
    "\x1b[38;5;208mn", // orange
    "\x1b[38;5;214mt", // amber
    "\x1b[38;5;220me", // gold
    "\x1b[38;5;226mr", // bright yellow
    " ",
    "\x1b[38;5;220mr", // gold (descend)
    "\x1b[38;5;214mu", // amber
    "\x1b[38;5;208mn", // orange
    " ",
    "\x1b[38;5;214mf", // amber (rise again)
    "\x1b[38;5;220mi", // gold
    "\x1b[38;5;226mx", // bright yellow
    "\x1b[0m",         // reset all
);

// ── TUI guard ────────────────────────────────────────────────────────────────

/// RAII guard: enters raw mode + alternate screen on creation, restores on drop.
pub struct TuiGuard;

impl TuiGuard {
    pub fn enter() -> Option<Self> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
            return None;
        }
        if enable_raw_mode().is_err() {
            return None;
        }
        let mut stdout = io::stdout();
        if execute!(stdout, EnterAlternateScreen, cursor::Hide).is_err() {
            let _ = disable_raw_mode();
            return None;
        }
        Some(Self)
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
        let _ = stdout.flush();
        let _ = disable_raw_mode();
    }
}

// ── Mode ─────────────────────────────────────────────────────────────────────

pub enum Mode {
    Overview {
        cursor: usize,
    },
    LogView {
        svc_idx: usize,
        scroll: usize,
        follow: bool,
    },
    Diagnose {
        svc_idx: usize,
        findings: Vec<crate::diagnosis::Finding>,
        cursor: usize,
    },
    Credentials,
}

// ── Input events ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum InputEvent {
    Up,
    Down,
    Enter,
    Back,
    PageUp,
    PageDown,
    Follow,
    Credentials,
    Diagnose,
    Report,
    Restart,
    Stop,
    FullRestart,
    RotateLog,
    TogglePaths,
    /// Leave the TUI and return to the workspace selector without stopping the stack.
    Detach,
    OpenInCode,
}

/// Translate a crossterm `KeyEvent` into our `InputEvent` vocabulary.
pub fn map_key_event(ke: KeyEvent) -> Option<InputEvent> {
    match ke.code {
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc | KeyCode::Left => {
            Some(InputEvent::Back)
        }
        KeyCode::Up | KeyCode::Char('k') => Some(InputEvent::Up),
        KeyCode::Down | KeyCode::Char('j') => Some(InputEvent::Down),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Some(InputEvent::Enter),
        KeyCode::PageUp => Some(InputEvent::PageUp),
        KeyCode::PageDown => Some(InputEvent::PageDown),
        KeyCode::Char('f') => Some(InputEvent::Follow),
        KeyCode::Char('e') => Some(InputEvent::Credentials),
        KeyCode::Char('d') => Some(InputEvent::Diagnose),
        KeyCode::Char('p') | KeyCode::Char('P') => Some(InputEvent::TogglePaths),
        KeyCode::Char('r') if ke.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(InputEvent::FullRestart)
        }
        KeyCode::Char('r') if !ke.modifiers.contains(KeyModifiers::SHIFT) => {
            Some(InputEvent::Report)
        }
        KeyCode::Char('R') | KeyCode::Char('r') if ke.modifiers.contains(KeyModifiers::SHIFT) => {
            Some(InputEvent::Restart)
        }
        KeyCode::Char('s') | KeyCode::Char('S') => Some(InputEvent::Stop),
        KeyCode::Char('m') | KeyCode::Char('M') => Some(InputEvent::Detach),
        KeyCode::Char('c') if !ke.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(InputEvent::RotateLog)
        }
        KeyCode::Char('o') | KeyCode::Char('O') => Some(InputEvent::OpenInCode),
        _ => None,
    }
}

pub fn spawn_input_thread(
    tx: mpsc::SyncSender<InputEvent>,
    stopping: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) {
    thread::spawn(move || loop {
        if stopping.load(Ordering::Relaxed) {
            return;
        }
        // While paused, the main thread drives event::read() directly (inline
        // confirm prompts). If we kept polling here, we'd race with it and
        // about half the keypresses would land in the mpsc channel and get
        // silently dropped — that's the "double-tap" bug.
        if paused.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        if let Ok(true) = event::poll(Duration::from_millis(20)) {
            if paused.load(Ordering::Relaxed) {
                continue;
            }
            if let Ok(Event::Key(ke)) = event::read() {
                if let Some(e) = map_key_event(ke) {
                    let _ = tx.try_send(e);
                }
            }
        }
    });
}

/// RAII guard that pauses the input thread for the lifetime of the guard so
/// the holder can call `event::read()` directly without racing.
pub struct InputPauseGuard {
    flag: Arc<AtomicBool>,
}

impl InputPauseGuard {
    pub fn new(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Relaxed);
        // Wait long enough for the input thread to observe the flag — its
        // poll timeout is 20 ms, so 40 ms covers any in-flight read.
        thread::sleep(Duration::from_millis(40));
        drain_input_events();
        Self {
            flag: Arc::clone(flag),
        }
    }
}

impl Drop for InputPauseGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Relaxed);
    }
}

// ── Render helpers ─────────────────────────────────────────────────────────────

/// Write a list of ANSI-coded lines to the alternate screen buffer.
pub fn draw_ansi_lines(_tui: &mut TuiGuard, lines: &[String]) {
    use crossterm::cursor::MoveTo;
    use crossterm::terminal::{Clear, ClearType};
    let mut out = io::stdout();
    let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
    for line in lines {
        let _ = write!(out, "{}\r\n", line);
    }
    let _ = out.flush();
}

/// Drain all pending crossterm events.
pub fn drain_input_events() {
    while event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let _ = event::read();
    }
}

pub fn terminal_size() -> (usize, usize) {
    crossterm::terminal::size()
        .map(|(c, r)| (c as usize, r as usize))
        .unwrap_or((120, 40))
}

/// Count the number of visible (non-ANSI) characters in `s`.
pub fn ansi_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut len = 0usize;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\x1b' && b.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < b.len() && b[i] != b'm' {
                i += 1;
            }
            i += 1;
        } else {
            len += 1;
            i += 1;
        }
    }
    len
}

/// Return `s` followed by enough spaces to reach `width` visible columns.
pub fn pad_ansi(s: &str, width: usize) -> String {
    let pad = width.saturating_sub(ansi_len(s));
    format!("{s}{}", " ".repeat(pad))
}

// ── Per-process shutdown state ────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
pub enum TermStatus {
    Terminating,
    Stopped(i32),
    Killed,
}

/// Abort the current session worker and let the parent selector redraw the
/// workspace menu. Restores cooked mode so the picker renders correctly, then
/// exits with status 0 — `wait_for_session` treats that as a clean exit.
pub fn exit_to_selector_menu() -> ! {
    let _ = disable_raw_mode();
    ensure_cooked_output();
    print!("\x1b[H\x1b[2J");
    println!("  {YLW}Wizard aborted — returning to workspace menu.{R}");
    let _ = io::stdout().flush();
    std::process::exit(0);
}

/// Restore terminal to cooked mode via direct tcsetattr.
pub fn ensure_cooked_output() {
    #[cfg(unix)]
    unsafe {
        let fd = libc::STDOUT_FILENO;
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) == 0 {
            t.c_oflag |= libc::OPOST | libc::ONLCR;
            t.c_lflag |= libc::ICANON | libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ISIG;
            t.c_iflag |= libc::ICRNL;
            let _ = libc::tcsetattr(fd, libc::TCSANOW, &t);
        }
    }
}

/// Render the shutdown screen.
pub fn render_shutdown(
    slug: &str,
    pairs: &[(String, Option<usize>)],
    term_status: &[TermStatus],
    elapsed: Duration,
    timed_out: bool,
) {
    let _ = disable_raw_mode();
    ensure_cooked_output();
    print!("\x1b[H\x1b[2J\r");
    print!(
        "\r\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}  {YLW}{BOLD}shutting down…{R}\r\n\r\n"
    );

    for (name, proc_j) in pairs {
        let status = match proc_j {
            None => format!("{DIM}already stopped{R}"),
            Some(j) => match &term_status[*j] {
                TermStatus::Terminating => format!("{YLW}terminating…{R}"),
                TermStatus::Stopped(0) => format!("{GRN}stopped{R}"),
                TermStatus::Stopped(c) => format!("{GRN}stopped ({c}){R}"),
                TermStatus::Killed => format!("{RED}force killed{R}"),
            },
        };
        print!("  {:<26}{status}\r\n", name);
    }

    print!("\r\n");

    let pending = term_status
        .iter()
        .filter(|s| **s == TermStatus::Terminating)
        .count();
    if timed_out {
        print!("  {RED}Grace period exceeded — processes were force-killed.{R}\r\n");
    } else if pending == 0 {
        print!("  {GRN}{BOLD}All processes stopped.{R}\r\n");
    } else {
        print!(
            "  {DIM}Waiting for {pending} process{}…  {}s{R}\r\n",
            if pending == 1 { "" } else { "es" },
            elapsed.as_secs()
        );
    }
    print!("\r\n");
    let _ = io::stdout().flush();
}
