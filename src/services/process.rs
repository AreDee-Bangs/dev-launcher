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

/// Path of the detach marker file for a given slug.
pub fn detached_marker_path(slug: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/dev-launcher-{slug}.detached"))
}

/// Write the detach marker so kill_orphaned_pids knows these PIDs are intentional.
pub fn mark_detached(slug: &str) {
    let _ = fs::write(detached_marker_path(slug), "");
}

/// Running status of a workspace derived from its PID file.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceRunStatus {
    NotRunning,
    Running,
    Degraded,
    Failed,
}

/// Check how many recorded PIDs are still alive and return a status.
pub fn workspace_run_status(slug: &str) -> WorkspaceRunStatus {
    let Ok(content) = fs::read_to_string(pid_file_path(slug)) else {
        return WorkspaceRunStatus::NotRunning;
    };
    let pids: Vec<i32> = content
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    if pids.is_empty() {
        return WorkspaceRunStatus::NotRunning;
    }
    let alive = pids
        .iter()
        .filter(|&&pid| unsafe { libc::kill(pid, 0) == 0 })
        .count();
    match alive {
        0 => WorkspaceRunStatus::Failed,
        n if n == pids.len() => WorkspaceRunStatus::Running,
        _ => WorkspaceRunStatus::Degraded,
    }
}

/// Kill any PIDs recorded in a leftover PID file from a crashed previous session.
/// Skips killing if a detach marker exists and processes are still alive (intentional detach).
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

    let marker = detached_marker_path(slug);
    if marker.exists() {
        let any_alive = pids.iter().any(|&pid| unsafe { libc::kill(pid, 0) == 0 });
        if any_alive {
            // Intentionally detached — leave them running.
            return;
        }
        // All dead — clean up the stale marker.
        let _ = fs::remove_file(&marker);
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

// ── Log rotation ─────────────────────────────────────────────────────────────

/// Copy current log → .log.1 (shift older rotations up to 5), then truncate.
///
/// Child processes keep their O_APPEND fd open; after truncation the kernel
/// appends from offset 0, so writes continue seamlessly without changing the fd.
pub fn rotate_log(log_path: &Path) -> io::Result<()> {
    const MAX_ROTATIONS: usize = 5;

    for n in (1..MAX_ROTATIONS).rev() {
        let src = PathBuf::from(format!("{}.{n}", log_path.display()));
        let dst = PathBuf::from(format!("{}.{}", log_path.display(), n + 1));
        if src.exists() {
            let _ = fs::rename(&src, &dst);
        }
    }
    let overflow = PathBuf::from(format!("{}.{}", log_path.display(), MAX_ROTATIONS + 1));
    let _ = fs::remove_file(&overflow);

    if log_path.exists() && log_path.metadata().map(|m| m.len()).unwrap_or(0) > 0 {
        let rotated = PathBuf::from(format!("{}.1", log_path.display()));
        fs::copy(log_path, &rotated)?;
    }

    let _ = OpenOptions::new().write(true).truncate(true).open(log_path);
    Ok(())
}

/// gzip-compress all rotated log files (*.log.N) in the session directory.
/// Leaves *.log (current active files) uncompressed.
pub fn compress_rotated_logs(logs_dir: &Path) {
    let Ok(entries) = fs::read_dir(logs_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".gz") {
            continue;
        }
        // Match *.log.<number>  e.g. copilot-worker.log.1
        let mut parts = name.rsplitn(2, '.');
        let ext = parts.next().unwrap_or("");
        let stem = parts.next().unwrap_or("");
        if ext.parse::<u32>().is_ok() && stem.contains(".log") {
            let _ = Command::new("gzip").arg("-f").arg(&path).status();
        }
    }
}

// ── Health probe ──────────────────────────────────────────────────────────────

pub fn probe(url: &str) -> bool {
    match ureq::get(url).timeout(Duration::from_secs(2)).call() {
        Ok(r) => r.status() < 500,
        Err(ureq::Error::Status(code, _)) => code < 500,
        Err(_) => false,
    }
}
