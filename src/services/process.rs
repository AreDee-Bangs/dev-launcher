use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ── SIGHUP coordination ───────────────────────────────────────────────────────

pub static SIGHUP_STOP: AtomicBool = AtomicBool::new(false);

pub extern "C" fn sighup_handler(_: libc::c_int) {
    SIGHUP_STOP.store(true, Ordering::Relaxed);
}

// ── PID file helpers ──────────────────────────────────────────────────────────

/// Path of the PID file for a given slug.
pub fn pid_file_path(slug: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/dev-launcher-{slug}.pids"))
}

/// Kill any PIDs recorded in a leftover PID file from a crashed previous session.
pub fn kill_orphaned_pids(slug: &str) {
    let path = pid_file_path(slug);
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    let pids: Vec<i32> = content
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    if pids.is_empty() {
        return;
    }
    eprintln!("  [dev-launcher] Found orphaned PIDs from a previous session: {pids:?}");
    eprintln!("  [dev-launcher] Sending SIGTERM…");
    for &pid in &pids {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    for &pid in &pids {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    let _ = fs::remove_file(&path);
    eprintln!("  [dev-launcher] Orphan cleanup done.");
}

/// Append a PID to the session PID file.
pub fn record_pid(slug: &str, pid: u32) {
    let path = pid_file_path(slug);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{pid}");
    }
}

// ── Process management ────────────────────────────────────────────────────────

/// Managed child process (owned by main thread).
pub struct Proc {
    pub idx: usize,
    pub pgid: i32,
    pub child: Child,
}

impl Proc {
    pub fn kill(&mut self) {
        unsafe {
            libc::kill(-self.pgid, libc::SIGTERM);
        }
    }

    pub fn try_reap(&mut self) -> Option<i32> {
        self.child
            .try_wait()
            .ok()
            .flatten()
            .map(|s| s.code().unwrap_or(-1))
    }
}

// ── Log file helper ───────────────────────────────────────────────────────────

pub fn open_log(path: &Path) -> std::fs::File {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|_| panic!("cannot open log {}", path.display()))
}

// ── Service spawning ──────────────────────────────────────────────────────────

pub fn spawn_svc(
    program: &str,
    args: &[&str],
    dir: &Path,
    extra_env: &HashMap<String, String>,
    log: &Path,
) -> io::Result<(Child, i32)> {
    use std::os::unix::process::CommandExt;

    let log_out = open_log(log);
    let log_err = log_out.try_clone()?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .envs(extra_env)
        .stdin(Stdio::null())
        .stdout(log_out)
        .stderr(log_err);
    cmd.process_group(0);
    let child = cmd.spawn()?;
    let pgid = child.id() as i32;
    Ok((child, pgid))
}

// ── Health probe ──────────────────────────────────────────────────────────────

pub fn probe(url: &str) -> bool {
    match ureq::get(url).timeout(Duration::from_secs(2)).call() {
        Ok(r) => r.status() < 500,
        Err(ureq::Error::Status(code, _)) => code < 500,
        Err(_) => false,
    }
}
