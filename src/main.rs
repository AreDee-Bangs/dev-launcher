//! dev-feature — multi-product stack launcher with process-tree management, health monitoring,
//! and an interactive TUI for diving into per-service logs.
//!
//! Spawns every service in its own process group, polls health endpoints concurrently,
//! and terminates the entire tree on Ctrl+C — no orphan processes.
//!
//! Keys (when stdin is a TTY):
//!   Overview : ↑↓ / j k  navigate   Enter / → / l  open logs   q  quit
//!   Log view : ↑↓ / j k  scroll ±5   PgUp/PgDn  scroll ±20   f  follow tail
//!              q / ← / Esc  back to overview

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};
use std::thread;

use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use tui_textarea::TextArea;

// ── Signal / orphan-recovery globals ─────────────────────────────────────────
// SIGHUP is sent when the terminal window is closed. The ctrlc crate only
// handles SIGINT/SIGTERM. We install a raw libc handler for SIGHUP that sets
// this static so the main loop runs the same clean shutdown path.
static SIGHUP_STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn sighup_handler(_: libc::c_int) {
    SIGHUP_STOP.store(true, Ordering::Relaxed);
}

/// Path of the PID file for a given slug. Written at spawn time so that a
/// subsequent launch can kill any orphans left behind by a SIGKILL'd session.
fn pid_file_path(slug: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/dev-feature-{slug}.pids"))
}

/// Kill any PIDs recorded in a leftover PID file from a crashed previous session.
fn kill_orphaned_pids(slug: &str) {
    let path = pid_file_path(slug);
    let Ok(content) = fs::read_to_string(&path) else { return };
    let pids: Vec<i32> = content.lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    if pids.is_empty() { return }
    eprintln!("  [dev-feature] Found orphaned PIDs from a previous session: {pids:?}");
    eprintln!("  [dev-feature] Sending SIGTERM…");
    for &pid in &pids {
        unsafe { libc::kill(pid, libc::SIGTERM); }
    }
    // Brief pause then SIGKILL any survivors.
    thread::sleep(Duration::from_millis(500));
    for &pid in &pids {
        unsafe { libc::kill(pid, libc::SIGKILL); }
    }
    let _ = fs::remove_file(&path);
    eprintln!("  [dev-feature] Orphan cleanup done.");
}

/// Append a PID to the session PID file (called once per spawned process).
fn record_pid(slug: &str, pid: u32) {
    let path = pid_file_path(slug);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{pid}");
    }
}

// ── Terminal output helpers ───────────────────────────────────────────────────

/// Re-enable `OPOST | ONLCR` on stdout so bare `\n` is converted to `\r\n` by
/// the terminal driver.  crossterm's `disable_raw_mode()` restores the saved
/// attributes, but if those were captured while `ONLCR` was already off (e.g.
/// a prior raw-mode session or a non-standard shell), the staircase reappears.
/// Calling this after any TuiGuard drop guarantees correct `println!` behaviour
/// without having to change every print site to use explicit `\r\n`.
/// Restore the terminal to a fully-cooked state via direct tcsetattr.
///
/// crossterm's disable_raw_mode() restores from its own saved snapshot, but if
/// that snapshot was captured while input flags were already partially stripped
/// (from a previous raw session), the restore leaves ICANON/ECHO/ICRNL off.
/// Calling this after disable_raw_mode() ensures all cooked-mode flags are on,
/// so the next enable_raw_mode() saves a clean baseline to restore to.
fn ensure_cooked_output() {
    #[cfg(unix)]
    unsafe {
        let fd = libc::STDOUT_FILENO;
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) == 0 {
            // Output: translate \n → \r\n
            t.c_oflag |= libc::OPOST | libc::ONLCR;
            // Input: canonical line buffering, echo, CR→LF translation
            t.c_lflag |= libc::ICANON | libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ISIG;
            t.c_iflag |= libc::ICRNL;
            let _ = libc::tcsetattr(fd, libc::TCSANOW, &t);
        }
    }
}

// ── ANSI ──────────────────────────────────────────────────────────────────────

const R:    &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM:  &str = "\x1b[2m";
const GRN:  &str = "\x1b[32m";
const YLW:  &str = "\x1b[33m";
const RED:  &str = "\x1b[31m";
const CYN:  &str = "\x1b[36m";

// ── Build version ─────────────────────────────────────────────────────────────
const BUILD_VERSION: &str = concat!("dev-feature.", env!("BUILD_TIMESTAMP"));

// ── Warm-gradient "Enter run fix" label ───────────────────────────────────────
// Uses xterm-256 palette (202→208→214→220→226) so it works on any terminal that
// supports 256 colours — no 24-bit requirement.  Bold + orange→yellow arc makes
// the actionable hint visually pop out of the surrounding dim status bar.
const ENTER_RUN_FIX: &str = concat!(
    "\x1b[1m",                // bold on
    "\x1b[38;5;202mE",        // orange-red
    "\x1b[38;5;208mn",        // orange
    "\x1b[38;5;214mt",        // amber
    "\x1b[38;5;220me",        // gold
    "\x1b[38;5;226mr",        // bright yellow  ← peak
    " ",
    "\x1b[38;5;220mr",        // gold (descend)
    "\x1b[38;5;214mu",        // amber
    "\x1b[38;5;208mn",        // orange
    " ",
    "\x1b[38;5;214mf",        // amber (rise again)
    "\x1b[38;5;220mi",        // gold
    "\x1b[38;5;226mx",        // bright yellow  ← peak
    "\x1b[0m",                // reset all
);

// ── Finding kinds ─────────────────────────────────────────────────────────────
// Each finding has a stable kind string so the recipe catalog (RECIPE_CATALOG)
// can declare whether a fix is implemented.  Kinds starting with "info/" are
// purely informational and never trigger the "report missing recipe" prompt.

const KIND_INFO:              &str = "info/generic";  // baseline kind; specific variants below
const KIND_INFO_LOG_TAIL:     &str = "info/log-tail";
const KIND_INFO_LOG_PATTERNS: &str = "info/log-patterns";
const KIND_INFO_NO_ISSUES:    &str = "info/no-issues";
const KIND_INFO_BOOTSTRAP_CHECK: &str = "info/bootstrap-check";

const KIND_PYTHON_VENV:       &str = "python-venv-missing";
const KIND_NODE_MODULES:      &str = "node-modules-missing";
const KIND_ENV_PLACEHOLDER:   &str = "env-placeholder-credentials";
const KIND_BOOTSTRAP_RUN:     &str = "bootstrap-command-needed";
const KIND_DEGRADED_UNKNOWN:  &str = "service-degraded-unknown";
const KIND_CRASH:             &str = "service-crashed";
const KIND_OPENCTI_ES_PARTIAL_INIT: &str = "opencti-es-partial-init";
const KIND_CONNECTOR_TYPE_MISSING:  &str = "connector-type-missing";
const KIND_CONNECTOR_LICENCE_MISSING: &str = "connector-licence-missing";
const KIND_MINIO_DOWN:              &str = "docker-service-down/minio";

/// Kinds that have a known, implemented recipe in this binary.
/// A finding whose kind is NOT in this list (and is not an info/ kind) will
/// offer the user a shortcut to file a GitHub issue requesting the recipe.
const RECIPE_CATALOG: &[&str] = &[
    KIND_PYTHON_VENV,
    KIND_NODE_MODULES,
    KIND_ENV_PLACEHOLDER,
    KIND_BOOTSTRAP_RUN,
    KIND_OPENCTI_ES_PARTIAL_INIT,
    KIND_CONNECTOR_TYPE_MISSING,
    KIND_CONNECTOR_LICENCE_MISSING,
];

/// Returns true when the finding represents an issue with no implemented recipe,
/// meaning the user should be prompted to report it.
fn needs_recipe(f: &Finding) -> bool {
    if f.kind.starts_with("info/") { return false; }
    !RECIPE_CATALOG.contains(&f.kind)
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name    = "dev-feature",
    about   = "Launch the full multi-product dev stack for a feature branch.\n\
               Each service runs in its own process group; Ctrl+C kills the entire tree.",
    version
)]
struct Args {
    // ── Workspace shortcuts ───────────────────────────────────────────────────

    /// Open an existing workspace by its 8-char hash ID (shown in the workspace list).
    #[arg(long)] workspace: Option<String>,

    /// Branch for Filigran Copilot — creates or finds a workspace matching all supplied branches.
    #[arg(long)] copilot_branch: Option<String>,

    /// Branch for OpenCTI.
    #[arg(long)] opencti_branch: Option<String>,

    /// Branch for OpenAEV.
    #[arg(long)] openaev_branch: Option<String>,

    /// Branch for the ImportDoc connector.
    #[arg(long)] connector_branch: Option<String>,

    // ── Per-product worktree path overrides (runtime-only, not saved) ───────────

    /// Use an existing worktree directory directly for Filigran Copilot.
    #[arg(long)] copilot_worktree: Option<PathBuf>,

    /// Use an existing worktree directory directly for OpenCTI.
    #[arg(long)] opencti_worktree: Option<PathBuf>,

    /// Use an existing worktree directory directly for OpenAEV.
    #[arg(long)] openaev_worktree: Option<PathBuf>,

    /// Use an existing worktree directory directly for the ImportDoc connector.
    #[arg(long)] connector_worktree: Option<PathBuf>,

    // ── Per-product commit pinning (saved to workspace, creates detached worktree) ─

    /// Launch Filigran Copilot at a specific commit (creates a detached worktree).
    #[arg(long)] copilot_commit: Option<String>,

    /// Launch OpenCTI at a specific commit (creates a detached worktree).
    #[arg(long)] opencti_commit: Option<String>,

    /// Launch OpenAEV at a specific commit (creates a detached worktree).
    #[arg(long)] openaev_commit: Option<String>,

    /// Launch the ImportDoc connector at a specific commit (creates a detached worktree).
    #[arg(long)] connector_commit: Option<String>,

    // ── Runtime-only overrides (not saved to workspace) ───────────────────────

    /// Skip the OpenCTI React frontend only.
    #[arg(long)] no_opencti_front: bool,

    /// Skip the OpenAEV React frontend only.
    #[arg(long)] no_openaev_front: bool,

    /// Override the log directory (default: /tmp/dev-feature-logs/{workspace-hash}).
    #[arg(long)] logs_dir: Option<PathBuf>,

    // ── Root configuration ────────────────────────────────────────────────────

    /// Path to the filigran workspace root (overrides env var and config file).
    /// Same effect as setting FILIGRAN_WORKSPACE_ROOT.
    #[arg(long)] workspace_root: Option<PathBuf>,
}

// ── Health state ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Health {
    Pending,
    Launching,
    Probing(u32),
    Up,
    Running,
    Degraded(String),
    Crashed(i32),
}

impl Health {
    fn label(&self) -> String {
        match self {
            Health::Pending       => format!("{DIM}pending{R}"),
            Health::Launching     => format!("{YLW}launching{R}"),
            Health::Probing(n)    => format!("{YLW}health probe #{n}{R}"),
            Health::Up            => format!("{GRN}up{R}"),
            Health::Running       => format!("{CYN}running{R}"),
            Health::Degraded(msg) => format!("{RED}degraded ({msg}){R}"),
            Health::Crashed(code) => format!("{RED}crashed ({code}){R}"),
        }
    }

    /// Plain-text label used when ANSI codes would break column alignment.
    fn label_plain(&self) -> String {
        match self {
            Health::Pending       => "pending".into(),
            Health::Launching     => "launching".into(),
            Health::Probing(n)    => format!("health probe #{n}"),
            Health::Up            => "up".into(),
            Health::Running       => "running".into(),
            Health::Degraded(msg) => format!("degraded ({msg})"),
            Health::Crashed(code) => format!("crashed ({code})"),
        }
    }

    fn is_done(&self) -> bool {
        matches!(self, Health::Up | Health::Running | Health::Degraded(_) | Health::Crashed(_))
    }
}

// ── Log diagnosis patterns ────────────────────────────────────────────────────

/// Known failure signatures. Each entry is `(needle_lowercase, human_reason)`.
/// Patterns are checked against log lines converted to lowercase.
const DIAG_PATTERNS: &[(&str, &str)] = &[
    ("econnrefused",                             "Connection refused — is the required service running?"),
    ("amqp: connection refused",                 "RabbitMQ is not reachable — check Docker container"),
    ("connection refused to localhost:5672",      "RabbitMQ port 5672 not reachable"),
    ("connection refused to localhost:5432",      "PostgreSQL not reachable — check Docker container"),
    ("could not connect to server: connection refused", "PostgreSQL is not reachable"),
    ("redis: could not connect",                 "Redis is not reachable — check Docker container"),
    ("error connecting to redis",                "Redis connection failed"),
    ("connection refused to localhost:6379",      "Redis port 6379 not reachable"),
    ("elasticsearch: no living connections",      "Elasticsearch cluster is unreachable"),
    ("connection refused to localhost:9200",      "Elasticsearch port 9200 not reachable"),
    ("minio: connection refused",                "MinIO/S3 is not reachable"),
    ("connection refused to localhost:9000",      "MinIO port 9000 not reachable"),
    ("address already in use",                   "Port conflict — another process is using this port"),
    ("eaddrinuse",                               "Port already in use — stop the conflicting process"),
    ("no module named",                          "Python module missing — run pip install or recreate venv"),
    ("modulenotfounderror",                      "Python module not found — check venv"),
    ("cannot find module",                       "Node.js module missing — run yarn install"),
    ("error: cannot find module",                "Node.js module missing — run yarn install"),
    ("changeme",                                 "Placeholder credentials detected — edit .env.dev"),
    ("invalid pem",                              "Invalid PEM certificate — check CONNECTOR_LICENCE_KEY_PEM"),
    ("certificate verify failed",                "TLS certificate verification failed"),
    ("permission denied",                        "Permission denied — check file/directory ownership"),
    ("killed",                                   "Process killed — possibly out of memory (OOM)"),
    ("out of memory",                            "Out of memory — free RAM or increase system swap"),
    ("no space left on device",                  "Disk full — free up space before restarting"),
];

// ── LLM provider ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum LlmProvider {
    /// Anthropic Messages API format (`/v1/messages`, `x-api-key` header).
    Anthropic,
    /// OpenAI-compatible Chat Completions format (`/chat/completions`, Bearer auth).
    /// Works with OpenAI, Ollama, LiteLLM, Azure OpenAI, Mistral, etc.
    OpenAICompatible,
}

#[derive(Clone, Debug)]
struct LlmConfig {
    provider: LlmProvider,
    /// API key — sent as `x-api-key` (Anthropic) or `Authorization: Bearer` (OpenAI-compat).
    api_key:  String,
    model:    String,
    /// Base URL for the provider, without trailing slash.
    /// Anthropic default : `https://api.anthropic.com/v1`
    /// OpenAI default    : `https://api.openai.com/v1`
    /// Custom example    : `http://localhost:4000/v1` (LiteLLM / Ollama)
    base_url: String,
}

// ── Diagnosis event (diag thread → main loop) ─────────────────────────────────

enum DiagEvent {
    Result { svc_idx: usize, msg: String },
}

// ── Service display state (shared with health thread) ─────────────────────────

#[derive(Debug)]
/// Stored command needed to restart a service after a crash or manual retry.
#[derive(Clone)]
struct SpawnCmd {
    prog:            String,
    args:            Vec<String>,
    dir:             PathBuf,
    env:             HashMap<String, String>,
    requires_docker: bool,
}

struct Svc {
    name:            String,
    url:             Option<String>,
    health_path:     String,
    health:          Health,
    pid:             Option<u32>,
    started_at:      Option<Instant>,
    startup_timeout: Duration,
    log_path:        PathBuf,
    /// Diagnosis message set by the log daemon after a crash.
    diagnosis:       Option<String>,
    /// Stored command — populated at spawn time; enables R-key restart.
    spawn_cmd:       Option<SpawnCmd>,
    /// Service names that must be Up/Running before this one is spawned.
    requires:        Vec<String>,
}

impl Svc {
    fn new(
        name: impl Into<String>,
        url: Option<impl Into<String>>,
        health_path: impl Into<String>,
        timeout_secs: u64,
        log_path: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            url: url.map(Into::into),
            health_path: health_path.into(),
            health: Health::Pending,
            pid: None,
            started_at: None,
            startup_timeout: Duration::from_secs(timeout_secs),
            log_path,
            diagnosis: None,
            spawn_cmd: None,
            requires: Vec::new(),
        }
    }

    fn health_url(&self) -> Option<String> {
        self.url.as_deref().map(|b| format!("{b}{}", self.health_path))
    }

    fn secs(&self) -> u64 {
        self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0)
    }

    fn is_healthy(&self) -> bool {
        matches!(self.health, Health::Up | Health::Running)
    }

    fn is_waiting_for_requires(&self) -> bool {
        matches!(&self.health, Health::Degraded(m) if m.starts_with("Waiting for "))
            && self.spawn_cmd.is_some()
    }
}

type State = Arc<Mutex<Vec<Svc>>>;

// ── Repo manifest ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct ManifestDocker {
    compose_dev: Option<String>,
    project:     Option<String>,
}

struct SvcDef {
    name:            String,
    args:            Vec<String>,
    cwd:             String,
    health:          Option<String>,
    timeout_secs:    u64,
    requires_docker: bool,
    log_name:        Option<String>,
    /// Service names (within this stack) that must be Up before this one starts.
    requires:        Vec<String>,
}

enum BootstrapDef {
    Check { path: String, missing_hint: String },
    RunIfMissing { check: String, command: Vec<String>, cwd: Option<String> },
}

#[derive(Default)]
struct RepoManifest {
    docker:    ManifestDocker,
    services:  Vec<SvcDef>,
    bootstrap: Vec<BootstrapDef>,
}

// ── Managed children (owned by main thread) ───────────────────────────────────

struct Proc {
    idx:   usize,
    pgid:  i32,
    child: Child,
}

impl Proc {
    fn kill(&mut self) {
        unsafe { libc::kill(-self.pgid, libc::SIGTERM); }
    }

    fn try_reap(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().map(|s| s.code().unwrap_or(-1))
    }
}

// ── Paths ─────────────────────────────────────────────────────────────────────

struct Paths {
    copilot:   PathBuf,
    opencti:   PathBuf,
    connector: PathBuf,
    openaev:   PathBuf,
}

impl Paths {
}

// ── Git / worktree helpers ────────────────────────────────────────────────────


/// Return the currently checked-out branch name for `dir`, or empty string.
fn current_branch(dir: &Path) -> String {
    if !dir.is_dir() { return String::new(); }
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD")
        .unwrap_or_default()
}

/// Return the short (7-char) commit hash for `dir`, or empty string.
fn current_commit_short(dir: &Path) -> String {
    if !dir.is_dir() { return String::new(); }
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Sentinel prefix used to store commit-pinned branches in workspace configs.
const COMMIT_PREFIX: &str = "commit:";

/// If `branch` is a commit-pinned ref (stored as `"commit:<hash>"`), return the hash part.
fn parse_commit_ref(branch: &str) -> Option<&str> {
    branch.strip_prefix(COMMIT_PREFIX)
}

/// Convert a branch name to a filesystem-safe slug (e.g. `issue/123-foo` → `issue-123-foo`).
/// Commit refs (`commit:<hash>`) become `commit-<hash>`.
fn branch_to_slug(branch: &str) -> String {
    if let Some(hash) = parse_commit_ref(branch) {
        return format!("commit-{hash}");
    }
    branch.replace('/', "-")
}

/// Ensure a git worktree exists for `branch` (or a commit ref `commit:<hash>`).
/// Commit refs delegate to `ensure_worktree_at_commit`; branch names use the
/// standard `git worktree add` flow.
fn ensure_worktree(workspace: &Path, repo: &str, branch: &str) -> PathBuf {
    if let Some(commit) = parse_commit_ref(branch) {
        return ensure_worktree_at_commit(workspace, repo, commit);
    }
    ensure_worktree_branch(workspace, repo, branch)
}

/// Create (if missing) a detached worktree for a specific commit at
/// `{workspace}/{repo}-commit-{hash}`.
fn ensure_worktree_at_commit(workspace: &Path, repo: &str, commit: &str) -> PathBuf {
    let target = workspace.join(format!("{}-commit-{}", repo, commit));
    if target.is_dir() { return target; }

    let main_repo = workspace.join(repo);
    if !main_repo.is_dir() {
        println!("  {YLW}⚠{R}  {repo} main repo not found — cannot create worktree");
        return target;
    }

    println!("  Creating detached worktree {repo} @ {commit}…");
    let ok = Command::new("git")
        .args(["worktree", "add", "--detach", target.to_str().unwrap_or(""), commit])
        .current_dir(&main_repo)
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !ok {
        // Commit may not be present locally — fetch and retry.
        println!("  Fetching origin…");
        let _ = Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(&main_repo)
            .stdin(Stdio::null())
            .status();
        let ok2 = Command::new("git")
            .args(["worktree", "add", "--detach", target.to_str().unwrap_or(""), commit])
            .current_dir(&main_repo)
            .stdin(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok2 {
            println!("  {GRN}✓{R}  Worktree created: {}", target.display());
        } else {
            println!("  {RED}✗{R}  Could not create worktree for {repo} @ {commit}");
        }
    } else {
        println!("  {GRN}✓{R}  Worktree created: {}", target.display());
    }
    target
}

fn ensure_worktree_branch(workspace: &Path, repo: &str, branch: &str) -> PathBuf {
    let slug   = branch_to_slug(branch);
    let target = workspace.join(format!("{}-{}", repo, slug));
    if target.is_dir() { return target; }

    let main_repo = workspace.join(repo);
    if !main_repo.is_dir() {
        println!("  {YLW}⚠{R}  {repo} main repo not found — cannot create worktree");
        return target;
    }

    // If the main checkout is already on this branch a worktree would fail
    // ("already used by worktree").  Use the main repo directly instead.
    if current_branch(&main_repo) == branch {
        println!("  {GRN}✓{R}  {repo} already on {branch} — using main checkout");
        return main_repo.clone();
    }

    println!("  Creating worktree {repo} @ {branch}…");
    let ok = Command::new("git")
        .args(["worktree", "add", target.to_str().unwrap_or(""), branch])
        .current_dir(&main_repo)
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !ok {
        // Branch may not exist locally — fetch and retry with tracking.
        println!("  Fetching origin/{branch}…");
        let _ = Command::new("git")
            .args(["fetch", "origin", branch])
            .current_dir(&main_repo)
            .stdin(Stdio::null())
            .status();
        let ok2 = Command::new("git")
            .args(["worktree", "add", "--track", "-b", &slug,
                   target.to_str().unwrap_or(""), &format!("origin/{branch}")])
            .current_dir(&main_repo)
            .stdin(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok2 {
            println!("  {RED}✗{R}  Could not create worktree for {repo} @ {branch}");
        } else {
            println!("  {GRN}✓{R}  Worktree created: {}", target.display());
        }
    } else {
        println!("  {GRN}✓{R}  Worktree created: {}", target.display());
    }
    target
}

/// Returns a list of human-readable issues for a worktree directory:
/// uncommitted changes and/or unpushed commits.  Empty vec = clean.
fn worktree_dirty_reasons(dir: &Path) -> Vec<String> {
    let mut reasons = Vec::new();
    if !dir.is_dir() { return reasons; }

    // Uncommitted changes (staged or unstaged)
    if let Ok(out) = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .stderr(Stdio::null())
        .output()
    {
        let count = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        if count > 0 {
            reasons.push(format!("{count} uncommitted file(s)"));
        }
    }

    // Unpushed commits (silently skip if no upstream is configured)
    if let Ok(out) = Command::new("git")
        .args(["log", "@{u}..HEAD", "--oneline"])
        .current_dir(dir)
        .stderr(Stdio::null())
        .output()
    {
        if out.status.success() {
            let count = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count();
            if count > 0 {
                reasons.push(format!("{count} unpushed commit(s)"));
            }
        }
    }

    reasons
}

// ── Workspace ─────────────────────────────────────────────────────────────────

/// Fixed product registry — (repo dir, display label, short key, service desc).
/// Order is the canonical order shown throughout the UI.
const PRODUCTS: &[(&str, &str, &str, &str)] = &[
    ("filigran-copilot", "Filigran Copilot", "copilot",   "backend · worker · frontend"),
    ("opencti",          "OpenCTI",          "opencti",   "graphql · frontend"),
    ("openaev",          "OpenAEV",           "openaev",   "backend · frontend"),
    ("connectors",       "ImportDoc connector","connector","import-document-ai"),
];

#[derive(Clone, Debug)]
struct WorkspaceEntry {
    repo:    String,   // "filigran-copilot", "opencti", …
    enabled: bool,
    branch:  String,
}

#[derive(Clone, Debug)]
struct WorkspaceConfig {
    hash:     String,
    created:  String,             // "YYYY-MM-DD"
    entries:  Vec<WorkspaceEntry>, // same order as PRODUCTS
}

impl WorkspaceConfig {
    /// One-line human-readable summary of enabled products + branches.
    fn summary(&self) -> String {
        let parts: Vec<String> = self.entries.iter()
            .zip(PRODUCTS.iter())
            .filter(|(e, _)| e.enabled && !e.branch.is_empty())
            .map(|(e, (_, label, _, _))| {
                let short = label.split_whitespace().last().unwrap_or(label);
                let branch_display = if let Some(hash) = parse_commit_ref(&e.branch) {
                    format!("@{hash}")
                } else {
                    e.branch.clone()
                };
                format!("{}:{}", short, branch_display)
            })
            .collect();
        if parts.is_empty() { "(empty)".to_string() } else { parts.join("  ") }
    }
}

/// Return `{workspace_root}/.dev-workspaces`.
fn workspaces_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".dev-workspaces")
}

/// Current date as "YYYY-MM-DD" using the system clock.
fn today() -> String {
    // Use `date` command — avoids pulling in a time crate.
    Command::new("date").arg("+%Y-%m-%d").output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// FNV-1a 32-bit hash of the sorted enabled `repo=branch` pairs → 8 hex chars.
fn compute_workspace_hash(entries: &[WorkspaceEntry]) -> String {
    let mut pairs: Vec<String> = entries.iter()
        .filter(|e| e.enabled && !e.branch.is_empty())
        .map(|e| format!("{}={}", e.repo, e.branch))
        .collect();
    pairs.sort();
    let input = pairs.join("\n");
    let mut h: u32 = 2_166_136_261;
    for b in input.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16_777_619);
    }
    format!("{:08x}", h)
}

fn save_workspace(dir: &Path, config: &WorkspaceConfig) {
    let wdir = dir.join(&config.hash);
    let _ = fs::create_dir_all(&wdir);
    let path = wdir.join("workspace.conf");
    let mut out = format!("hash={}\ncreated={}\n", config.hash, config.created);
    for (e, (_, _, key, _)) in config.entries.iter().zip(PRODUCTS.iter()) {
        out.push_str(&format!("{}_enabled={}\n{}_branch={}\n", key, e.enabled, key, e.branch));
    }
    let _ = fs::write(&path, out);
}

fn load_workspace(dir: &Path, hash: &str) -> Option<WorkspaceConfig> {
    let path = dir.join(hash).join("workspace.conf");
    if !path.exists() { return None; }
    let map = parse_env_file(&path);
    // Skip tombstoned workspaces — they remain on disk for history but are invisible.
    if map.contains_key("deleted") { return None; }
    let entries = PRODUCTS.iter().map(|(repo, _, key, _)| {
        WorkspaceEntry {
            repo:    repo.to_string(),
            enabled: map.get(&format!("{key}_enabled")).map_or(false, |v| v == "true"),
            branch:  map.get(&format!("{key}_branch")).cloned().unwrap_or_default(),
        }
    }).collect();
    Some(WorkspaceConfig {
        hash:    hash.to_string(),
        created: map.get("created").cloned().unwrap_or_default(),
        entries,
    })
}

fn list_workspaces(dir: &Path) -> Vec<WorkspaceConfig> {
    if !dir.is_dir() { return vec![]; }
    let mut configs: Vec<WorkspaceConfig> = fs::read_dir(dir).into_iter()
        .flatten().flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let hash = e.file_name().to_string_lossy().to_string();
            load_workspace(dir, &hash)
        })
        .collect();
    configs.sort_by(|a, b| b.created.cmp(&a.created));
    configs
}

/// Append a `deleted=<date>` line to `workspace.conf`, keeping the directory for history.
/// Subsequent calls to `load_workspace` will return `None` for tombstoned entries.
fn tombstone_workspace(dir: &Path, hash: &str) {
    use io::Write as _;
    let path = dir.join(hash).join("workspace.conf");
    if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&path) {
        let _ = writeln!(f, "deleted={}", today());
    }
}



/// Convert a `WorkspaceConfig` back to `ProductChoice` list (for the UI and path resolution).
///
/// Always iterates `PRODUCTS` as the canonical list so that every product is
/// present in the result regardless of what was (or wasn't) stored in the config.
/// Saved state (enabled / branch) is looked up by repo name, not position, making
/// this robust against entries being missing, reordered, or added to PRODUCTS later.
fn workspace_to_choices(config: &WorkspaceConfig, workspace_root: &Path) -> Vec<ProductChoice> {
    PRODUCTS.iter().map(|(repo, label, _, desc)| {
        let saved  = config.entries.iter().find(|e| e.repo.as_str() == *repo);
        let branch = saved.map(|e| e.branch.clone()).unwrap_or_default();
        let enabled = saved.map(|e| e.enabled).unwrap_or(false);
        let path = if branch.is_empty() {
            workspace_root.join(repo)
        } else {
            let slug = branch_to_slug(&branch);
            let wt   = workspace_root.join(format!("{}-{}", repo, slug));
            if wt.is_dir() { wt } else { workspace_root.join(repo) }
        };
        ProductChoice {
            label, desc, repo,
            enabled,
            available: path.is_dir() || workspace_root.join(repo).is_dir(),
            branch,
        }
    }).collect()
}

/// Convert `ProductChoice` list to a `WorkspaceConfig` (save after selection).
fn choices_to_workspace(choices: &[ProductChoice]) -> WorkspaceConfig {
    let entries: Vec<WorkspaceEntry> = choices.iter().map(|c| WorkspaceEntry {
        repo:    c.repo.to_string(),
        enabled: c.enabled,
        branch:  c.branch.clone(),
    }).collect();
    let hash = compute_workspace_hash(&entries);
    WorkspaceConfig { hash, created: today(), entries }
}

// ── Workspace selector TUI ────────────────────────────────────────────────────

enum WorkspaceAction {
    Open(WorkspaceConfig),
    Delete(WorkspaceConfig),
    CreateNew,
    Quit,
}

/// What the user chose to do from the product selector.
enum LaunchMode {
    /// Start normally — reuse existing Docker containers/volumes.
    Normal,
    /// Wipe Docker containers + volumes for this workspace before starting.
    Clean,
    Quit,
}

fn build_workspace_selector_lines(workspaces: &[WorkspaceConfig], cursor: usize) -> Vec<String> {
    let sep = "─".repeat(72);
    let mut out = Vec::new();
    out.push(format!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}\n"));
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!("  {BOLD}Workspaces{R}  {DIM}— select one to start or create a new one{R}"));
    out.push(format!("  {DIM}{sep}{R}\n"));

    let total = workspaces.len() + 1; // +1 for "Create new"
    for (i, ws) in workspaces.iter().enumerate() {
        let marker = if i == cursor { format!("{CYN}{BOLD}▶{R} ") } else { "  ".to_string() };
        let hash   = format!("{DIM}[{}]{R}", ws.hash);
        let summary = ws.summary();
        let date   = format!("{DIM}{}{R}", ws.created);
        let summary_display = if summary.len() > 52 {
            format!("{}…", &summary[..51])
        } else {
            summary.clone()
        };
        out.push(format!("  {marker}{hash}  {:<54}{date}", summary_display));
    }

    // "Create new" entry
    let new_idx = workspaces.len();
    let marker = if new_idx == cursor { format!("{CYN}{BOLD}▶{R} ") } else { "  ".to_string() };
    out.push(format!("  {marker}{GRN}[+] Create new workspace{R}"));

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    if cursor < total - 1 {
        out.push(format!("  {DIM}↑↓ navigate   Enter open   d delete   q quit{R}"));
    } else {
        out.push(format!("  {DIM}↑↓ navigate   Enter create   q quit{R}"));
    }
    out.push(String::new());
    out
}

fn run_workspace_selector(workspaces: &[WorkspaceConfig]) -> WorkspaceAction {
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
        if event::poll(Duration::from_millis(20)).unwrap_or(false) {
            let Ok(Event::Key(ke)) = event::read() else { continue; };
            // Ignore release / repeat events — only act on the initial key press.
            if ke.kind != crossterm::event::KeyEventKind::Press { continue; }
            match ke.code {
                KeyCode::Up   | KeyCode::Char('k') => { cursor = cursor.saturating_sub(1); }
                KeyCode::Down | KeyCode::Char('j') => { if cursor + 1 < total { cursor += 1; } }
                KeyCode::Enter => {
                    drain_input_events();
                    drop(raw.take());
                    if cursor == workspaces.len() {
                        return WorkspaceAction::CreateNew;
                    } else {
                        return WorkspaceAction::Open(workspaces[cursor].clone());
                    }
                }
                KeyCode::Char('d') | KeyCode::Char('D') => {
                    if cursor < workspaces.len() {
                        drain_input_events();
                        drop(raw.take());
                        return WorkspaceAction::Delete(workspaces[cursor].clone());
                    }
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

/// Full workspace removal flow:
///   1. Check each enabled worktree for uncommitted/unpushed work.
///   2. If dirty: show per-repo details and ask for confirmation TWICE.
///      If clean: ask once.
///   3. On confirmed: docker compose down → remove worktrees → tombstone config.
///
/// Wipe all Docker containers **and** volumes for every enabled product in this
/// workspace before starting the stack.  Called when the user presses `c` in
/// the product selector ("clean start").
///
/// Uses the same workspace-scoped project names as `run_workspace_delete` so it
/// correctly targets only THIS workspace's containers/volumes.
fn clean_docker_for_workspace(slug: &str, paths: &Paths,
    no_copilot: bool, no_opencti: bool, no_openaev: bool)
{
    let sep = "─".repeat(72);
    println!("  {DIM}{sep}{R}");
    println!("  {BOLD}Clean start  {DIM}—  wiping Docker containers + volumes for {slug}{R}");
    println!("  {DIM}{sep}{R}\n");

    // (repo-dir-name, path, skip?)
    let products: &[(&str, &Path, bool)] = &[
        ("filigran-copilot", paths.copilot.as_path(),  no_copilot),
        ("opencti",          paths.opencti.as_path(),  no_opencti),
        ("openaev",          paths.openaev.as_path(),  no_openaev),
    ];

    for &(repo, dir, skip) in products {
        println!("  {BOLD}{repo}{R}");

        if skip {
            println!("    {DIM}skipped (product disabled){R}");
            continue;
        }
        if !dir.is_dir() {
            println!("    {DIM}skipped (directory not found: {}){R}", dir.display());
            continue;
        }
        println!("    {DIM}dir: {}{R}", dir.display());

        let resolved = resolve_product_docker_for_down(repo, dir, slug);
        if resolved.is_none() {
            println!("    {DIM}skipped (resolve_product_docker_for_down returned None){R}");
            continue;
        }
        let (ws_proj, base_proj, compose_file) = resolved.unwrap();
        println!("    {DIM}ws project   : {ws_proj}{R}");
        println!("    {DIM}base project : {base_proj}{R}");
        println!("    {DIM}compose file : {}{R}", compose_file.display());

        if !compose_file.exists() {
            println!("    {YLW}compose file not found — skipping{R}");
            continue;
        }

        // Show running containers before the wipe so we can confirm what's there.
        let before = Command::new("docker")
            .args(["ps", "-a", "--filter", &format!("label=com.docker.compose.project={ws_proj}"),
                   "--format", "{{.Names}}  {{.Status}}"])
            .stdin(Stdio::null()).output();
        let before_str = before.ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        if before_str.trim().is_empty() {
            println!("    {DIM}no containers found under project {ws_proj}{R}");
        } else {
            println!("    {DIM}containers before wipe:{R}");
            for line in before_str.lines().filter(|l| !l.trim().is_empty()) {
                println!("      {DIM}{line}{R}");
            }
        }

        let file_str = compose_file.to_str().unwrap_or("");

        // (a) Workspace-scoped project with container-name override.
        println!("    {DIM}─ (a) workspace-scoped down -v{R}");
        let ws_override = write_compose_override(&compose_file, slug);
        let mut argv: Vec<&str> = vec!["compose", "-p", &ws_proj, "-f", file_str];
        let ov_str: String;
        if let Some(ref ov) = ws_override {
            ov_str = ov.to_string_lossy().into_owned();
            println!("    {DIM}    override file: {ov_str}{R}");
            argv.extend_from_slice(&["-f", &ov_str]);
        }
        argv.extend_from_slice(&["down", "-v"]);
        run_blocking_logged("docker", &argv, dir);

        // (b) Base project name (old naming scheme / started outside dev-feature).
        println!("    {DIM}─ (b) base-project down -v{R}");
        run_blocking_logged("docker",
            &["compose", "-p", &base_proj, "-f", file_str, "down", "-v"], dir);

        println!("    {GRN}done{R}");
        println!();
    }
    println!();
}

/// Called outside raw mode (the selector drops raw before returning Delete).
fn run_workspace_delete(config: &WorkspaceConfig, workspace_root: &Path, ws_dir: &Path) {
    let sep = "─".repeat(56);
    println!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}remove workspace {}{R}", config.hash);
    println!("\n  {DIM}{sep}{R}");

    // ── 1. Classify entries ────────────────────────────────────────────────────
    // Separate entries into:
    //   - worktrees_to_remove : extra git worktrees created by dev-feature
    //   - main_checkouts      : main repo dirs (never removed — shared infra)
    struct DirtyEntry { repo: String, worktree: PathBuf, reasons: Vec<String> }
    let mut dirty: Vec<DirtyEntry> = Vec::new();
    let mut worktrees_to_remove: Vec<(String, PathBuf)> = Vec::new();
    let mut main_checkouts:       Vec<(String, PathBuf)> = Vec::new();

    for entry in &config.entries {
        if !entry.enabled { continue; }
        let main = workspace_root.join(&entry.repo);

        if entry.branch.is_empty() {
            // Product was included using its main checkout directly (no branch).
            if main.is_dir() { main_checkouts.push((entry.repo.clone(), main)); }
            continue;
        }

        let slug = branch_to_slug(&entry.branch);
        let wt   = workspace_root.join(format!("{}-{}", entry.repo, slug));

        if !wt.is_dir() || wt == main {
            // Either no worktree was created (main was already on this branch),
            // or the worktree path IS the main checkout (same path).
            if main.is_dir() { main_checkouts.push((entry.repo.clone(), main)); }
        } else {
            // A real separate worktree exists at wt — collect for removal.
            let reasons = worktree_dirty_reasons(&wt);
            if !reasons.is_empty() {
                dirty.push(DirtyEntry { repo: entry.repo.clone(), worktree: wt.clone(), reasons });
            }
            worktrees_to_remove.push((entry.repo.clone(), wt));
        }
    }

    // ── 2. Preview what will happen ────────────────────────────────────────────
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

    // ── 3. Confirmation ────────────────────────────────────────────────────────
    if !dirty.is_empty() {
        println!("  {YLW}{BOLD}Warning: the following worktrees have ongoing work:{R}\n");
        for d in &dirty {
            println!("  {YLW}▶{R}  {BOLD}{}{R}  ({})", d.repo, d.reasons.join(", "));
            println!("     {DIM}{}{R}", d.worktree.display());
        }
        println!();
        print!("  Type {BOLD}YES{R} to confirm removal despite uncommitted work: ");
        let _ = io::stdout().flush();
        match read_line_or_interrupt() {
            Some(l) if l.trim() == "YES" => {}
            _ => { println!("  Cancelled."); return; }
        }
        print!("  This cannot be undone.  Type {BOLD}YES{R} again to proceed: ");
        let _ = io::stdout().flush();
        match read_line_or_interrupt() {
            Some(l) if l.trim() == "YES" => {}
            _ => { println!("  Cancelled."); return; }
        }
    } else {
        print!("  Remove workspace {BOLD}{}{R}? [y/N] ", config.hash);
        let _ = io::stdout().flush();
        match read_line_or_interrupt() {
            Some(l) if l.trim().eq_ignore_ascii_case("y") => {}
            _ => { println!("  Cancelled."); return; }
        }
    }

    println!();

    // ── 4. Docker compose down ─────────────────────────────────────────────────
    // Run for ALL enabled entries regardless of branch — Docker was started for
    // every enabled product, including those using the main checkout directly.
    let ws_hash = config.hash.as_str();
    for entry in &config.entries {
        if !entry.enabled { continue; }
        if entry.repo == "connectors" { continue; } // shares OpenCTI docker

        // Resolve the dir to run compose from: worktree if it exists, else main.
        let slug = branch_to_slug(&entry.branch);
        let wt   = workspace_root.join(format!("{}-{}", entry.repo, slug));
        let dir_buf = if !entry.branch.is_empty() && wt.is_dir() {
            wt
        } else {
            workspace_root.join(&entry.repo)
        };
        let dir = dir_buf.as_path();
        if !dir.is_dir() { continue; }

        let Some((ws_project, base_project, compose_file)) =
            resolve_product_docker_for_down(&entry.repo, dir, ws_hash) else { continue };

        if !compose_file.exists() { continue; }
        let file_str = compose_file.to_str().unwrap_or("");

        print!("  Stopping {} Docker containers… ", entry.repo);
        let _ = io::stdout().flush();

        // (a) Workspace-scoped project (new naming scheme with hash suffix).
        // `-v` removes named volumes so the next launch of this (or any new)
        // workspace starts with a clean ES/Redis/etc. data state.
        let ws_override = write_compose_override(&compose_file, ws_hash);
        let mut argv_ws: Vec<&str> = vec!["compose", "-p", &ws_project, "-f", file_str];
        let ov_str: String;
        if let Some(ref ov) = ws_override {
            ov_str = ov.to_string_lossy().into_owned();
            argv_ws.extend_from_slice(&["-f", &ov_str]);
        }
        argv_ws.extend_from_slice(&["down", "-v"]);
        let _ = run_blocking("docker", &argv_ws, dir);

        // (b) Base project name (old scheme / started by ./dev.sh or plain compose).
        let _ = run_blocking("docker",
            &["compose", "-p", &base_project, "-f", file_str, "down", "-v"], dir);

        // (c) Straggler sweep: force-remove any remaining containers whose name
        //     still starts with the product prefix (catches containers with explicit
        //     container_name: that slipped through both compose-down calls).
        let container_prefix = base_project.split('-').next().unwrap_or(&base_project);
        docker_kill_by_name_fragment(container_prefix);

        println!("{GRN}done{R}");
    }

    // ── 5. Worktree removal ────────────────────────────────────────────────────
    for (repo, wt) in &worktrees_to_remove {
        let main_repo = workspace_root.join(repo);
        let wt_str    = wt.to_str().unwrap_or("");

        print!("  Removing worktree {}… ", wt.display());
        let _ = io::stdout().flush();

        let git_ok = Command::new("git")
            .args(["worktree", "remove", "--force", wt_str])
            .current_dir(&main_repo)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        // If git couldn't remove it (already pruned from git's registry), the
        // directory might still exist as a plain dir — force-remove it.
        if !git_ok && wt.is_dir() {
            let _ = fs::remove_dir_all(wt);
        }

        // Prune dangling git worktree reference.
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&main_repo)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .status();

        // Also delete the local branch that tracked this worktree, if it still exists.
        let slug = wt.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(&format!("{}-", repo)))
            .unwrap_or("")
            .to_string();
        if !slug.is_empty() {
            let _ = Command::new("git")
                .args(["branch", "-D", &slug])
                .current_dir(&main_repo)
                .stdin(Stdio::null()).stderr(Stdio::null())
                .status();
        }

        if !wt.is_dir() {
            println!("{GRN}done{R}");
        } else {
            println!("{YLW}could not remove — delete manually:{R}");
            println!("    rm -rf {}", wt.display());
        }
    }

    // ── 6. Tombstone ──────────────────────────────────────────────────────────
    tombstone_workspace(ws_dir, &config.hash);

    println!("\n  {GRN}✓{R}  Workspace {BOLD}{}{R} deleted.", config.hash);
    if !worktrees_to_remove.is_empty() {
        println!("  {DIM}Worktree directories removed.{R}");
    }
    if !main_checkouts.is_empty() {
        println!("  {DIM}Main repo directories kept — they are shared across all workspaces.{R}");
        println!("  {DIM}To fully reset, delete {} manually.{R}", workspace_root.display());
    }
    println!();
}

// ── Terminal ──────────────────────────────────────────────────────────────────

fn terminal_size() -> (usize, usize) {
    crossterm::terminal::size()
        .map(|(c, r)| (c as usize, r as usize))
        .unwrap_or((120, 40))
}

/// RAII guard: enters raw mode + alternate screen on creation, restores on drop.
/// Owns a `Terminal<CrosstermBackend<Stdout>>` for use by Ratatui widgets.
/// Render functions that still use raw ANSI can write to stdout directly while
/// the terminal is held — they just must not call `terminal.draw()` concurrently.
struct TuiGuard;

impl TuiGuard {
    fn enter() -> Option<Self> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 { return None; }
        if enable_raw_mode().is_err() { return None; }
        let mut stdout = io::stdout();
        // Enter alternate screen buffer + hide cursor.
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
        // Leave alternate screen, show cursor, reset attributes.
        let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
        let _ = stdout.flush();
        let _ = disable_raw_mode();
    }
}

// ── Input ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum InputEvent {
    Up, Down,
    Enter,       // open log / confirm (Enter, →, l)
    Back,        // close log / quit (q, Q, Esc, ←)
    PageUp, PageDown,
    Follow,      // f — toggle live-follow in log view
    Credentials, // e — show .env credentials overlay
    Diagnose,    // d — run on-demand service diagnosis from log view
    Report,      // r — report missing recipe (Diagnose mode only)
    Restart,     // R — kill and re-spawn the highlighted service
}

/// Translate a crossterm `KeyEvent` into our `InputEvent` vocabulary.
fn map_key_event(ke: KeyEvent) -> Option<InputEvent> {
    match ke.code {
        // Back: q / Q / Esc / Left-arrow
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc | KeyCode::Left
            => Some(InputEvent::Back),
        // Up: ↑ / k
        KeyCode::Up | KeyCode::Char('k') => Some(InputEvent::Up),
        // Down: ↓ / j
        KeyCode::Down | KeyCode::Char('j') => Some(InputEvent::Down),
        // Enter: Return / Right-arrow / l
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Some(InputEvent::Enter),
        KeyCode::PageUp   => Some(InputEvent::PageUp),
        KeyCode::PageDown => Some(InputEvent::PageDown),
        KeyCode::Char('f') => Some(InputEvent::Follow),
        KeyCode::Char('e') => Some(InputEvent::Credentials),
        KeyCode::Char('d') => Some(InputEvent::Diagnose),
        KeyCode::Char('r') if !ke.modifiers.contains(KeyModifiers::SHIFT)
            => Some(InputEvent::Report),
        KeyCode::Char('R') | KeyCode::Char('r')
            if ke.modifiers.contains(KeyModifiers::SHIFT)
            => Some(InputEvent::Restart),
        _ => None,
    }
}

fn spawn_input_thread(tx: mpsc::SyncSender<InputEvent>, stopping: Arc<AtomicBool>) {
    thread::spawn(move || {
        loop {
            if stopping.load(Ordering::Relaxed) { return; }
            // Poll with a short timeout so the stopping flag is checked frequently.
            match event::poll(Duration::from_millis(20)) {
                Ok(true) => {
                    if let Ok(Event::Key(ke)) = event::read() {
                        if let Some(e) = map_key_event(ke) {
                            let _ = tx.try_send(e);
                        }
                    }
                }
                _ => {}
            }
        }
    });
}

// ── Mode ─────────────────────────────────────────────────────────────────────

enum Mode {
    Overview    { cursor: usize },
    LogView     { svc_idx: usize, scroll: usize, follow: bool },
    Diagnose    { svc_idx: usize, findings: Vec<Finding>, cursor: usize },
    Credentials,
}

/// Write a list of ANSI-coded lines to the alternate screen buffer.
/// Convert a slice of ANSI-coded strings into a Ratatui `Text<'static>`.
///
/// Handles the specific sequences produced by our render functions:
///   `\x1b[0m`         → reset
///   `\x1b[1m`         → bold
///   `\x1b[2m`         → dim
///   `\x1b[22m`        → clear bold/dim
///   `\x1b[31m`–`[36m` → standard fg colours (red, green, yellow, cyan …)
///   `\x1b[38;5;Nm`    → 256-colour fg (used by the gradient hint)
///
/// Unknown sequences are silently ignored so service log output with arbitrary
/// ANSI codes is displayed with whatever styling was understood.
/// Render a list of ANSI-coded lines to the terminal.
///
/// Clears the screen, moves to (0,0), then writes each line with its embedded
/// ANSI escape codes directly via crossterm.  This path is byte-transparent:
/// UTF-8 multi-byte sequences, box-drawing chars, and coloured spans all reach
/// the terminal unmodified.
fn draw_ansi_lines(_tui: &mut TuiGuard, lines: &[String]) {
    use crossterm::terminal::{Clear, ClearType};
    use crossterm::cursor::MoveTo;
    let mut out = io::stdout();
    let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
    for line in lines {
        // `\r\n` required in raw mode: \n alone only moves down, not to column 0.
        let _ = write!(out, "{}\r\n", line);
    }
    let _ = out.flush();
}

/// Drain all pending crossterm events (use before entering a selector after raw
/// output so stale key events don't accidentally fire in the new screen).
fn drain_input_events() {
    while event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let _ = event::read();
    }
}


// ── Per-process shutdown state ────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
enum TermStatus {
    /// SIGTERM sent; waiting for the process to exit.
    Terminating,
    /// Process exited on its own (exit code).
    Stopped(i32),
    /// SIGKILL was sent after the grace timeout.
    Killed,
}

// ── Log tail ──────────────────────────────────────────────────────────────────

/// Return the last `max_lines` lines from a file (reads the whole file — fine for dev logs).
fn tail_file(path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(f) = File::open(path) else { return vec![] };
    let all: Vec<String> = io::BufReader::new(f).lines().filter_map(|l| l.ok()).collect();
    let start = all.len().saturating_sub(max_lines);
    all[start..].to_vec()
}

// ── ANSI-aware string helpers ─────────────────────────────────────────────────

/// Count the number of visible (non-ANSI) characters in `s`.
fn ansi_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut len = 0usize;
    let mut i   = 0;
    while i < b.len() {
        if b[i] == b'\x1b' && b.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < b.len() && b[i] != b'm' { i += 1; }
            i += 1;
        } else {
            len += 1;
            i   += 1;
        }
    }
    len
}

/// Return `s` followed by enough spaces to reach `width` visible columns.
fn pad_ansi(s: &str, width: usize) -> String {
    let pad = width.saturating_sub(ansi_len(s));
    format!("{s}{}", " ".repeat(pad))
}

// ── Render — overview ─────────────────────────────────────────────────────────

fn build_overview_lines(svcs: &[Svc], slug: &str, logs_dir: &Path, cursor: usize, has_tui: bool) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}\n"));

    // Column widths: name 26 | status 32 | pid 7 | url + elapsed
    out.push(format!("  {BOLD}  {:<26}{:<32}{:<7}{R}", "Service", "Status", "PID"));
    out.push(format!("  {DIM}  {}{R}", "─".repeat(67)));

    let visible: Vec<usize> = svcs.iter().enumerate()
        .filter(|(_, s)| s.health != Health::Pending)
        .map(|(i, _)| i)
        .collect();

    for (row, &i) in visible.iter().enumerate() {
        let s = &svcs[i];
        let pid     = s.pid.map(|p| p.to_string()).unwrap_or_default();
        let url_str = s.url.as_deref().unwrap_or("");
        let elapsed = s.started_at.map(|_| s.secs());

        let (marker, name_str) = if has_tui && row == cursor {
            (format!("{CYN}{BOLD}▶{R} "), format!("{BOLD}{}{R}", s.name))
        } else {
            ("  ".to_string(), s.name.to_string())
        };

        // Plain label for cursor row (avoids ANSI bytes confusing the width calc).
        let status_col = if has_tui && row == cursor {
            pad_ansi(&s.health.label_plain(), 32)
        } else {
            pad_ansi(&s.health.label(), 32)
        };

        let mut line = format!("  {marker}{:<26}{status_col}{:<7}", name_str, pid);
        if !url_str.is_empty() { line.push_str(&format!("  {DIM}{url_str}{R}")); }
        if let Some(s) = elapsed { line.push_str(&format!("  {DIM}{s}s{R}")); }
        out.push(line);

        if let Some(diag) = &s.diagnosis {
            out.push(format!("        {YLW}▸ {diag}{R}"));
        }
    }

    out.push(String::new());

    let active: Vec<_> = svcs.iter().filter(|s| s.health != Health::Pending).collect();
    let all_up = !active.is_empty()
        && active.iter().all(|s| matches!(s.health, Health::Up | Health::Running));
    let any_bad = active.iter().any(|s| {
        matches!(s.health, Health::Crashed(_) | Health::Degraded(_))
    });

    if any_bad {
        out.push(format!("  {RED}{BOLD}One or more services failed.{R}"));
    } else if all_up {
        out.push(format!("  {GRN}{BOLD}All services up.{R}"));
    } else {
        out.push(format!("  Waiting for services…"));
    }

    if has_tui {
        out.push(format!("  {DIM}↑↓ navigate   Enter/→ logs   d diagnose   R restart   e credentials   q quit{R}"));
    } else {
        out.push(format!("  {DIM}Ctrl+C to stop   tail -f {}/*.log{R}", logs_dir.display()));
    }
    out.push(String::new());
    out
}

// ── Diagnosis ─────────────────────────────────────────────────────────────────

// ── Finding data types ────────────────────────────────────────────────────────

/// One command step in a multi-step fix sequence.
#[derive(Clone)]
struct FixStep { args: Vec<String>, cwd: PathBuf }

impl FixStep {
    fn new(args: &[&str], cwd: &Path) -> Self {
        Self { args: args.iter().map(|s| s.to_string()).collect(), cwd: cwd.to_path_buf() }
    }
}

/// An automated action that can be executed directly from the diagnosis screen.
#[derive(Clone)]
enum FixAction {
    /// Run a sequence of shell commands (printed + executed in order).
    /// When `restart_after` is true the app automatically re-spawns the service
    /// after all steps complete successfully.
    Steps { label: String, steps: Vec<FixStep>, restart_after: bool },
    /// Launch the interactive env-var wizard for a product.
    /// When `restart_after` is true the service is automatically re-spawned once
    /// the wizard closes so the new values are picked up without a full relaunch.
    EnvWizard { env_path: PathBuf, deploy_to: Option<PathBuf>, vars: &'static [EnvVar], product: &'static str, restart_after: bool },
    /// Write a single key=value into an env file (idempotent) then optionally restart.
    PatchEnvVar {
        label:         String,
        env_path:      PathBuf,
        key:           &'static str,
        value:         &'static str,
        restart_after: bool,
    },
}

impl FixAction {
    fn label(&self) -> &str {
        match self {
            FixAction::Steps       { label, .. } => label.as_str(),
            FixAction::EnvWizard   { product, .. } => product,
            FixAction::PatchEnvVar { label, .. } => label.as_str(),
        }
    }
    fn restart_after(&self) -> bool {
        match self {
            FixAction::Steps       { restart_after, .. } => *restart_after,
            FixAction::EnvWizard   { restart_after, .. } => *restart_after,
            FixAction::PatchEnvVar { restart_after, .. } => *restart_after,
        }
    }
}

/// A single diagnosed issue, optionally with a runnable fix.
#[derive(Clone)]
struct Finding {
    kind:     &'static str,
    title:    String,
    body:     Vec<String>,
    fix:      Option<FixAction>,
    resolved: bool,
}

impl Finding {
    fn info(kind: &'static str, title: impl Into<String>, body: Vec<String>) -> Self {
        Self { kind, title: title.into(), body, fix: None, resolved: false }
    }
    fn fixable(kind: &'static str, title: impl Into<String>, body: Vec<String>, fix: FixAction) -> Self {
        Self { kind, title: title.into(), body, fix: Some(fix), resolved: false }
    }
}

/// Execute a fix action synchronously with visible output.
/// Must be called with raw mode already dropped.
/// Returns true when all steps completed successfully.
fn run_fix_action(action: &FixAction) -> bool {
    let sep = "─".repeat(56);
    match action {
        FixAction::Steps { label, steps, .. } => {
            println!("\n  {BOLD}{CYN}Applying fix:{R}  {label}\n  {DIM}{sep}{R}\n");
            for step in steps {
                println!("  {DIM}$ {}{R}", step.args.join(" "));
                let prog  = step.args[0].as_str();
                let argv: Vec<&str> = step.args[1..].iter().map(|s| s.as_str()).collect();
                let code  = run_blocking(prog, &argv, &step.cwd);
                if code != 0 {
                    println!("\n  {RED}✗{R}  Command exited {code}. Remaining steps skipped.");
                    return false;
                }
            }
            println!("\n  {GRN}✓{R}  Fix applied.");
            true
        }
        FixAction::EnvWizard { env_path, deploy_to, vars, product, .. } => {
            run_env_wizard(env_path, vars, product);
            if let Some(dest) = deploy_to {
                deploy_workspace_env(env_path, dest);
            }
            true
        }
        FixAction::PatchEnvVar { label, env_path, key, value, .. } => {
            println!("\n  {BOLD}{CYN}Applying fix:{R}  {label}\n  {DIM}{sep}{R}\n");
            let mut env = parse_env_file(env_path);
            env.insert(key.to_string(), value.to_string());
            write_env_file(env_path, &env);
            println!("  {GRN}✓{R}  Set {key}={value}");
            println!("  {DIM}  in {}{R}", env_path.display());
            true
        }
    }
}

/// Extra runtime context attached to a missing-recipe report.
struct IssueContext {
    /// Health label at the time the user triggered the report (e.g. "Crashed (exit 1)").
    health:      String,
    /// Seconds the service had been running (0 if not yet started).
    uptime_secs: u64,
    /// Absolute path to the service log file.
    log_path:    PathBuf,
    /// Full command used to start the service, if known.
    spawn_cmd:   Option<String>,
}

/// Open a GitHub issue on the dev-launcher repo requesting a new recipe for the
/// given finding.  Returns the issue URL on success or an error string.
fn create_github_issue(
    kind:       &str,
    svc_name:   &str,
    title:      &str,
    body_lines: &[String],
    log_tail:   &[String],
    ctx:        &IssueContext,
) -> Result<String, String> {
    let issue_title = format!("recipe needed: {} ({})", title, kind);

    // ── Service state table ───────────────────────────────────────────────────
    let uptime_str = if ctx.uptime_secs == 0 {
        "< 1s".to_string()
    } else if ctx.uptime_secs < 60 {
        format!("{}s", ctx.uptime_secs)
    } else {
        format!("{}m {}s", ctx.uptime_secs / 60, ctx.uptime_secs % 60)
    };
    let cmd_str = ctx.spawn_cmd.as_deref().unwrap_or("unknown");
    let log_str = ctx.log_path.to_string_lossy();

    let svc_state = format!(
        "### Service state\n\
         | Field | Value |\n\
         |-------|-------|\n\
         | **Health**  | {health} |\n\
         | **Uptime**  | {uptime} |\n\
         | **Command** | `{cmd}` |\n\
         | **Log**     | `{log}` |\n",
        health = ctx.health,
        uptime = uptime_str,
        cmd    = cmd_str,
        log    = log_str,
    );

    // ── Environment ───────────────────────────────────────────────────────────
    let os_version = Command::new("uname").args(["-srm"])
        .output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));

    let env_section = format!(
        "### Environment\n\
         | Field | Value |\n\
         |-------|-------|\n\
         | **OS**   | {os} |\n\
         | **Arch** | {arch} |\n",
        os   = os_version,
        arch = std::env::consts::ARCH,
    );

    // ── Logs section ─────────────────────────────────────────────────────────
    let log_section = if log_tail.is_empty() {
        String::new()
    } else {
        format!("### Logs\n```\n{}\n```\n\n", log_tail.join("\n"))
    };

    let issue_body = format!(
        "## Missing fix recipe\n\n\
         A finding was encountered that has no automated fix yet.\n\n\
         | Field   | Value |\n\
         |---------|-------|\n\
         | **Kind**    | `{kind}` |\n\
         | **Service** | `{svc_name}` |\n\
         | **Finding** | {title} |\n\n\
         {svc_state}\n\
         ### Details\n\
         ```\n{details}\n```\n\n\
         {log_section}\
         {env_section}\n\
         Please implement a recipe (fix action) for this kind in `diagnose_service`.",
        details     = body_lines.join("\n"),
        svc_state   = svc_state,
        log_section = log_section,
        env_section = env_section,
    );

    // GitHub GraphQL hard limit is 65536 bytes. Truncate if needed.
    const GH_BODY_LIMIT: usize = 65_000;
    let body_arg = if issue_body.len() > GH_BODY_LIMIT {
        format!("{}\n\n*(truncated — body exceeded GitHub limit)*", &issue_body[..GH_BODY_LIMIT])
    } else {
        issue_body
    };

    let out = Command::new("gh")
        .args(["issue", "create",
               "--repo",  "AreDee-Bangs/dev-launcher",
               "--title", &issue_title,
               "--body",  &body_arg,
               "--label", "recipe-needed"])
        .stdin(Stdio::null())   // prevent gh from blocking on auth prompts
        .output()
        .map_err(|e| format!("gh not found: {e}"))?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Build the fix steps needed to create a Python venv and install dependencies
/// in `backend_dir`.  Tries `requirements.txt` then `pyproject.toml`.
fn venv_fix_steps(backend_dir: &Path) -> Vec<FixStep> {
    let mut steps = vec![
        FixStep::new(&["python3", "-m", "venv", ".venv"], backend_dir),
    ];
    if backend_dir.join("requirements.txt").exists() {
        steps.push(FixStep::new(
            &[".venv/bin/pip", "install", "-q", "-r", "requirements.txt"],
            backend_dir,
        ));
    } else if backend_dir.join("pyproject.toml").exists() {
        steps.push(FixStep::new(
            &[".venv/bin/pip", "install", "-q", "-e", "."],
            backend_dir,
        ));
    }
    steps
}

/// Analyse a service and return a list of `Finding` structs, each of which may
/// carry a `FixAction` that the user can execute directly from the diagnosis screen.
fn diagnose_service(svc: &Svc, paths: &Paths, ws_env_dir: &Path) -> Vec<Finding> {
    // ── Map service name → repo directory ────────────────────────────────────
    let repo_dir: &Path = if svc.name.starts_with("copilot") {
        &paths.copilot
    } else if svc.name.starts_with("opencti") {
        &paths.opencti
    } else if svc.name.starts_with("connector") {
        &paths.connector
    } else if svc.name.starts_with("openaev") {
        &paths.openaev
    } else {
        &paths.copilot
    };

    let mut findings: Vec<Finding> = Vec::new();

    // ── 1. Degraded: surface reason with automated fix ────────────────────────
    if let Health::Degraded(msg) = &svc.health {
        let body = vec![msg.clone()];

        // Helper closures for resolved-condition checks — used to avoid offering
        // a fix that was already applied but whose effect the service hasn't
        // reflected yet (because it hasn't restarted).
        let backend_dir_for_check = || {
            if repo_dir.join("backend").is_dir() {
                repo_dir.join("backend")
            } else {
                repo_dir.to_path_buf()
            }
        };
        let fe_dir_for_check = || {
            if repo_dir.join("frontend").is_dir() {
                repo_dir.join("frontend")
            } else {
                repo_dir.to_path_buf()
            }
        };

        enum DegradedOutcome {
            NeedsRestart,           // fix was applied, condition resolved, just needs service restart
            Fixable(FixAction, &'static str), // (fix, kind)
            Unknown,
        }

        let outcome = if msg.contains("venv") || msg.contains(".venv") {
            let bd = backend_dir_for_check();
            // Check both `python` and versioned symlinks (e.g. python3.14).
            let venv_ok = bd.join(".venv/bin/python").exists()
                || bd.join(".venv/bin/python3").exists()
                || fs::read_dir(bd.join(".venv/bin")).ok()
                    .and_then(|mut d| d.next())
                    .is_some();
            if venv_ok {
                DegradedOutcome::NeedsRestart
            } else {
                DegradedOutcome::Fixable(
                    FixAction::Steps {
                        label: "Create Python virtual environment and install dependencies".into(),
                        steps: venv_fix_steps(&bd),
                        restart_after: false,
                    },
                    KIND_PYTHON_VENV,
                )
            }
        } else if msg.contains("node_modules") {
            let fe = fe_dir_for_check();
            if fe.join("node_modules").is_dir() {
                DegradedOutcome::NeedsRestart
            } else {
                DegradedOutcome::Fixable(
                    FixAction::Steps {
                        label: "Install JavaScript dependencies (yarn install)".into(),
                        steps: vec![FixStep::new(&["yarn", "install"], &fe)],
                        restart_after: false,
                    },
                    KIND_NODE_MODULES,
                )
            }
        } else if msg.contains("APP__ADMIN__PASSWORD") || msg.contains("credentials") {
            DegradedOutcome::Fixable(
                FixAction::EnvWizard {
                    env_path:  ws_env_dir.join("opencti.env"),
                    deploy_to: Some(paths.opencti.join("opencti-platform/opencti-graphql/.env.dev")),
                    vars:      OPENCTI_ENV_VARS,
                    product:   "OpenCTI",
                    restart_after: false,
                },
                KIND_ENV_PLACEHOLDER,
            )
        } else if msg.contains("OPENCTI_TOKEN") {
            DegradedOutcome::Fixable(
                FixAction::EnvWizard {
                    env_path:  ws_env_dir.join("connector.env"),
                    deploy_to: Some(paths.connector.join(".env.dev")),
                    vars:      CONNECTOR_ENV_VARS,
                    product:   "ImportDocumentAI connector",
                    restart_after: false,
                },
                KIND_ENV_PLACEHOLDER,
            )
        } else {
            DegradedOutcome::Unknown
        };

        match outcome {
            DegradedOutcome::NeedsRestart => {
                // The underlying condition is already fixed; the service just
                // hasn't been restarted yet.  Show an info finding — no fix action.
                let mut restart_body = body;
                restart_body.push("  Fix already applied — restart the service to pick it up.".into());
                restart_body.push("  Press Ctrl+C and run dev-feature again.".into());
                let mut f = Finding::info(KIND_INFO, "Dependency installed — restart needed", restart_body);
                f.resolved = true;
                findings.push(f);
            }
            DegradedOutcome::Fixable(fix, kind) => {
                findings.push(Finding::fixable(kind, "Service did not start", body, fix));
            }
            DegradedOutcome::Unknown => {
                findings.push(Finding::info(KIND_DEGRADED_UNKNOWN, "Service did not start", body));
            }
        }
    }

    // ── 1b. Crashed: check for known patterns before falling back to generic ──
    if let Health::Crashed(code) = &svc.health {
        let crash_log = tail_file(&svc.log_path, 30);
        let mut crash_handled = false;

        // ── opencti-graphql: Elasticsearch partial init ───────────────────────
        // Happens when the platform was killed mid-initialization, leaving a
        // partial ES index.  Fix: stop the ES container and wipe its data volume.
        if svc.name == "opencti-graphql"
            && crash_log.iter().any(|l| l.contains("index already exists"))
        {
            let es_container = Command::new("docker")
                .args(["ps", "-a",
                       "--filter", "name=opencti-dev-elasticsearch",
                       "--format", "{{.Names}}"])
                .output().ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
                .filter(|s| !s.is_empty());
            let es_volume = Command::new("docker")
                .args(["volume", "ls", "--filter", "name=esdata", "-q"])
                .output().ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
                .filter(|s| !s.is_empty());

            match (es_container, es_volume) {
                (Some(container), Some(volume)) => {
                    // Derive compose project name from the container's labels.
                    let compose_project = Command::new("docker")
                        .args(["inspect", &container, "--format",
                               "{{index .Config.Labels \"com.docker.compose.project\"}}"])
                        .output().ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    let compose_file = paths.opencti
                        .join("opencti-platform/opencti-dev/docker-compose.yml");

                    let mut steps = vec![
                        FixStep::new(&["docker", "stop", &container], &paths.opencti),
                        FixStep::new(&["docker", "volume", "rm", &volume], &paths.opencti),
                    ];
                    // Recreate the ES container + fresh volume via compose so the app
                    // can restart opencti-graphql immediately after.
                    if let (Some(ref project), true) = (&compose_project, compose_file.exists()) {
                        let file_str = compose_file.to_string_lossy().into_owned();
                        steps.push(FixStep::new(
                            &["docker", "compose", "-p", project, "-f", &file_str, "up", "-d",
                              "opencti-dev-elasticsearch"],
                            &paths.opencti,
                        ));
                    }

                    let mut body = vec![
                        format!("  Exit code : {code}"),
                        "  A previous run was interrupted during first-time schema init.".into(),
                        "  Elasticsearch holds a partial index that blocks re-initialization.".into(),
                        format!("  Container : {container}"),
                        format!("  Volume    : {volume}"),
                    ];
                    if compose_project.is_some() {
                        body.push("  Fix: stop ES, wipe volume, restart ES, then re-launch opencti-graphql.".into());
                    } else {
                        body.push("  Fix: stop ES and wipe volume — restart opencti-graphql manually after.".into());
                    }

                    findings.push(Finding::fixable(
                        KIND_OPENCTI_ES_PARTIAL_INIT,
                        "Elasticsearch index partially initialized (interrupted previous run)",
                        body,
                        FixAction::Steps {
                            label: "Wipe stale ES data, restart Elasticsearch, re-launch service".into(),
                            steps,
                            restart_after: compose_project.is_some(),
                        },
                    ));
                    crash_handled = true;
                }
                (container, volume) => {
                    // Pattern matched but Docker resources not found — surface as info
                    // so the user at least knows what happened.
                    findings.push(Finding::info(
                        KIND_OPENCTI_ES_PARTIAL_INIT,
                        "Elasticsearch index partially initialized (interrupted previous run)",
                        vec![
                            format!("  Exit code : {code}"),
                            "  A previous run was interrupted during first-time schema init.".into(),
                            "  Could not locate Docker resources to auto-fix:".into(),
                            format!("  Container : {}", container.as_deref().unwrap_or("not found")),
                            format!("  Volume    : {}", volume.as_deref().unwrap_or("not found")),
                            "  Manual fix: docker stop <es-container> && docker volume rm <esdata-volume>".into(),
                        ],
                    ));
                    crash_handled = true;
                }
            }
        }

        // ── connector: missing CONNECTOR_TYPE ────────────────────────────────
        // Happens when CONNECTOR_TYPE is absent from the env file so the Python
        // config library resolves it to None, causing ConnectorType(None) to throw.
        if !crash_handled
            && svc.name == "connector"
            && crash_log.iter().any(|l| l.contains("None is not a valid ConnectorType"))
        {
            let env_path = paths.connector.join(".env.dev");
            findings.push(Finding::fixable(
                KIND_CONNECTOR_TYPE_MISSING,
                "CONNECTOR_TYPE not configured",
                vec![
                    format!("  Exit code : {code}"),
                    "  The connector env file is missing CONNECTOR_TYPE.".into(),
                    "  Python resolved it to None → ConnectorType(None) → ValueError.".into(),
                    format!("  Env file  : {}", env_path.display()),
                    "  Fix: add CONNECTOR_TYPE=INTERNAL_IMPORT_FILE".into(),
                ],
                FixAction::PatchEnvVar {
                    label:    "Add CONNECTOR_TYPE=INTERNAL_IMPORT_FILE to connector env".into(),
                    env_path,
                    key:      "CONNECTOR_TYPE",
                    value:    "INTERNAL_IMPORT_FILE",
                    restart_after: true,
                },
            ));
            crash_handled = true;
        }

        // ── connector: licence key not configured ─────────────────────────────
        // CONNECTOR_LICENCE_KEY_PEM is empty / absent → Python tries to call
        // None.encode() inside base64.b64encode() → AttributeError.
        if !crash_handled
            && svc.name == "connector"
            && crash_log.iter().any(|l| l.contains("NoneType' object has no attribute 'encode'"))
        {
            let ws_connector = ws_env_dir.join("connector.env");
            let repo_connector = paths.connector.join(".env.dev");
            findings.push(Finding::fixable(
                KIND_CONNECTOR_LICENCE_MISSING,
                "Filigran licence key not configured",
                vec![
                    format!("  Exit code : {code}"),
                    "  CONNECTOR_LICENCE_KEY_PEM is empty or missing.".into(),
                    "  Python called base64.b64encode(None.encode()) → AttributeError.".into(),
                    format!("  Env file  : {}", ws_connector.display()),
                    "  Fix: paste the PEM certificate in the wizard below.".into(),
                ],
                FixAction::EnvWizard {
                    env_path:  ws_connector,
                    deploy_to: Some(repo_connector),
                    vars:      CONNECTOR_LICENCE_VARS,
                    product:   "ImportDocumentAI connector — licence key",
                    restart_after: true,
                },
            ));
            crash_handled = true;
        }

        if !crash_handled {
            findings.push(Finding::info(
                KIND_CRASH,
                format!("Service crashed (exit {})", code),
                vec![
                    format!("  Exit code: {}", code),
                    format!("  Log: {}", svc.log_path.display()),
                    "  No automated fix is available — use r to file a GitHub issue.".into(),
                ],
            ));
        }
    }

    // ── 2. Log pattern analysis ───────────────────────────────────────────────
    if matches!(svc.health, Health::Crashed(_) | Health::Degraded(_)) {
        let log_lines = tail_file(&svc.log_path, 150);

        // ── copilot-backend/worker: MinIO container down ──────────────────────
        if (svc.name.contains("copilot-backend") || svc.name.contains("copilot-worker"))
            && log_lines.iter().any(|l| l.to_lowercase().contains("minio not ready"))
        {
            let minio_info = Command::new("docker")
                .args(["ps", "-a",
                       "--filter", "name=copilot-minio",
                       "--format", "{{.Names}}\t{{.Status}}"])
                .stdin(Stdio::null())
                .output().ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.lines().next().map(|l| l.to_string()))
                .filter(|s| !s.is_empty());

            match minio_info {
                Some(ref line) => {
                    let parts: Vec<&str> = line.splitn(2, '\t').collect();
                    let cname  = parts[0].trim().to_string();
                    let status = parts.get(1).copied().unwrap_or("").trim().to_string();
                    let is_running = status.starts_with("Up");
                    if is_running {
                        findings.push(Finding::info(
                            KIND_MINIO_DOWN,
                            "MinIO unreachable (container running but connection failed)",
                            vec![
                                format!("  Container : {cname}"),
                                format!("  Status    : {status}"),
                                "  MinIO is running but the backend cannot connect to it.".into(),
                                "  Check MINIO_ENDPOINT in your .env (expected http://localhost:9000).".into(),
                            ],
                        ));
                    } else {
                        findings.push(Finding::fixable(
                            KIND_MINIO_DOWN,
                            "MinIO container is stopped",
                            vec![
                                format!("  Container : {cname}"),
                                format!("  Status    : {status}"),
                                "  MinIO (S3-compatible storage) is not running.".into(),
                                "  Fix: start the container, then restart the backend.".into(),
                            ],
                            FixAction::Steps {
                                label:         format!("Start MinIO container ({cname})"),
                                steps:         vec![FixStep::new(&["docker", "start", &cname], &paths.copilot)],
                                restart_after: true,
                            },
                        ));
                    }
                }
                None => {
                    let compose_file = paths.copilot.join("docker-compose.dev.yml");
                    let compose_str  = compose_file.to_string_lossy().into_owned();
                    if compose_file.exists() {
                        findings.push(Finding::fixable(
                            KIND_MINIO_DOWN,
                            "MinIO container not found — Docker stack not started",
                            vec![
                                "  No MinIO container detected (may never have been created).".into(),
                                "  Fix: start the full Copilot Docker stack.".into(),
                            ],
                            FixAction::Steps {
                                label: "Start Copilot Docker services (docker compose up -d)".into(),
                                steps: vec![FixStep::new(
                                    &["docker", "compose", "-f", &compose_str, "up", "-d"],
                                    &paths.copilot,
                                )],
                                restart_after: true,
                            },
                        ));
                    } else {
                        findings.push(Finding::info(
                            KIND_MINIO_DOWN,
                            "MinIO container not found",
                            vec![
                                "  No MinIO container detected — start Docker services first.".into(),
                                "  Run: docker compose -f docker-compose.dev.yml up -d".into(),
                            ],
                        ));
                    }
                }
            }
        }

        let mut matched: Vec<String> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for line in &log_lines {
            let lower = line.to_lowercase();
            for (needle, reason) in DIAG_PATTERNS {
                if lower.contains(needle) && !seen.contains(reason) {
                    seen.push(reason);
                    matched.push(format!("  — {reason}"));
                }
            }
        }
        if !matched.is_empty() {
            findings.push(Finding::info(KIND_INFO_LOG_PATTERNS, "Log patterns detected", matched));
        }
    }

    // ── 3. Env file placeholder values ────────────────────────────────────────
    struct EnvCheck {
        label:     &'static str,
        ws_path:   PathBuf,
        repo_path: PathBuf,
        vars:      &'static [EnvVar],
        product:   &'static str,
    }
    let env_checks = [
        EnvCheck {
            label:     "OpenCTI .env.dev",
            ws_path:   ws_env_dir.join("opencti.env"),
            repo_path: paths.opencti.join("opencti-platform/opencti-graphql/.env.dev"),
            vars:      OPENCTI_ENV_VARS,
            product:   "OpenCTI",
        },
        EnvCheck {
            label:     "Connector .env.dev",
            ws_path:   ws_env_dir.join("connector.env"),
            repo_path: paths.connector.join(".env.dev"),
            vars:      CONNECTOR_ENV_VARS,
            product:   "ImportDocumentAI connector",
        },
    ];
    for ec in &env_checks {
        // Prefer the workspace copy; fall back to the repo copy.
        let check_path = if ec.ws_path.exists() { &ec.ws_path } else { &ec.repo_path };
        if !check_path.exists() { continue; }
        let bad_keys: Vec<String> = parse_env_file(check_path).into_iter()
            .filter(|(_, v)| v == "ChangeMe")
            .map(|(k, _)| format!("  — {k} is still 'ChangeMe'"))
            .collect();
        if !bad_keys.is_empty() {
            findings.push(Finding::fixable(
                KIND_ENV_PLACEHOLDER,
                format!("Placeholder credentials in {}", ec.label),
                bad_keys,
                FixAction::EnvWizard {
                    env_path:  ec.ws_path.clone(),
                    deploy_to: Some(ec.repo_path.clone()),
                    vars:      ec.vars,
                    product:   ec.product,
                    restart_after: false,
                },
            ));
        }
    }

    // ── 4. Python venv missing ────────────────────────────────────────────────
    if svc.name.contains("backend") || svc.name.contains("worker") || svc.name.contains("connector") {
        let backend_dir = if repo_dir.join("backend").is_dir() {
            repo_dir.join("backend")
        } else {
            repo_dir.to_path_buf()
        };
        let venv_python = backend_dir.join(".venv/bin/python");
        if !venv_python.exists() {
            findings.push(Finding::fixable(
                KIND_PYTHON_VENV,
                "Python virtual environment missing",
                vec![format!("  Expected: {}", venv_python.display())],
                FixAction::Steps {
                    label: "Create Python virtual environment and install dependencies".into(),
                    steps: venv_fix_steps(&backend_dir),
                    restart_after: false,
                },
            ));
        }
    }

    // ── 5. node_modules missing ───────────────────────────────────────────────
    if svc.name.contains("frontend") {
        let fe_candidates = [repo_dir.join("frontend"), repo_dir.to_path_buf()];
        for fe_dir in &fe_candidates {
            if fe_dir.is_dir() && !fe_dir.join("node_modules").is_dir() {
                findings.push(Finding::fixable(
                    KIND_NODE_MODULES,
                    "JavaScript dependencies not installed",
                    vec![format!("  node_modules missing in {}", fe_dir.display())],
                    FixAction::Steps {
                        label: "Install JavaScript dependencies (yarn install)".into(),
                        steps: vec![FixStep::new(&["yarn", "install"], fe_dir)],
                        restart_after: false,
                    },
                ));
                break;
            }
        }
    }

    // ── 6. Bootstrap RunIfMissing steps from .dev-launcher.conf ──────────────
    let conf_path = repo_dir.join(".dev-launcher.conf");
    if let Some(manifest) = parse_dev_launcher_conf(&conf_path) {
        for step in &manifest.bootstrap {
            match step {
                BootstrapDef::Check { path, missing_hint } => {
                    if !repo_dir.join(path).exists() {
                        findings.push(Finding::info(
                            KIND_INFO_BOOTSTRAP_CHECK,
                            "Bootstrap check failed",
                            vec![format!("  {missing_hint}")],
                        ));
                    }
                }
                BootstrapDef::RunIfMissing { check, command, cwd } => {
                    if !repo_dir.join(check).exists() {
                        if let Some((prog, args)) = command.split_first() {
                            let work_dir = cwd.as_deref()
                                .map(|c| repo_dir.join(c))
                                .unwrap_or_else(|| repo_dir.to_path_buf());
                            let mut all_args = vec![prog.as_str()];
                            all_args.extend(args.iter().map(|s| s.as_str()));
                            findings.push(Finding::fixable(
                                KIND_BOOTSTRAP_RUN,
                                format!("Bootstrap step needed: {}", prog),
                                vec![format!("  Triggers when {} is missing", check)],
                                FixAction::Steps {
                                    label: format!("{}", command.join(" ")),
                                    steps: vec![FixStep::new(&all_args, &work_dir)],
                                    restart_after: false,
                                },
                            ));
                        }
                    }
                }
            }
        }
    }

    // ── 7. Recent log tail (always shown as context) ──────────────────────────
    let tail = tail_file(&svc.log_path, 20);
    if !tail.is_empty() {
        let body = tail.iter().map(|l| format!("  {DIM}{l}{R}")).collect();
        findings.push(Finding::info(KIND_INFO_LOG_TAIL, "Recent log output", body));
    }

    if findings.is_empty() {
        findings.push(Finding::info(
            KIND_INFO_NO_ISSUES,
            "No issues detected",
            vec![
                "  Service appears healthy.".into(),
                format!("  Log: {}", svc.log_path.display()),
            ],
        ));
    }

    findings
}

fn build_diagnose_lines(svc: &Svc, findings: &[Finding], cursor: usize) -> Vec<String> {
    let (cols, rows) = terminal_size();
    let header  = 4usize;
    let footer  = 2usize;
    let content = rows.saturating_sub(header + footer);
    let sep     = "─".repeat(cols.saturating_sub(4));

    let mut out = Vec::new();

    // ── Header ────────────────────────────────────────────────────────────────
    out.push(String::new());
    let mut hdr = format!("  {BOLD}{CYN}{}{R}  {BOLD}diagnosis{R}", svc.name);
    match &svc.health {
        Health::Crashed(c)  => hdr.push_str(&format!("  {RED}crashed ({c}){R}")),
        Health::Degraded(m) => hdr.push_str(&format!("  {RED}degraded{R}  {DIM}{m}{R}")),
        Health::Up          => hdr.push_str(&format!("  {GRN}up{R}")),
        other               => hdr.push_str(&format!("  {DIM}{}{R}", other.label_plain())),
    }
    out.push(hdr);
    out.push(format!("  {DIM}{}{R}", svc.log_path.display()));
    out.push(format!("  {DIM}{sep}{R}"));

    // ── Build finding blocks then paginate ────────────────────────────────────
    let mut lines: Vec<String> = Vec::new();
    for (i, f) in findings.iter().enumerate() {
        let marker = if i == cursor { format!("{CYN}{BOLD}▶{R} ") } else { "  ".to_string() };
        let check  = if f.resolved { format!("{GRN}✓{R}") }
                     else if f.fix.is_some() { format!("{YLW}●{R}") }
                     else { format!("{DIM}·{R}") };
        lines.push(format!("  {marker}{check}  {BOLD}{}{R}", f.title));
        for b in &f.body { lines.push(format!("       {b}")); }
        if let Some(fix) = &f.fix {
            if f.resolved {
                lines.push(format!("       {GRN}✓ Fixed{R}"));
            } else {
                lines.push(format!("       \x1b[1;38;5;214m→ Enter to run:\x1b[0m  {}", fix.label()));
            }
        } else if needs_recipe(f) {
            lines.push(format!("       {DIM}no recipe yet — press r to report{R}"));
        }
        lines.push(String::new());
    }

    let cursor_line = {
        let mut n = 0usize;
        for (i, f) in findings.iter().enumerate() {
            if i == cursor { break; }
            let extra = if f.fix.is_some() || needs_recipe(f) { 1 } else { 0 };
            n += 2 + f.body.len() + extra + 1;
        }
        n
    };
    let start = cursor_line.min(lines.len().saturating_sub(content));
    let end   = (start + content).min(lines.len());
    let page  = &lines[start..end];

    for line in page { out.push(line.clone()); }
    for _ in page.len()..content { out.push(String::new()); }

    // ── Footer ────────────────────────────────────────────────────────────────
    out.push(format!("  {DIM}{sep}{R}"));
    let fixable_count     = findings.iter().filter(|f| f.fix.is_some() && !f.resolved).count();
    let cursor_reportable = findings.get(cursor).map(|f| needs_recipe(f)).unwrap_or(false);
    if fixable_count > 0 && cursor_reportable {
        out.push(format!("  {DIM}↑↓ navigate   {R}{ENTER_RUN_FIX}{DIM}   r report   q / ← back{R}   {YLW}{fixable_count} fix(es) available{R}"));
    } else if fixable_count > 0 {
        out.push(format!("  {DIM}↑↓ navigate   {R}{ENTER_RUN_FIX}{DIM}   q / ← back{R}   {YLW}{fixable_count} fix(es) available{R}"));
    } else if cursor_reportable {
        out.push(format!("  {DIM}↑↓ navigate   r report missing recipe   q / ← back{R}"));
    } else {
        out.push(format!("  {DIM}↑↓ navigate   q / ← back{R}"));
    }
    out
}

// ── Render — log view ─────────────────────────────────────────────────────────

fn build_log_view_lines(svc: &Svc, scroll: usize, follow: bool) -> Vec<String> {
    let (cols, rows) = terminal_size();
    let header  = 4usize;
    let footer  = 2usize;
    let content = rows.saturating_sub(header + footer);
    let sep     = "─".repeat(cols.saturating_sub(4));

    let mut out = Vec::new();

    // ── Header ────────────────────────────────────────────────────────────────
    out.push(String::new());
    let mut hdr = format!("  {BOLD}{CYN}{}{R}", svc.name);
    match &svc.health {
        Health::Up          => hdr.push_str(&format!("  {GRN}up{R}")),
        Health::Running     => hdr.push_str(&format!("  {CYN}running{R}")),
        Health::Crashed(c)  => hdr.push_str(&format!("  {RED}crashed ({c}){R}")),
        Health::Degraded(m) => hdr.push_str(&format!("  {RED}degraded ({m}){R}")),
        other               => hdr.push_str(&format!("  {DIM}{}{R}", other.label_plain())),
    }
    if let Some(pid) = svc.pid { hdr.push_str(&format!("  {DIM}pid {pid}{R}")); }
    out.push(hdr);
    out.push(format!("  {DIM}{}{R}", svc.log_path.display()));
    out.push(format!("  {DIM}{sep}{R}"));

    // ── Content ───────────────────────────────────────────────────────────────
    let lines = tail_file(&svc.log_path, content + scroll + 300);
    let total = lines.len();

    let end   = total.saturating_sub(if follow { 0 } else { scroll });
    let start = end.saturating_sub(content);
    let page  = &lines[start..end];

    for line in page { out.push(format!("  {line}")); }
    for _ in page.len()..content { out.push(String::new()); }

    // ── Footer ────────────────────────────────────────────────────────────────
    out.push(format!("  {DIM}{sep}{R}"));
    let follow_label = if follow {
        format!("{GRN}following{R}")
    } else {
        format!("{YLW}paused{R}  {DIM}(f = follow){R}")
    };
    out.push(format!("  {DIM}q/← back   ↑↓ scroll   PgUp/PgDn fast   d diagnose{R}   {follow_label}"));
    out
}

// ── Render — shutdown ─────────────────────────────────────────────────────────

/// `pairs[i]` = (service_name, Option<proc_index_into_term_status>).
/// Services with `None` were already dead before shutdown was triggered.
fn render_shutdown(
    slug:        &str,
    pairs:       &[(String, Option<usize>)],
    term_status: &[TermStatus],
    elapsed:     Duration,
    timed_out:   bool,
) {
    let _ = disable_raw_mode();
    ensure_cooked_output();
    print!("\x1b[H\x1b[2J\r");
    print!("\r\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}  {YLW}{BOLD}shutting down…{R}\r\n\r\n");

    for (name, proc_j) in pairs {
        let status = match proc_j {
            None => format!("{DIM}already stopped{R}"),
            Some(j) => match &term_status[*j] {
                TermStatus::Terminating => format!("{YLW}terminating…{R}"),
                TermStatus::Stopped(0)  => format!("{GRN}stopped{R}"),
                TermStatus::Stopped(c)  => format!("{GRN}stopped ({c}){R}"),
                TermStatus::Killed      => format!("{RED}force killed{R}"),
            },
        };
        print!("  {:<26}{status}\r\n", name);
    }

    print!("\r\n");

    let pending = term_status.iter().filter(|s| **s == TermStatus::Terminating).count();
    if timed_out {
        print!("  {RED}Grace period exceeded — processes were force-killed.{R}\r\n");
    } else if pending == 0 {
        print!("  {GRN}{BOLD}All processes stopped.{R}\r\n");
    } else {
        print!("  {DIM}Waiting for {pending} process{}…  {}s{R}\r\n",
            if pending == 1 { "" } else { "es" }, elapsed.as_secs());
    }
    print!("\r\n");
    let _ = io::stdout().flush();
}

// ── Credentials overlay ───────────────────────────────────────────────────────

struct CredEntry {
    product: &'static str,
    label:   &'static str,
    value:   String,
}

/// Collect user-facing credentials from each product's workspace .env file.
fn gather_credentials(ws_env_dir: &Path, _paths: &Paths) -> Vec<CredEntry> {
    let mut out: Vec<CredEntry> = Vec::new();

    // Copilot — read from workspace copy (always up-to-date regardless of worktree).
    let copilot_env = ws_env_dir.join("copilot.env");
    if copilot_env.exists() {
        let map = parse_env_file(&copilot_env);
        for (key, label) in [
            ("ADMIN_EMAIL",    "Admin e-mail"),
            ("ADMIN_PASSWORD", "Admin password"),
        ] {
            if let Some(v) = map.get(key) {
                out.push(CredEntry { product: "Copilot", label, value: v.clone() });
            }
        }
    }

    // OpenCTI — read from workspace copy.
    let opencti_env = ws_env_dir.join("opencti.env");
    if opencti_env.exists() {
        let map = parse_env_file(&opencti_env);
        for (key, label) in [
            ("APP__ADMIN__EMAIL",    "Admin e-mail"),
            ("APP__ADMIN__PASSWORD", "Admin password"),
            ("APP__ADMIN__TOKEN",    "API token"),
        ] {
            if let Some(v) = map.get(key) {
                out.push(CredEntry { product: "OpenCTI", label, value: v.clone() });
            }
        }
    }

    // OpenAEV — read from workspace copy.
    let openaev_env = ws_env_dir.join("openaev.env");
    if openaev_env.exists() {
        let map = parse_env_file(&openaev_env);
        for (key, label) in [
            ("PGADMIN_USER",     "pgAdmin e-mail"),
            ("PGADMIN_PASSWORD", "pgAdmin password"),
        ] {
            if let Some(v) = map.get(key) {
                out.push(CredEntry { product: "OpenAEV", label, value: v.clone() });
            }
        }
    }

    // Connector — read from workspace copy.
    let connector_env = ws_env_dir.join("connector.env");
    if connector_env.exists() {
        let map = parse_env_file(&connector_env);
        if let Some(v) = map.get("OPENCTI_TOKEN") {
            out.push(CredEntry { product: "Connector", label: "OpenCTI token", value: v.clone() });
        }
    }

    out
}

fn build_credentials_lines(creds: &[CredEntry], slug: &str) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}  {BOLD}— credentials{R}\n"));

    let mut current_product = "";
    for entry in creds {
        if entry.product != current_product {
            current_product = entry.product;
            out.push(format!("  {BOLD}{current_product}{R}"));
            out.push(format!("  {DIM}{}{R}", "─".repeat(50)));
        }
        out.push(format!("  {:<24}{GRN}{}{R}", entry.label, entry.value));
    }

    if creds.is_empty() {
        out.push(format!("  {DIM}No .env files found. Run the stack at least once to generate them.{R}"));
    }

    out.push(String::new());
    out.push(format!("  {DIM}q/Esc back{R}"));
    out.push(String::new());
    out
}

// ── Low-level helpers ─────────────────────────────────────────────────────────

fn parse_compose_project_name(compose_file: &Path) -> Option<String> {
    let content = fs::read_to_string(compose_file).ok()?;
    for line in content.lines() {
        if !line.starts_with("name:") { continue; }
        let val = line["name:".len()..].trim()
            .trim_matches('"').trim_matches('\'').to_string();
        if !val.is_empty() { return Some(val); }
    }
    None
}

/// Build a workspace-scoped Docker project name: `{base}-{ws_hash[..8]}`.
/// This ensures each workspace gets its own isolated set of containers even
/// when the compose file has hardcoded `container_name:` directives.
fn ws_docker_project(base: &str, ws_hash: &str) -> String {
    format!("{}-{}", base, &ws_hash[..8.min(ws_hash.len())])
}

/// Parse a docker-compose file and return `(service_name, container_name)` pairs
/// for every service that has an explicit `container_name:` directive.
fn parse_compose_container_names(compose_file: &Path) -> Vec<(String, String)> {
    let content = match fs::read_to_string(compose_file) {
        Ok(c) => c, Err(_) => return Vec::new(),
    };
    let mut result    = Vec::new();
    let mut in_svcs   = false;
    let mut cur_svc   = String::new();

    for line in content.lines() {
        // Top-level `services:` section marker.
        if line == "services:" { in_svcs = true; continue; }
        if !in_svcs { continue; }

        // A line at exactly 2-space indent that ends with `:` and has no spaces
        // in the name is a service declaration.
        if let Some(rest) = line.strip_prefix("  ") {
            if !rest.starts_with(' ') && !rest.starts_with('#') {
                if let Some(svc) = rest.strip_suffix(':') {
                    if !svc.is_empty() && !svc.contains(' ') {
                        cur_svc = svc.to_string();
                        continue;
                    }
                }
            }
        }

        // `container_name:` at 4-space indent inside a service block.
        if !cur_svc.is_empty() {
            if let Some(rest) = line.strip_prefix("    container_name:") {
                let cn = rest.trim().trim_matches('"').trim_matches('\'').to_string();
                if !cn.is_empty() {
                    result.push((cur_svc.clone(), cn));
                }
            }
        }
    }
    result
}

/// Write a compose override file to `/tmp` that appends `{ws_hash[..8]}` to every
/// explicit `container_name:` in the given compose file.
///
/// Returns `None` if the compose file has no explicit container names (no override
/// needed in that case — Docker Compose auto-names already include the project name).
fn write_compose_override(compose_file: &Path, ws_hash: &str) -> Option<PathBuf> {
    let mappings = parse_compose_container_names(compose_file);
    if mappings.is_empty() { return None; }

    let suffix   = &ws_hash[..8.min(ws_hash.len())];
    let out_path = PathBuf::from(format!("/tmp/dev-feature-override-{suffix}.yml"));

    let mut lines = vec!["services:".to_string()];
    for (svc, cn) in &mappings {
        lines.push(format!("  {}:", svc));
        lines.push(format!("    container_name: {cn}-{suffix}"));
    }
    fs::write(&out_path, lines.join("\n") + "\n").ok()?;
    Some(out_path)
}

/// Stop and remove any containers whose name contains `name_fragment`, regardless
/// of which compose project they belong to.  Used as a straggler sweep after
/// `docker compose down` to catch containers started outside dev-feature.
fn docker_kill_by_name_fragment(name_fragment: &str) {
    let out = Command::new("docker")
        .args(["ps", "-a", "-q", "--filter", &format!("name={name_fragment}")])
        .stdin(Stdio::null()).stderr(Stdio::null()).output();
    let ids: Vec<String> = out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();
    for id in &ids {
        let _ = Command::new("docker")
            .args(["rm", "-f", id])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .status();
    }
}

fn split_health_url_parts(full: Option<&str>) -> (Option<String>, String) {
    match full {
        None => (None, String::new()),
        Some(url) => {
            if let Some(pos) = url.find("://") {
                let after = &url[pos + 3..];
                if let Some(slash) = after.find('/') {
                    (Some(url[..pos + 3 + slash].to_string()), url[pos + 3 + slash..].to_string())
                } else {
                    (Some(url.to_string()), String::new())
                }
            } else {
                (Some(url.to_string()), String::new())
            }
        }
    }
}

fn parse_dev_launcher_conf(path: &Path) -> Option<RepoManifest> {
    let content = fs::read_to_string(path).ok()?;
    let mut docker = ManifestDocker::default();
    let mut services: Vec<SvcDef> = Vec::new();
    let mut bootstrap: Vec<BootstrapDef> = Vec::new();

    // Current section state
    enum Section {
        None,
        Docker,
        Service,
        Bootstrap,
    }

    let mut section = Section::None;
    // Per-service accumulator
    let mut svc_name   = String::new();
    let mut svc_args: Vec<String> = Vec::new();
    let mut svc_cwd    = String::new();
    let mut svc_health: Option<String> = None;
    let mut svc_timeout: u64 = 30;
    let mut svc_req_docker = false;
    let mut svc_log: Option<String> = None;
    let mut svc_requires: Vec<String> = Vec::new();
    // Per-bootstrap accumulator
    let mut bs_check   = String::new();
    let mut bs_missing = String::new();
    let mut bs_run_if  = String::new();
    let mut bs_command: Vec<String> = Vec::new();
    let mut bs_cwd: Option<String> = None;

    let flush_service = |name: &str, args: &Vec<String>, cwd: &str,
                         health: &Option<String>, timeout: u64,
                         req_docker: bool, log: &Option<String>,
                         requires: &Vec<String>,
                         svcs: &mut Vec<SvcDef>| {
        if !name.is_empty() {
            svcs.push(SvcDef {
                name:            name.to_string(),
                args:            args.clone(),
                cwd:             cwd.to_string(),
                health:          health.clone(),
                timeout_secs:    timeout,
                requires_docker: req_docker,
                log_name:        log.clone(),
                requires:        requires.clone(),
            });
        }
    };

    let flush_bootstrap = |check: &str, missing: &str, run_if: &str,
                           command: &Vec<String>, cwd: &Option<String>,
                           bootstrap: &mut Vec<BootstrapDef>| {
        if !check.is_empty() && !missing.is_empty() {
            bootstrap.push(BootstrapDef::Check {
                path:         check.to_string(),
                missing_hint: missing.to_string(),
            });
        } else if !run_if.is_empty() && !command.is_empty() {
            bootstrap.push(BootstrapDef::RunIfMissing {
                check:   run_if.to_string(),
                command: command.clone(),
                cwd:     cwd.clone(),
            });
        }
    };

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { continue; }

        if line.starts_with('[') && line.ends_with(']') {
            // Flush previous section
            match &section {
                Section::Service => {
                    flush_service(&svc_name, &svc_args, &svc_cwd, &svc_health,
                                  svc_timeout, svc_req_docker, &svc_log, &svc_requires, &mut services);
                    svc_name = String::new(); svc_args = Vec::new();
                    svc_cwd = String::new(); svc_health = None;
                    svc_timeout = 30; svc_req_docker = false; svc_log = None;
                    svc_requires = Vec::new();
                }
                Section::Bootstrap => {
                    flush_bootstrap(&bs_check, &bs_missing, &bs_run_if,
                                    &bs_command, &bs_cwd, &mut bootstrap);
                    bs_check = String::new(); bs_missing = String::new();
                    bs_run_if = String::new(); bs_command = Vec::new(); bs_cwd = None;
                }
                _ => {}
            }

            let inner = line[1..line.len()-1].trim();
            if inner == "docker" {
                section = Section::Docker;
            } else if inner == "bootstrap" {
                section = Section::Bootstrap;
            } else if let Some(rest) = inner.strip_prefix("service ") {
                svc_name = rest.trim().to_string();
                section = Section::Service;
            } else {
                section = Section::None;
            }
            continue;
        }

        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().to_string();
            match &section {
                Section::Docker => match k {
                    "compose_dev" => docker.compose_dev = Some(v),
                    "project"     => docker.project     = Some(v),
                    _ => {}
                },
                Section::Service => match k {
                    "command"         => svc_args = v.split_whitespace().map(|s| s.to_string()).collect(),
                    "cwd"             => svc_cwd = v,
                    "health"          => svc_health = if v.is_empty() { None } else { Some(v) },
                    "timeout"         => svc_timeout = v.parse().unwrap_or(30),
                    "requires_docker" => svc_req_docker = matches!(v.as_str(), "true" | "1" | "yes"),
                    "log"             => svc_log = if v.is_empty() { None } else { Some(v) },
                    "requires"        => svc_requires = v.split_whitespace().map(|s| s.to_string()).collect(),
                    _ => {}
                },
                Section::Bootstrap => match k {
                    "check"          => bs_check   = v,
                    "missing"        => bs_missing  = v,
                    "run_if_missing" => bs_run_if   = v,
                    "command"        => bs_command  = v.split_whitespace().map(|s| s.to_string()).collect(),
                    "cwd"            => bs_cwd      = if v.is_empty() { None } else { Some(v) },
                    _ => {}
                },
                _ => {}
            }
        }
    }

    // Flush final section
    match &section {
        Section::Service => {
            flush_service(&svc_name, &svc_args, &svc_cwd, &svc_health,
                          svc_timeout, svc_req_docker, &svc_log, &svc_requires, &mut services);
        }
        Section::Bootstrap => {
            flush_bootstrap(&bs_check, &bs_missing, &bs_run_if,
                            &bs_command, &bs_cwd, &mut bootstrap);
        }
        _ => {}
    }

    Some(RepoManifest { docker, services, bootstrap })
}

fn infer_repo_manifest(repo_dir: &Path) -> RepoManifest {
    let mut docker = ManifestDocker::default();
    let mut services: Vec<SvcDef> = Vec::new();
    let mut bootstrap: Vec<BootstrapDef> = Vec::new();

    // Check for docker-compose.dev.yml
    let compose_file = repo_dir.join("docker-compose.dev.yml");
    if compose_file.exists() {
        docker.compose_dev = Some("docker-compose.dev.yml".to_string());
        docker.project = parse_compose_project_name(&compose_file)
            .or_else(|| {
                repo_dir.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| format!("{n}-dev"))
            });
    }

    // Check for Python backend
    let backend_dir = repo_dir.join("backend");
    let python = backend_dir.join(".venv/bin/python");
    if backend_dir.is_dir() && backend_dir.join("app/main.py").exists() {
        services.push(SvcDef {
            name:            "backend".to_string(),
            args:            vec![
                ".venv/bin/python".to_string(),
                "-m".to_string(), "uvicorn".to_string(),
                "app.main:application".to_string(),
                "--reload".to_string(),
                "--host".to_string(), "0.0.0.0".to_string(),
                "--port".to_string(), "8100".to_string(),
                "--timeout-graceful-shutdown".to_string(), "3".to_string(),
            ],
            cwd:             "backend".to_string(),
            health:          Some("http://localhost:8100/api/health".to_string()),
            timeout_secs:    120,
            requires_docker: true,
            log_name:        None,
            requires:        Vec::new(),
        });
        services.push(SvcDef {
            name:            "worker".to_string(),
            args:            vec![
                ".venv/bin/python".to_string(),
                "-m".to_string(), "saq".to_string(),
                "app.worker.settings".to_string(),
            ],
            cwd:             "backend".to_string(),
            health:          None,
            timeout_secs:    10,
            requires_docker: true,
            log_name:        Some("copilot-worker.log".to_string()),
            requires:        Vec::new(),
        });
        bootstrap.push(BootstrapDef::Check {
            path:         "backend/.venv/bin/python".to_string(),
            missing_hint: "Run ./dev.sh once to create the Python venv".to_string(),
        });
        let _ = python; // silence unused warning
    }

    // Check for frontend
    let frontend_dir = repo_dir.join("frontend");
    if frontend_dir.join("package.json").exists() {
        services.push(SvcDef {
            name:            "frontend".to_string(),
            args:            vec!["yarn".to_string(), "dev".to_string()],
            cwd:             "frontend".to_string(),
            health:          Some("http://localhost:3100".to_string()),
            timeout_secs:    90,
            requires_docker: false,
            log_name:        None,
            requires:        Vec::new(),
        });
        bootstrap.push(BootstrapDef::RunIfMissing {
            check:   "frontend/node_modules".to_string(),
            command: vec!["yarn".to_string(), "install".to_string()],
            cwd:     Some("frontend".to_string()),
        });
    }

    RepoManifest { docker, services, bootstrap }
}

fn save_dev_launcher_conf(conf_path: &Path, repo_name: &str, manifest: &RepoManifest) {
    let mut out = format!("# {} — dev-feature launcher configuration\n", repo_name);
    out.push_str("# Auto-generated. Edit to customize. Re-run dev-feature to apply changes.\n\n");

    out.push_str("[docker]\n");
    if let Some(ref cd) = manifest.docker.compose_dev {
        out.push_str(&format!("compose_dev = {}\n", cd));
    }
    if let Some(ref p) = manifest.docker.project {
        out.push_str(&format!("project     = {}\n", p));
    }
    out.push('\n');

    for svc in &manifest.services {
        out.push_str(&format!("[service {}]\n", svc.name));
        if !svc.args.is_empty() {
            out.push_str(&format!("command         = {}\n", svc.args.join(" ")));
        }
        if !svc.cwd.is_empty() {
            out.push_str(&format!("cwd             = {}\n", svc.cwd));
        }
        if let Some(ref h) = svc.health {
            out.push_str(&format!("health          = {}\n", h));
        }
        out.push_str(&format!("timeout         = {}\n", svc.timeout_secs));
        if svc.requires_docker {
            out.push_str("requires_docker = true\n");
        }
        if let Some(ref l) = svc.log_name {
            out.push_str(&format!("log             = {}\n", l));
        }
        out.push('\n');
    }

    for step in &manifest.bootstrap {
        out.push_str("[bootstrap]\n");
        match step {
            BootstrapDef::Check { path, missing_hint } => {
                out.push_str(&format!("check   = {}\n", path));
                out.push_str(&format!("missing = {}\n", missing_hint));
            }
            BootstrapDef::RunIfMissing { check, command, cwd } => {
                out.push_str(&format!("run_if_missing = {}\n", check));
                out.push_str(&format!("command        = {}\n", command.join(" ")));
                if let Some(ref c) = cwd {
                    out.push_str(&format!("cwd            = {}\n", c));
                }
            }
        }
        out.push('\n');
    }

    let _ = fs::write(conf_path, out);
}

fn load_repo_manifest(repo_dir: &Path, repo_name: &str) -> RepoManifest {
    let conf_path = repo_dir.join(".dev-launcher.conf");
    if conf_path.exists() {
        if let Some(m) = parse_dev_launcher_conf(&conf_path) { return m; }
    }
    let manifest = infer_repo_manifest(repo_dir);
    if !manifest.services.is_empty() {
        println!("  {DIM}Auto-generating .dev-launcher.conf for {repo_name}…{R}");
        save_dev_launcher_conf(&conf_path, repo_name, &manifest);
    }
    manifest
}

/// Patch in-memory manifest port numbers to match the workspace env's BASE_URL /
/// FRONTEND_URL.  The cached .dev-launcher.conf stores the dev-feature default ports
/// (8100 / 3100); this replaces them with whatever the user actually configured.
fn patch_manifest_ports(manifest: &mut RepoManifest, backend_port: u16, frontend_port: u16) {
    const DEFAULT_BACKEND:  u16 = 8100;
    const DEFAULT_FRONTEND: u16 = 3100;
    for svc in &mut manifest.services {
        let (from, to) = match svc.name.as_str() {
            "backend"  => (DEFAULT_BACKEND,  backend_port),
            "frontend" => (DEFAULT_FRONTEND, frontend_port),
            _          => continue,
        };
        if from == to { continue; }
        if let Some(ref mut h) = svc.health {
            *h = h.replace(&format!(":{from}"), &format!(":{to}"));
        }
        for arg in &mut svc.args {
            if arg == &from.to_string() { *arg = to.to_string(); }
        }
    }
}

/// Derive the base Docker project name for a repo (without workspace suffix).
fn resolve_docker_project_base(repo_dir: &Path, manifest: &RepoManifest) -> String {
    if let Some(p) = &manifest.docker.project { return p.clone(); }
    let compose_file = manifest.docker.compose_dev.as_deref().unwrap_or("docker-compose.dev.yml");
    if let Some(name) = parse_compose_project_name(&repo_dir.join(compose_file)) { return name; }
    repo_dir.file_name().and_then(|n| n.to_str()).map(|n| format!("{n}-dev")).unwrap_or_else(|| "dev".to_string())
}

/// Full workspace-scoped Docker project name: `{base}-{ws_hash[..8]}`.
fn resolve_docker_project(repo_dir: &Path, manifest: &RepoManifest, ws_hash: &str) -> String {
    ws_docker_project(&resolve_docker_project_base(repo_dir, manifest), ws_hash)
}

/// Resolve the workspace-scoped docker project name and compose file path for a
/// product during workspace *removal* — read-only, never auto-generates config.
///
/// Returns `(ws_project, base_project, compose_file)` so the caller can run
/// `compose down` with the workspace-scoped name and fall back to the base name
/// for containers that predate the workspace-isolation change.
///
/// Returns `None` when the product shares another product's docker (connectors).
fn resolve_product_docker_for_down(
    repo:     &str,
    repo_dir: &Path,
    ws_hash:  &str,
) -> Option<(String, String, PathBuf)> {
    // Connector shares OpenCTI docker — caller handles it separately.
    if repo == "connectors" { return None; }

    // OpenCTI: hardcoded compose location.
    if repo == "opencti" {
        let compose  = repo_dir.join("opencti-platform/opencti-dev/docker-compose.yml");
        let base     = "opencti-dev".to_string();
        let ws_proj  = ws_docker_project(&base, ws_hash);
        return Some((ws_proj, base, compose));
    }

    // OpenAEV: compose lives under openaev-dev/
    if repo == "openaev" {
        let dev_dir  = repo_dir.join("openaev-dev");
        let compose  = dev_dir.join("docker-compose.yml");
        let conf     = parse_dev_launcher_conf(&repo_dir.join(".dev-launcher.conf")).unwrap_or_default();
        let base     = conf.docker.project.unwrap_or_else(|| "openaev-dev".to_string());
        let ws_proj  = ws_docker_project(&base, ws_hash);
        return Some((ws_proj, base, compose));
    }

    // All other repos (copilot, etc.): try .dev-launcher.conf then docker-compose.dev.yml.
    let conf_path = repo_dir.join(".dev-launcher.conf");
    let manifest  = parse_dev_launcher_conf(&conf_path).unwrap_or_default();

    let compose_name = manifest.docker.compose_dev.as_deref().unwrap_or("docker-compose.dev.yml");
    let compose_file = repo_dir.join(compose_name);

    let base = if let Some(p) = manifest.docker.project {
        p
    } else if let Some(name) = parse_compose_project_name(&compose_file) {
        name
    } else {
        repo_dir.file_name()
            .and_then(|n| n.to_str())
            .map(|n| format!("{n}-dev"))
            .unwrap_or_else(|| "dev".to_string())
    };

    let ws_proj = ws_docker_project(&base, ws_hash);
    Some((ws_proj, base, compose_file))
}

fn run_manifest_bootstrap(repo_dir: &Path, manifest: &RepoManifest) -> bool {
    let mut ok = true;
    for step in &manifest.bootstrap {
        match step {
            BootstrapDef::Check { path, missing_hint } => {
                if !repo_dir.join(path).exists() {
                    println!("  {YLW}⚠{R}  {missing_hint}");
                    ok = false;
                }
            }
            BootstrapDef::RunIfMissing { check, command, cwd } => {
                if !repo_dir.join(check).exists() {
                    let work_dir = cwd.as_deref().map(|c| repo_dir.join(c)).unwrap_or_else(|| repo_dir.to_owned());
                    if let Some((prog, args)) = command.split_first() {
                        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                        run_blocking(prog, &args_ref, &work_dir);
                    }
                }
            }
        }
    }
    ok
}

fn parse_env_file(path: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(f) = File::open(path) else { return out };
    for line in io::BufReader::new(f).lines().flatten() {
        let line = line.trim().to_string();
        if line.is_empty() || line.starts_with('#') { continue; }
        if let Some((k, v)) = line.split_once('=') {
            // Unescape \n sequences written by write_env_file (e.g. multi-line PEM).
            let v = v.trim_matches('"').trim_matches('\'').replace("\\n", "\n");
            out.insert(k.into(), v);
        }
    }
    out
}

fn open_log(path: &Path) -> File {
    OpenOptions::new().create(true).append(true).open(path)
        .unwrap_or_else(|_| panic!("cannot open log {}", path.display()))
}

fn run_blocking(program: &str, args: &[&str], dir: &Path) -> i32 {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .status()
        .ok()
        .and_then(|s| s.code())
        .unwrap_or(-1)
}

/// Like `run_blocking` but prints the full command, working directory, and
/// exit code so the operator can see exactly what is being executed.
/// stdout/stderr are inherited so Docker's own output streams through directly.
fn run_blocking_logged(program: &str, args: &[&str], dir: &Path) -> i32 {
    println!("    {DIM}$ {program} {args}{R}", args = args.join(" "));
    println!("    {DIM}  cwd: {}{R}", dir.display());
    let _ = io::stdout().flush();
    let code = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .status()
        .ok()
        .and_then(|s| s.code())
        .unwrap_or(-1);
    println!("    {DIM}  exit: {code}{R}");
    code
}

/// Returns true when the Docker daemon is reachable.
/// Parse the port number out of a URL like "http://localhost:4000/health".
fn extract_url_port(url: &str) -> Option<u16> {
    // Strip scheme, then take everything after the last ':'
    let after_scheme = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
    let host_port = after_scheme.split('/').next()?;  // drop path
    let port_str  = host_port.rsplit(':').next()?;
    port_str.parse().ok()
}

/// Returns None when the port is free, or Some(human-readable message + PIDs) when occupied.
/// Uses `lsof` to identify the conflicting process — macOS only but that's our target.
fn port_in_use(port: u16) -> Option<String> {
    let out = Command::new("lsof")
        .args(["-ti", &format!(":{port}")])
        .output().ok()?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let pids: Vec<&str> = raw.split_whitespace().collect();
    if pids.is_empty() { return None; }
    // Resolve PID → process name for a friendlier message.
    let procs: Vec<String> = pids.iter().filter_map(|pid| {
        Command::new("ps").args(["-p", pid, "-o", "comm="])
            .output().ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| format!("{} (PID {})", s.trim(), pid))
    }).collect();
    let desc = if procs.is_empty() { pids.join(", ") } else { procs.join(", ") };
    Some(format!("Port {port} already in use by {desc} — stop it then press R to retry"))
}

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// How many services in a compose project are currently running.
fn docker_compose_running_count(project: &str) -> usize {
    let out = Command::new("docker")
        .args(["compose", "-p", project, "ps", "--services", "--filter", "status=running"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// A Docker Compose project that was brought up by this session.
/// Stored at startup so the shutdown path can tear it down cleanly.
#[derive(Clone)]
struct DockerProject {
    label:         String,
    project:       String,
    compose_file:  PathBuf,
    work_dir:      PathBuf,
    override_file: Option<PathBuf>,
}

/// Run `docker compose -p <project> -f <file> [-f <override>] down`.
fn docker_compose_down(dp: &DockerProject) {
    print!("  Stopping {} containers…\r\n", dp.label);
    let _ = io::stdout().flush();
    let file_str = dp.compose_file.to_str().unwrap_or("");
    let ov_str   = dp.override_file.as_ref().and_then(|p| p.to_str()).unwrap_or("");
    let mut argv: Vec<&str> = vec!["compose", "-p", &dp.project, "-f", file_str];
    if !ov_str.is_empty() { argv.extend_from_slice(&["-f", ov_str]); }
    argv.push("down");
    let code = run_blocking("docker", &argv, &dp.work_dir);
    if code == 0 {
        print!("  {GRN}✓{R}  {} containers stopped.\r\n", dp.label);
    } else {
        print!("  {RED}✗{R}  {} docker down failed (exit {code}).\r\n", dp.label);
    }
    let _ = io::stdout().flush();
}

/// Run `docker compose -p <project> -f <file> up -d [extra…]`.
/// Prints a one-line status and returns whether the command succeeded.
fn docker_compose_up(label: &str, project: &str, compose_file: &Path, work_dir: &Path, extra: &[&str]) -> bool {
    let running_before = docker_compose_running_count(project);
    let file_str = compose_file.to_str().unwrap();
    let mut argv: Vec<&str> = vec!["compose", "-p", project, "-f", file_str];
    argv.extend_from_slice(extra);
    argv.extend_from_slice(&["up", "-d", "--remove-orphans"]);
    let code = run_blocking("docker", &argv, work_dir);
    if code == 0 {
        let running_after = docker_compose_running_count(project);
        let started = running_after.saturating_sub(running_before);
        if started == 0 {
            println!("  {GRN}✓{R}  {label} docker deps already up ({running_after} containers)");
        } else {
            println!("  {GRN}✓{R}  {label} docker deps started ({started} new, {running_after} total)");
        }
        true
    } else {
        // Compose can fail when containers with explicit `container_name` directives
        // already exist under a different project label (e.g. from `./dev.sh`, or
        // from a session before workspace-scoped naming was introduced).
        // Check via the Docker project label — exact project name — before failing hard.
        let label_already_up = Command::new("docker")
            .args(["ps", "-q", "--filter", &format!("label=com.docker.compose.project={project}")])
            .stdin(Stdio::null()).stderr(Stdio::null()).output()
            .ok().and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        if label_already_up > 0 {
            println!("  {GRN}✓{R}  {label} docker deps already up ({label_already_up} containers)");
            return true;
        }
        println!("  {RED}✗{R}  {label} docker deps failed (exit {code}) — services depending on them will degrade");
        false
    }
}

fn spawn_svc(
    program: &str,
    args: &[&str],
    dir: &Path,
    extra_env: &HashMap<String, String>,
    log: &Path,
) -> io::Result<(Child, i32)> {
    let log_out = open_log(log);
    let log_err = log_out.try_clone()?;
    let mut cmd = Command::new(program);
    cmd .args(args)
        .current_dir(dir)
        .envs(extra_env)
        .stdin(Stdio::null())
        .stdout(log_out)
        .stderr(log_err);
    cmd.process_group(0);
    let child = cmd.spawn()?;
    let pgid  = child.id() as i32;
    Ok((child, pgid))
}

fn probe(url: &str) -> bool {
    match ureq::get(url).timeout(Duration::from_secs(2)).call() {
        Ok(r)                              => r.status() < 500,
        Err(ureq::Error::Status(code, _)) => code < 500, // 4xx = server is up, auth/not-found
        Err(_)                            => false,       // connection refused / timeout
    }
}

/// Before spawning opencti-graphql, check whether Elasticsearch already has
/// OpenCTI indices from a previous session (partial or complete initialisation).
///
/// If any `opencti*` indices are found they are deleted so that opencti-graphql
/// can perform a clean `[INIT]` on startup.  Without this wipe, OpenCTI crashes
/// with "index already exists" whenever the ES volume survives a previous run —
/// because OpenCTI's init code calls `PUT /{index}` (create) unconditionally and
/// does not handle the 400 "already exists" response.
///
/// The wipe is safe in a dev environment: OpenCTI re-imports all data on init,
/// and the ES volume for this workspace is scoped to this workspace's hash anyway.
///
/// Skipped silently when ES is not yet responding (first-ever launch, Docker still
/// starting) — no harm done, the volume is empty in that case.
fn wipe_opencti_es_indices_if_stale(es_port: u16) {
    let cat_url = format!("http://localhost:{es_port}/_cat/indices?h=index");
    println!("  {DIM}[ES pre-flight] querying {cat_url}{R}");
    let _ = io::stdout().flush();

    let resp = match ureq::get(&cat_url).timeout(Duration::from_secs(2)).call() {
        Ok(r) => {
            println!("  {DIM}[ES pre-flight] ES responded (HTTP {}){R}", r.status());
            r
        }
        Err(e) => {
            println!("  {DIM}[ES pre-flight] ES not reachable ({e}) — skipping index wipe{R}");
            return;
        }
    };

    let body = resp.into_string().unwrap_or_default();
    let all_indices: Vec<&str> = body.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    println!("  {DIM}[ES pre-flight] total indices: {}  {:?}{R}", all_indices.len(), all_indices);

    let stale: Vec<String> = all_indices.iter()
        .filter(|l| l.starts_with("opencti"))
        .map(|l| l.to_string())
        .collect();

    if stale.is_empty() {
        println!("  {DIM}[ES pre-flight] no opencti* indices — nothing to wipe{R}");
        return;
    }

    println!("  {YLW}⚠{R}  ES has {} stale OpenCTI index(es) — wiping for clean init:", stale.len());
    for idx in &stale {
        let url = format!("http://localhost:{es_port}/{idx}");
        print!("    {DIM}DELETE {url} … {R}");
        let _ = io::stdout().flush();
        match ureq::request("DELETE", &url).timeout(Duration::from_secs(5)).call() {
            Ok(r)                              => println!("{GRN}{}{R}", r.status()),
            Err(ureq::Error::Status(404, _))   => println!("{DIM}404 already gone{R}"),
            Err(ureq::Error::Status(code, _))  => println!("{RED}HTTP {code}{R}"),
            Err(e)                             => println!("{RED}error: {e}{R}"),
        }
    }
}


// ── LLM helpers ───────────────────────────────────────────────────────────────

/// Escape a string for JSON — wraps in quotes and escapes special chars.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"'  => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {},
            c    => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Extract the first JSON string value after `"key":` in a flat response body.
fn extract_json_string(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\":", key);
    let start  = body.find(&needle)? + needle.len();
    let rest   = body[start..].trim_start();
    if !rest.starts_with('"') { return None; }
    let inner  = &rest[1..];
    let mut out = String::new();
    let mut chars = inner.chars();
    loop {
        match chars.next()? {
            '"'  => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                c   => out.push(c),
            },
            c    => out.push(c),
        }
    }
}

/// Call the Anthropic Messages API (`{base_url}/messages`) and return the first text block.
fn call_anthropic(cfg: &LlmConfig, prompt: &str) -> Option<String> {
    let url  = format!("{}/messages", cfg.base_url);
    let body = format!(
        "{{\"model\":{},\"max_tokens\":256,\"messages\":[{{\"role\":\"user\",\"content\":{}}}]}}",
        json_string(&cfg.model),
        json_string(prompt),
    );
    let resp = ureq::post(&url)
        .set("x-api-key", &cfg.api_key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .timeout(Duration::from_secs(15))
        .send_string(&body)
        .ok()?;
    let text = resp.into_string().ok()?;
    // Parse: {"content":[{"type":"text","text":"..."}], ...}
    extract_json_string(&text, "text")
}

/// Call an OpenAI-compatible Chat Completions endpoint (`{base_url}/chat/completions`).
/// Works with OpenAI, Ollama, LiteLLM, Azure OpenAI, Mistral, and any compatible provider.
fn call_openai_compatible(cfg: &LlmConfig, prompt: &str) -> Option<String> {
    let url  = format!("{}/chat/completions", cfg.base_url);
    let body = format!(
        "{{\"model\":{},\"max_tokens\":256,\"messages\":[{{\"role\":\"user\",\"content\":{}}}]}}",
        json_string(&cfg.model),
        json_string(prompt),
    );
    let mut req = ureq::post(&url)
        .set("content-type", "application/json")
        .timeout(Duration::from_secs(15));
    if !cfg.api_key.is_empty() {
        req = req.set("authorization", &format!("Bearer {}", cfg.api_key));
    }
    let resp = req.send_string(&body).ok()?;
    let text = resp.into_string().ok()?;
    // Parse: {"choices":[{"message":{"content":"..."}}], ...}
    let choices_pos = text.find("\"choices\"")?;
    extract_json_string(&text[choices_pos..], "content")
}

/// Ask the configured LLM to diagnose a crash based on the tail of the log.
fn llm_diagnose(cfg: &LlmConfig, log_tail: &str) -> Option<String> {
    let prompt = format!(
        "You are a dev-tools assistant. A local development service crashed. \
         The last lines of its log are below. In one short sentence (max 120 chars), \
         state the most likely cause and the single best fix. \
         Be direct — no preamble.\n\nLog tail:\n{log_tail}"
    );
    match cfg.provider {
        LlmProvider::Anthropic       => call_anthropic(cfg, &prompt),
        LlmProvider::OpenAICompatible => call_openai_compatible(cfg, &prompt),
    }
}

// ── Diagnosis helpers ─────────────────────────────────────────────────────────

/// Scan the last 200 lines of a log for a known pattern.
/// Returns the human-readable reason for the first match, or `None`.
fn check_diag_patterns(log_path: &Path) -> Option<String> {
    let lines = tail_file(log_path, 200);
    for line in &lines {
        let lower = line.to_lowercase();
        for (needle, reason) in DIAG_PATTERNS {
            if lower.contains(needle) {
                return Some(reason.to_string());
            }
        }
    }
    None
}

/// Diagnose a crash: pattern match first; fall back to LLM if no pattern found.
fn diagnose_crash(log_path: &Path, llm: Option<&LlmConfig>) -> Option<String> {
    // 1. Known-pattern fast path.
    if let Some(reason) = check_diag_patterns(log_path) {
        return Some(reason);
    }
    // 2. LLM fallback — only if configured.
    let cfg = llm?;
    let tail = tail_file(log_path, 60).join("\n");
    if tail.trim().is_empty() { return None; }
    llm_diagnose(cfg, &tail)
}

// ── LLM config resolution ─────────────────────────────────────────────────────

/// Resolve the LLM config.
///
/// Priority for each field:
///   api_key  : config file → `FILIGRAN_LLM_KEY` env var (empty = LLM disabled)
///   url      : config file → inferred from provider/key
///   provider : config file → inferred from url → inferred from key prefix
///   model    : config file → provider default
///
/// Provider inference rules (when not explicitly set):
///   - URL contains "anthropic"       → Anthropic
///   - URL set (other)                → OpenAICompatible
///   - No URL, key starts "sk-ant-"   → Anthropic (default URL)
///   - No URL, other key              → OpenAICompatible (default URL)
fn resolve_llm_config(dev_cfg: Option<&DevConfig>) -> Option<LlmConfig> {
    // API key — empty string is treated as "not set".
    let api_key = dev_cfg
        .and_then(|c| c.llm_api_key.as_deref())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("FILIGRAN_LLM_KEY").ok())
        .unwrap_or_default();

    // URL is required when no key is set (allows keyless local providers like Ollama).
    let custom_url = dev_cfg.and_then(|c| c.llm_url.as_deref());
    if api_key.trim().is_empty() && custom_url.is_none() { return None; }

    // Explicit provider hint from config.
    let provider_hint = dev_cfg.and_then(|c| c.llm_provider.as_deref());

    // Determine provider: explicit > URL > key prefix > default OpenAICompatible.
    let is_anthropic = match provider_hint {
        Some(p) => p.eq_ignore_ascii_case("anthropic"),
        None    => custom_url.map(|u| u.contains("anthropic")).unwrap_or(false)
                   || (custom_url.is_none() && api_key.starts_with("sk-ant-")),
    };

    let (provider, default_url, default_model) = if is_anthropic {
        (LlmProvider::Anthropic,        "https://api.anthropic.com/v1", "claude-haiku-4-5-20251001")
    } else {
        (LlmProvider::OpenAICompatible, "https://api.openai.com/v1",    "gpt-4o-mini")
    };

    let base_url = custom_url.unwrap_or(default_url).trim_end_matches('/').to_string();

    let model = dev_cfg
        .and_then(|c| c.llm_model.as_deref())
        .unwrap_or(default_model)
        .to_string();

    Some(LlmConfig { provider, api_key, model, base_url })
}

// ── Persistent configuration ──────────────────────────────────────────────────

struct DevConfig {
    workspace_root: PathBuf,
    /// API key sent to the provider. Can also be set via FILIGRAN_LLM_KEY env var.
    /// May be empty for local providers like Ollama that require no authentication.
    llm_api_key:    Option<String>,
    /// Base URL of the LLM provider, e.g.:
    ///   https://api.anthropic.com/v1          (Anthropic — default when key starts sk-ant-)
    ///   https://api.openai.com/v1             (OpenAI — default for other keys)
    ///   http://localhost:4000/v1              (LiteLLM proxy)
    ///   http://localhost:11434/v1             (Ollama)
    ///   https://<endpoint>.openai.azure.com/openai/deployments/<name>
    llm_url:        Option<String>,
    /// Force provider format: "anthropic" or "openai" (auto-inferred from URL when omitted).
    llm_provider:   Option<String>,
    /// Model name override — defaults to claude-haiku-4-5-20251001 (Anthropic) or gpt-4o-mini.
    llm_model:      Option<String>,
}

/// `~/.dev-launcher/config`
fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".dev-launcher/config")
}

/// Expand a leading `~/` to the real home directory.
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(s)
    }
}

fn load_config() -> Option<DevConfig> {
    let path = config_path();
    if !path.exists() { return None; }
    let map = parse_env_file(&path);
    let root_str = map.get("workspace_root")?;
    let root = expand_tilde(root_str);
    if root.is_dir() {
        Some(DevConfig {
            workspace_root: root,
            llm_api_key:  map.get("llm_api_key").cloned(),
            llm_url:      map.get("llm_url").cloned(),
            llm_provider: map.get("llm_provider").cloned(),
            llm_model:    map.get("llm_model").cloned(),
        })
    } else {
        // Saved path is stale — fall through to wizard.
        println!("  {YLW}⚠{R}  Config workspace_root no longer exists: {}", root.display());
        println!("  {DIM}(saved in {}){R}", config_path().display());
        println!();
        None
    }
}

fn save_config(config: &DevConfig) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut content = format!("workspace_root={}\n", config.workspace_root.display());
    if let Some(k) = &config.llm_api_key  { content.push_str(&format!("llm_api_key={k}\n")); }
    if let Some(u) = &config.llm_url      { content.push_str(&format!("llm_url={u}\n")); }
    if let Some(p) = &config.llm_provider { content.push_str(&format!("llm_provider={p}\n")); }
    if let Some(m) = &config.llm_model    { content.push_str(&format!("llm_model={m}\n")); }
    let _ = fs::write(&path, content);
}

// ── Repository registry ───────────────────────────────────────────────────────

/// Default registry embedded at compile time from `repos.conf` next to Cargo.toml.
/// Users can override by placing their own copy at `~/.dev-launcher/repos.conf`.
const DEFAULT_REPOS_CONF: &str = include_str!("../repos.conf");

#[derive(Clone)]
struct RepoEntry {
    /// Local directory name (used as clone destination and worktree base).
    dir:   String,
    label: String,
    url:   String,
    group: String,
}

fn repos_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".dev-launcher/repos.conf")
}

/// Parse an INI-style repos.conf into a list of `RepoEntry` values.
/// Sections: `[dir-name]`; fields: `label`, `url`, `group`.
fn parse_repos_conf(content: &str) -> Vec<RepoEntry> {
    let mut entries: Vec<RepoEntry> = Vec::new();
    let mut dir   = String::new();
    let mut label = String::new();
    let mut url   = String::new();
    let mut group = String::new();

    let flush = |dir: &str, label: &str, url: &str, group: &str, out: &mut Vec<RepoEntry>| {
        if !dir.is_empty() && !url.is_empty() {
            out.push(RepoEntry {
                dir:   dir.to_string(),
                label: if label.is_empty() { dir.to_string() } else { label.to_string() },
                url:   url.to_string(),
                group: group.to_string(),
            });
        }
    };

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if line.starts_with('[') && line.ends_with(']') {
            flush(&dir, &label, &url, &group, &mut entries);
            dir   = line[1..line.len()-1].trim().to_string();
            label = String::new();
            url   = String::new();
            group = String::new();
        } else if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "label" => label = v.trim().to_string(),
                "url"   => url   = v.trim().to_string(),
                "group" => group = v.trim().to_string(),
                _ => {}
            }
        }
    }
    flush(&dir, &label, &url, &group, &mut entries);
    entries
}

/// Load repo registry: user override (`~/.dev-launcher/repos.conf`) or embedded default.
fn load_repos() -> Vec<RepoEntry> {
    let user_path = repos_config_path();
    if user_path.exists() {
        if let Ok(content) = fs::read_to_string(&user_path) {
            let entries = parse_repos_conf(&content);
            if !entries.is_empty() { return entries; }
        }
    }
    parse_repos_conf(DEFAULT_REPOS_CONF)
}

// ── Clone selector TUI ────────────────────────────────────────────────────────

struct CloneChoice {
    entry:   RepoEntry,
    /// Selected for cloning.
    enabled: bool,
    /// Already present on disk — shown as cloned, not toggleable.
    present: bool,
}

fn build_clone_selector_lines(dest: &Path, choices: &[CloneChoice], cursor: usize) -> Vec<String> {
    let sep = "─".repeat(70);
    let mut out = Vec::new();
    out.push(format!("\n  {BOLD}{CYN}{BUILD_VERSION}  —  clone repositories{R}\n"));
    out.push(format!("  {DIM}Destination: {}{R}", dest.display()));
    out.push(format!("\n  {DIM}{sep}{R}"));
    out.push(format!("  {BOLD}Select repositories to clone{R}"));
    out.push(format!("  {DIM}{sep}{R}\n"));

    let mut last_group = "";
    for (i, c) in choices.iter().enumerate() {
        if c.entry.group != last_group && !c.entry.group.is_empty() {
            if i > 0 { out.push(String::new()); }
            out.push(format!("  {DIM}{}{R}", c.entry.group));
            last_group = &c.entry.group;
        }

        let marker = if i == cursor { format!("{CYN}{BOLD}▶{R} ") } else { "  ".to_string() };

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

        out.push(format!("  {marker}{checkbox}  {name}  {DIM}{}{R}", c.entry.url));
    }

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!("  {DIM}↑↓ / j k  navigate   Space toggle   a all   n none   Enter clone   q skip{R}"));
    out.push(String::new());
    out
}

/// Interactive clone selector. Returns `true` if the user confirmed, `false` if skipped.
fn run_clone_selector(dest: &Path, choices: &mut Vec<CloneChoice>) -> bool {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 { return false; }
    let mut raw = TuiGuard::enter();
    let mut cursor = 0usize;
    if let Some(first) = choices.iter().position(|c| !c.present) { cursor = first; }
    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(tui, &build_clone_selector_lines(dest, choices, cursor));
        }
        if event::poll(Duration::from_millis(20)).unwrap_or(false) {
            let Ok(Event::Key(ke)) = event::read() else { continue; };
            match ke.code {
                KeyCode::Up   | KeyCode::Char('k') => { cursor = cursor.saturating_sub(1); }
                KeyCode::Down | KeyCode::Char('j') => { if cursor + 1 < choices.len() { cursor += 1; } }
                KeyCode::Char(' ') => {
                    if !choices[cursor].present {
                        choices[cursor].enabled = !choices[cursor].enabled;
                    }
                }
                // Select all missing
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    for c in choices.iter_mut() { if !c.present { c.enabled = true; } }
                }
                // Deselect all
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    for c in choices.iter_mut() { if !c.present { c.enabled = false; } }
                }
                KeyCode::Enter => { drop(raw.take()); return true; }
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                    drop(raw.take()); return false;
                }
                _ => {}
            }
        }
    }
}

/// Clone selected repos. Git output streams directly to the terminal.
fn clone_repos(dest: &Path, choices: &[CloneChoice]) {
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
            Ok(s) if s.success() =>
                println!("  {GRN}✓{R}  {} cloned\n", c.entry.label),
            Ok(s) =>
                println!("  {RED}✗{R}  {} failed (exit {})\n", c.entry.label, s.code().unwrap_or(-1)),
            Err(e) =>
                println!("  {RED}✗{R}  {} error: {e}\n", c.entry.label),
        }
    }
    println!("  {DIM}{sep}{R}\n");
}

/// First-run interactive wizard: ask for workspace root, optionally clone repos, persist.
fn run_config_wizard() -> PathBuf {
    let sep = "─".repeat(60);
    println!("\n  {BOLD}{CYN}{BUILD_VERSION}  —  first-run setup{R}\n");
    println!("  {DIM}{sep}{R}");
    println!("  Workspace root not configured.\n");
    println!("  Enter the directory that will contain:");
    println!("  {DIM}  filigran-copilot/   opencti/   connectors/   openaev/{R}\n");
    println!("  The directory can be new — repositories can be cloned for you.");
    println!();
    println!("  You can override this setting later with:");
    println!("  {DIM}  --workspace-root <path>{R}");
    println!("  {DIM}  FILIGRAN_WORKSPACE_ROOT=<path>{R}");
    println!("  {DIM}  edit {}{R}", config_path().display());
    println!("\n  {DIM}{sep}{R}\n");

    loop {
        print!("  Workspace root path: ");
        let _ = io::stdout().flush();
        let input = match read_line_or_interrupt() {
            None => { println!("\n  {YLW}Aborted.{R}"); std::process::exit(0); }
            Some(s) => s,
        };
        let trimmed = input.trim();
        if trimmed.is_empty() { continue; }

        let candidate = expand_tilde(trimmed);

        // Create directory if it doesn't exist.
        if !candidate.exists() {
            print!("  Directory does not exist. Create it? {DIM}[Y/n]{R} ");
            let _ = io::stdout().flush();
            match read_line_or_interrupt() {
                None => { println!("\n  {YLW}Aborted.{R}"); std::process::exit(0); }
                Some(s) if matches!(s.trim().to_ascii_lowercase().as_str(), "n" | "no") => {
                    println!("  Skipped.");
                    continue;
                }
                _ => {
                    if let Err(e) = fs::create_dir_all(&candidate) {
                        println!("  {RED}✗{R}  Could not create directory: {e}");
                        continue;
                    }
                    println!("  {GRN}✓{R}  Created {}", candidate.display());
                }
            }
        }

        if !candidate.is_dir() {
            println!("  {RED}✗{R}  Not a directory: {}", candidate.display());
            continue;
        }

        // Offer to clone any repositories not yet present.
        let repos = load_repos();
        let mut clone_choices: Vec<CloneChoice> = repos.into_iter().map(|entry| {
            let present = candidate.join(&entry.dir).is_dir();
            CloneChoice { entry, enabled: !present, present }
        }).collect();

        let any_missing = clone_choices.iter().any(|c| !c.present);
        if any_missing {
            println!();
            println!("  {DIM}Some repositories are not yet present in this directory.{R}");
            if run_clone_selector(&candidate, &mut clone_choices) {
                let any_selected = clone_choices.iter().any(|c| c.enabled && !c.present);
                if any_selected {
                    clone_repos(&candidate, &clone_choices);
                }
            } else {
                println!("  {DIM}Cloning skipped — you can run git clone manually later.{R}\n");
            }
        }

        let cfg = DevConfig {
            workspace_root: candidate.clone(),
            llm_api_key: None, llm_url: None, llm_provider: None, llm_model: None,
        };
        save_config(&cfg);
        println!("  {GRN}✓{R}  Saved → {}", config_path().display());
        println!();
        return candidate;
    }
}

/// Resolve the workspace root. Priority:
///   1. `--workspace-root <path>` CLI flag
///   2. `FILIGRAN_WORKSPACE_ROOT` env var
///   3. `workspace_root` key in `~/.config/dev-feature/config`
///   4. Interactive first-run wizard (saves result to config file)
fn resolve_workspace_root(args: &Args) -> PathBuf {
    // 1. CLI flag
    if let Some(root) = &args.workspace_root {
        let root = if root.starts_with("~/") {
            expand_tilde(root.to_str().unwrap_or(""))
        } else {
            root.clone()
        };
        if root.is_dir() { return root; }
        eprintln!("--workspace-root '{}' is not a directory.", root.display());
        std::process::exit(1);
    }

    // 2. Env var
    if let Ok(raw) = std::env::var("FILIGRAN_WORKSPACE_ROOT") {
        let root = expand_tilde(raw.trim());
        if root.is_dir() { return root; }
        eprintln!("FILIGRAN_WORKSPACE_ROOT='{}' is not a directory.", root.display());
        std::process::exit(1);
    }

    // 3. Config file
    if let Some(cfg) = load_config() {
        return cfg.workspace_root;
    }

    // 4. First-run wizard
    run_config_wizard()
}

// ── Connector bootstrapping ───────────────────────────────────────────────────

fn ensure_connector_env(dir: &Path) -> PathBuf {
    let path = dir.join(".env.dev");
    if !path.exists() {
        let _ = fs::write(&path, "\
# Connector dev environment — fill in before running\n\
OPENCTI_URL=http://localhost:4000\n\
OPENCTI_TOKEN=ChangeMe\n\
CONNECTOR_TYPE=INTERNAL_IMPORT_FILE\n\
CONNECTOR_ID=54263257-26dc-4cca-8c45-deea44cdecf1\n\
CONNECTOR_NAME=ImportDocumentAI\n\
CONNECTOR_SCOPE=application/pdf,text/plain,text/html,text/markdown\n\
CONNECTOR_AUTO=false\n\
CONNECTOR_LOG_LEVEL=debug\n\
CONNECTOR_WEB_SERVICE_URL=https://importdoc.ariane.testing.filigran.io\n\
IMPORT_DOCUMENT_CREATE_INDICATOR=false\n\
IMPORT_DOCUMENT_INCLUDE_RELATIONSHIPS=true\n\
CONNECTOR_LICENCE_KEY_PEM=\n\
");
        println!("  {YLW}Created connector env template — edit before starting:{R}");
        println!("  {DIM}{}{R}\n", path.display());
    } else {
        // Patch files created before CONNECTOR_TYPE was added to the template.
        let mut env = parse_env_file(&path);
        if !env.contains_key("CONNECTOR_TYPE") {
            env.insert("CONNECTOR_TYPE".to_string(), "INTERNAL_IMPORT_FILE".to_string());
            write_env_file(&path, &env);
            println!("  {GRN}✓{R}  Added missing CONNECTOR_TYPE=INTERNAL_IMPORT_FILE to connector env");
        }
    }
    path
}

/// Path to a product's .env file inside the workspace directory.
/// e.g. `<ws_dir>/<hash>/opencti.env`
fn ws_env_path(ws_env_dir: &Path, product: &str) -> PathBuf {
    ws_env_dir.join(format!("{product}.env"))
}

/// Initialise a workspace .env file from the best available source.
///
/// Priority order:
///   1. Already exists in workspace → no-op.
///   2. Repo already has a populated .env (migration from old system) → copy it.
///   3. First matching template file (`.env.sample`, `.env.example`) → copy it.
///   4. Hardcoded template string → write it.
fn init_workspace_env(
    env_path:      &Path,
    repo_existing: Option<&Path>,
    template_srcs: &[PathBuf],
    hardcoded:     &str,
) {
    if env_path.exists() { return; }
    // Migration: if the repo already has a populated file, seed the workspace copy from it.
    if let Some(p) = repo_existing {
        if p.exists() {
            let map = parse_env_file(p);
            let has_real = map.values().any(|v| !v.is_empty() && v != "ChangeMe");
            if has_real {
                let _ = fs::copy(p, env_path);
                return;
            }
        }
    }
    // Fresh: copy from .env.sample / .env.example if present.
    for src in template_srcs {
        if src.exists() {
            let _ = fs::copy(src, env_path);
            return;
        }
    }
    // Fallback: write the hardcoded template.
    let _ = fs::write(env_path, hardcoded);
}

/// Copy a workspace .env file to its destination inside a repo worktree.
/// Creates parent directories if necessary; silently skips when `src` is absent.
/// Scan a docker-compose file for a port mapping that exposes `container_port`
/// and return the host-side port number.  Handles both quoted and unquoted forms:
///   - "6380:6379"
///   - 6380:6379
fn compose_host_port(compose_file: &Path, container_port: u16) -> Option<u16> {
    let content = fs::read_to_string(compose_file).ok()?;
    let suffix = format!(":{container_port}");
    for line in content.lines() {
        // Strip list dashes, quotes, and whitespace.
        let t = line.trim()
            .trim_start_matches('-').trim()
            .trim_matches('"').trim_matches('\'');
        if let Some(pos) = t.find(':') {
            let host_part = t[..pos].trim();
            let cont_part = t[pos + 1..].trim();
            if cont_part == container_port.to_string() {
                if let Ok(port) = host_part.parse::<u16>() {
                    return Some(port);
                }
            }
        }
        // Also handle the ":<container_port>" suffix form without a host prefix
        // (shouldn't appear in standard compose but be defensive).
        let _ = suffix.as_str();
    }
    None
}

/// Patch the `REDIS_URL` in a workspace env file to use the actual host port
/// exposed by docker-compose.  Called every launch so stale envs are fixed too.
/// Rewrite the port in a URL-like string ("host:port", "scheme://host:port", "host:port/path").
fn replace_port_in_value(value: &str, new_port: u16) -> String {
    if let Some(colon) = value.rfind(':') {
        let (base, rest) = value.split_at(colon);
        let after_colon = &rest[1..];
        let port_end   = after_colon.find('/').unwrap_or(after_colon.len());
        format!("{}:{}{}", base, new_port, &after_colon[port_end..])
    } else {
        format!("{}:{}", value, new_port)
    }
}

/// A single port-alignment check: one env key ↔ one docker-compose container port.
struct PortCheck {
    /// Human-readable label shown in the pre-flight output.
    label:          &'static str,
    /// Key in the env file whose value contains a port.
    env_key:        &'static str,
    /// Fallback value when the key is absent from the env file.
    default_value:  &'static str,
    /// The container-side port to look up in the compose file.
    container_port: u16,
}

/// Compare env port values against docker-compose host-port mappings and auto-correct
/// any mismatches.  Prints a line for every correction made so the user can see it.
fn preflight_port_checks(env_path: &Path, compose_file: &Path, checks: &[PortCheck]) {
    if !env_path.exists() || !compose_file.exists() { return; }
    let mut map     = parse_env_file(env_path);
    let mut changed = false;

    for c in checks {
        let Some(host_port) = compose_host_port(compose_file, c.container_port) else { continue };
        if host_port == c.container_port { continue; } // no remapping — nothing to fix

        let current = map.get(c.env_key).cloned().unwrap_or_else(|| c.default_value.to_string());
        let patched = replace_port_in_value(&current, host_port);
        if patched != current {
            println!("  {YLW}⚡{R}  {}: {} → {}  {DIM}(compose maps :{} → :{}){R}",
                c.label, current, patched, c.container_port, host_port);
            map.insert(c.env_key.to_string(), patched);
            changed = true;
        }
    }

    if changed {
        write_env_file(env_path, &map);
    }
}

/// Patch a URL key's port in the env file from `from_port` to `to_port`, but only
/// if the current value uses exactly `from_port` (i.e. the template default was never
/// changed by the user).  Prints one line per correction, like `preflight_port_checks`.
fn patch_url_default(env_path: &Path, key: &str, from_port: u16, to_port: u16) {
    if from_port == to_port || !env_path.exists() { return; }
    let mut map = parse_env_file(env_path);
    let Some(current) = map.get(key).cloned() else { return };
    if extract_url_port(&current) != Some(from_port) { return; }
    let patched = replace_port_in_value(&current, to_port);
    if patched == current { return; }
    println!("  {YLW}⚡{R}  {key}: {current} → {patched}  {DIM}(dev-feature port){R}");
    map.insert(key.to_string(), patched);
    write_env_file(env_path, &map);
}

/// Read a URL-valued key from the env file and return its port number,
/// falling back to `default` when the key is missing or has no parseable port.
fn read_env_url_port(env_path: &Path, key: &str, default: u16) -> u16 {
    if !env_path.exists() { return default; }
    parse_env_file(env_path)
        .get(key)
        .and_then(|v| extract_url_port(v))
        .unwrap_or(default)
}

fn deploy_workspace_env(src: &Path, dest: &Path) {
    if !src.exists() { return; }
    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::copy(src, dest);
}

fn ensure_connector_venv(dir: &Path) -> PathBuf {
    let venv = dir.join(".venv");
    if !venv.join("bin/python").exists() {
        println!("  Creating connector Python venv…");
        run_blocking("python3", &["-m", "venv", ".venv"], dir);
        let pip  = venv.join("bin/pip").to_string_lossy().into_owned();
        let reqs = dir.join("src/requirements.txt").to_string_lossy().into_owned();
        run_blocking(&pip, &["install", "-q", "-r", &reqs], dir);
    }
    venv
}

/// Ensure Corepack is enabled so that projects with `"packageManager": "yarn@4.x"`
/// use the correct Yarn version instead of the system's legacy Yarn 1.x.
///
/// `corepack enable` replaces the global `yarn` shim with one that reads
/// `packageManager` from each project's package.json and downloads/activates the
/// matching version on first use.  It is idempotent and fast when already enabled.
fn ensure_corepack() {
    let status = Command::new("corepack")
        .arg("enable")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("  Corepack enabled — yarn@4 shim active.");
        }
        _ => {
            println!("  {YLW}Could not enable Corepack — yarn@4 projects will fail to start.{R}");
            println!("  {DIM}Fix: npm install -g corepack   then re-run dev-feature{R}");
            println!("  {DIM}     (or: sudo corepack enable if node is installed system-wide){R}");
            println!();
        }
    }
}

/// Returns `Some(reason)` when the connector env file contains unfilled placeholder
/// values that would cause the connector to crash immediately on startup.
fn validate_connector_env(env: &HashMap<String, String>) -> Option<String> {
    let token = env.get("OPENCTI_TOKEN").map(|s| s.as_str()).unwrap_or("");
    if token.is_empty() || token == "ChangeMe" {
        return Some("OPENCTI_TOKEN not set — edit .env.dev before running".into());
    }
    None
}

/// Read POSTGRES_PASSWORD from a docker-compose YAML file by scanning for the key.
/// Returns None if the file can't be read or the key isn't found.
fn read_compose_postgres_password(compose_file: &Path) -> Option<String> {
    let content = fs::read_to_string(compose_file).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("POSTGRES_PASSWORD:") {
            let val = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !val.is_empty() { return Some(val); }
        }
    }
    None
}

/// Build the DATABASE_URL env var for the Copilot backend by reading the actual
/// POSTGRES_PASSWORD from docker-compose.dev.yml, so the backend can connect
/// regardless of what the config.py default says.
fn copilot_backend_env(copilot_dir: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let compose = copilot_dir.join("docker-compose.dev.yml");
    if let Some(password) = read_compose_postgres_password(&compose) {
        env.insert(
            "DATABASE_URL".into(),
            format!("postgresql+asyncpg://copilot:{password}@localhost:5432/copilot"),
        );
    }
    env
}

/// Ensure OpenCTI graphql's Python deps (eql, yara-python, pycti…) are installed.
///
/// On systems where pip3 cannot install globally (Homebrew-managed Python, PEP 668),
/// we create a project-local venv at `<graphql_dir>/.python-venv` and install there.
/// Returns a `PYTHONPATH` value that points at that venv's site-packages so the
/// Node.js → Python bridge can find the packages at runtime.
fn ensure_opencti_graphql_python_deps(dir: &Path) -> Option<String> {
    let venv = dir.join(".python-venv");
    let venv_python = venv.join("bin/python3");
    let reqs = dir.join("src/python/requirements.txt");
    if !reqs.exists() { return None; }

    // Create venv if it doesn't exist yet.
    if !venv_python.exists() {
        println!("  Creating OpenCTI graphql Python venv…");
        let ok = run_blocking("python3", &["-m", "venv", venv.to_str().unwrap()], dir);
        if ok != 0 {
            println!("  {YLW}Could not create Python venv — opencti-graphql may fail.{R}");
            return None;
        }
    }

    // Check if eql is already installed in the venv.
    let already = Command::new(venv_python.to_str().unwrap())
        .args(["-c", "import eql"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false);

    if !already {
        println!("  Installing OpenCTI graphql Python deps (eql, yara, pycti…)");
        let pip = venv.join("bin/pip3").to_string_lossy().into_owned();
        run_blocking(&pip, &["install", "-q", "-r", reqs.to_str().unwrap()], dir);
    }

    // Return the site-packages path so the caller can set PYTHONPATH.
    let site_packages = Command::new(venv_python.to_str().unwrap())
        .args(["-c", "import site; print(site.getsitepackages()[0])"])
        .stdin(Stdio::null()).stderr(Stdio::null())
        .output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    site_packages
}

fn ensure_copilot_fe_deps(dir: &Path) {
    if !dir.join("node_modules").is_dir() {
        println!("  Installing Copilot frontend deps…");
        run_blocking("yarn", &["install"], dir);
    }
}

fn ensure_opencti_fe_deps(dir: &Path) {
    if !dir.join("node_modules").is_dir() {
        println!("  Installing OpenCTI frontend deps…");
        run_blocking("yarn", &["install"], dir);
    }
}

fn ensure_openaev_fe_deps(dir: &Path) {
    if !dir.join("node_modules").is_dir() {
        println!("  Installing OpenAEV frontend deps…");
        run_blocking("yarn", &["install"], dir);
    }
}


/// Resolve the Maven executable: prefer `mvnw` wrapper in the repo root, fall back to `mvn`.
fn maven_cmd(openaev_root: &Path) -> String {
    let wrapper = openaev_root.join("mvnw");
    if wrapper.exists() {
        wrapper.to_string_lossy().into_owned()
    } else {
        "mvn".to_string()
    }
}

// ── Environment wizard ────────────────────────────────────────────────────────

/// A required variable that the wizard will prompt for when missing or placeholder.
struct EnvVar {
    key:         &'static str,
    /// Short label shown in the audit table.
    label:       &'static str,
    /// One-line hint shown below the variable name during prompting.
    hint:        &'static str,
    /// True → mask the current value in the audit table (tokens, certs, keys).
    secret:      bool,
    /// True → accept multiple lines until the user types END on its own line.
    multiline:   bool,
    /// True → generate a random UUID v4 when the user leaves the prompt blank.
    auto_uuid:   bool,
    /// True → generate 32 random bytes as base64 when the user leaves the prompt blank.
    auto_b64:    bool,
}

/// Variables required to boot OpenCTI for the first time.
/// `APP__ADMIN__TOKEN` doubles as the value you'll later paste into the connector as
/// `OPENCTI_TOKEN`, so knowing it up-front saves a second wizard run.
const OPENCTI_ENV_VARS: &[EnvVar] = &[
    EnvVar {
        key:       "APP__ADMIN__EMAIL",
        label:     "Admin e-mail",
        hint:      "Login e-mail for the built-in admin account (any valid address works)",
        secret:    false,
        multiline: false,
        auto_uuid: false,
        auto_b64:  false,
    },
    EnvVar {
        key:       "APP__ADMIN__PASSWORD",
        label:     "Admin password",
        hint:      "Password for the built-in admin account (anything except 'ChangeMe')",
        secret:    true,
        multiline: false,
        auto_uuid: false,
        auto_b64:  false,
    },
    EnvVar {
        key:       "APP__ADMIN__TOKEN",
        label:     "Admin API token (UUID)",
        hint:      "Leave blank to auto-generate — copy this value into OPENCTI_TOKEN for the connector",
        secret:    true,
        multiline: false,
        auto_uuid: true,
        auto_b64:  false,
    },
    EnvVar {
        key:       "APP__ENCRYPTION_KEY",
        label:     "Encryption key (base64)",
        hint:      "Leave blank to auto-generate — equivalent to: openssl rand -base64 32",
        secret:    true,
        multiline: false,
        auto_uuid: false,
        auto_b64:  true,
    },
];

/// Create `<graphql_dir>/.env.dev` from defaults if it doesn't exist yet.
fn ensure_opencti_env(gql_dir: &Path) {
    let path = gql_dir.join(".env.dev");
    if !path.exists() {
        let _ = fs::write(&path, "\
# OpenCTI graphql dev environment — generated by dev-feature\n\
# Leave TOKEN and ENCRYPTION_KEY as ChangeMe; the wizard will auto-generate them.\n\
APP__ADMIN__EMAIL=admin@opencti.io\n\
APP__ADMIN__PASSWORD=ChangeMe\n\
APP__ADMIN__TOKEN=ChangeMe\n\
APP__ENCRYPTION_KEY=ChangeMe\n\
");
    }
}

/// Focused wizard for the licence key only — used when the connector crashes
/// because CONNECTOR_LICENCE_KEY_PEM is absent/empty.  Token is excluded so the
/// user is not interrupted about a field they have already configured.
const CONNECTOR_LICENCE_VARS: &[EnvVar] = &[
    EnvVar {
        key:       "CONNECTOR_LICENCE_KEY_PEM",
        label:     "Filigran licence certificate (PEM)",
        hint:      "Paste the full -----BEGIN CERTIFICATE----- … -----END CERTIFICATE----- block",
        secret:    true,
        multiline: true,
        auto_uuid: false,
        auto_b64:  false,
    },
];

/// Variables that require real user-supplied values before the connector can start.
/// Variables with sensible defaults (OPENCTI_URL, CONNECTOR_ID, …) are omitted.
const CONNECTOR_ENV_VARS: &[EnvVar] = &[
    EnvVar {
        key:       "OPENCTI_TOKEN",
        label:     "OpenCTI API token",
        hint:      "Same value as APP__ADMIN__TOKEN set during OpenCTI setup",
        secret:    true,
        multiline: false,
        auto_uuid: false,
        auto_b64:  false,
    },
    EnvVar {
        key:       "CONNECTOR_LICENCE_KEY_PEM",
        label:     "Filigran licence certificate (PEM)",
        hint:      "Paste the full -----BEGIN CERTIFICATE----- … -----END CERTIFICATE----- block",
        secret:    true,
        multiline: true,
        auto_uuid: false,
        auto_b64:  false,
    },
];

/// Variables that need real values before the Copilot backend can start.
const COPILOT_ENV_VARS: &[EnvVar] = &[
    EnvVar {
        key:       "ADMIN_EMAIL",
        label:     "Admin e-mail",
        hint:      "Login e-mail for the built-in Copilot admin account",
        secret:    false,
        multiline: false,
        auto_uuid: false,
        auto_b64:  false,
    },
    EnvVar {
        key:       "ADMIN_PASSWORD",
        label:     "Admin password",
        hint:      "Password for the built-in admin account (anything except 'ChangeMe')",
        secret:    true,
        multiline: false,
        auto_uuid: false,
        auto_b64:  false,
    },
];

/// Read `n` random bytes from /dev/urandom.
fn rand_bytes(n: usize) -> Vec<u8> {
    use io::Read as _;
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf
}

/// Generate a random UUID v4.
fn gen_uuid() -> String {
    let mut b = rand_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant RFC 4122
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],b[1],b[2],b[3], b[4],b[5], b[6],b[7], b[8],b[9], b[10],b[11],b[12],b[13],b[14],b[15],
    )
}

/// Generate 32 random bytes encoded as base64 — suitable for APP__ENCRYPTION_KEY.
fn gen_base64_key() -> String {
    use std::fmt::Write as _;
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = rand_bytes(33); // 33 bytes → 44 base64 chars, no padding issues
    let mut out = String::with_capacity(44);
    for chunk in bytes[..33].chunks(3) {
        let n = match chunk.len() {
            3 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8 | chunk[2] as u32,
            2 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8,
            _ => (chunk[0] as u32) << 16,
        };
        let _ = write!(out, "{}{}{}{}", TABLE[(n >> 18 & 63) as usize] as char,
            TABLE[(n >> 12 & 63) as usize] as char,
            if chunk.len() > 1 { TABLE[(n >> 6 & 63) as usize] as char } else { '=' },
            if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Placeholder values that count as "not set".
fn is_placeholder(v: &str) -> bool {
    matches!(v.trim(), "" | "ChangeMe" | "changeme" | "TODO" | "CHANGEME")
}

/// Path to the user-global dev-feature preferences file.
/// Stores auto-generated values (emails, passwords, tokens) so they stay
/// consistent across workspaces on the same machine.
fn global_prefs_path() -> PathBuf {
    dirs_base_dir().join("defaults.env")
}

fn dirs_base_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".config").join("dev-feature"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/dev-feature-prefs"))
}

/// Generate a random password: 24 alphanumeric characters.
fn gen_password() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    rand_bytes(24).iter()
        .map(|b| CHARS[(b % CHARS.len() as u8) as usize] as char)
        .collect()
}

/// Auto-generate a single value for `v`.
///
/// Resolution order:
///  1. Global prefs file — reuse a previously generated (or user-set) value.
///  2. `auto_uuid`  → random UUID v4.
///  3. `auto_b64`   → 32-byte base64 key.
///  4. Key name contains "EMAIL" → `dev@dev.local`.
///  5. Anything else (password, token, secret …) → random 24-char password.
///
/// The generated value is written back to prefs so future workspaces reuse it.
fn auto_generate_value(v: &EnvVar, prefs: &mut HashMap<String, String>) -> String {
    // 1. Prefs hit
    if let Some(existing) = prefs.get(v.key) {
        if !is_placeholder(existing) {
            return existing.clone();
        }
    }
    // 2-5. Generate
    let generated = if v.auto_uuid {
        gen_uuid()
    } else if v.auto_b64 {
        gen_base64_key()
    } else if v.key.to_uppercase().contains("EMAIL") {
        "dev@dev.local".to_string()
    } else {
        gen_password()
    };
    // Persist to prefs so the next workspace gets the same value.
    prefs.insert(v.key.to_string(), generated.clone());
    generated
}

/// Auto-generate all `missing` variables, updating both `env` and the global
/// prefs file.  Prints one line per variable so the user can see what was set.
fn auto_generate_missing(
    env:        &mut HashMap<String, String>,
    missing:    &[&EnvVar],
    prefs_path: &Path,
) {
    let _ = fs::create_dir_all(prefs_path.parent().unwrap_or(Path::new(".")));
    let mut prefs = parse_env_file(prefs_path);

    for v in missing {
        let value = auto_generate_value(v, &mut prefs);
        let display = if v.secret {
            format!("{DIM}[generated]{R}")
        } else {
            format!("{DIM}{value}{R}")
        };
        println!("  {GRN}✓{R}  {:<38} {display}", v.key);
        env.insert(v.key.to_string(), value);
    }

    write_env_file(prefs_path, &prefs);
}

/// Rewrite `path` preserving comments and key ordering.
/// Actual newlines in values are escaped to `\n` so the file stays single-line per key.
fn write_env_file(path: &Path, env: &HashMap<String, String>) {
    let original = fs::read_to_string(path).unwrap_or_default();
    let mut written: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut lines: Vec<String> = Vec::new();

    for line in original.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            lines.push(line.to_string());
            continue;
        }
        if let Some((k, _)) = trimmed.split_once('=') {
            let k = k.trim();
            if let Some(val) = env.get(k) {
                lines.push(format!("{}={}", k, val.replace('\n', "\\n")));
                written.insert(k.to_string());
                continue;
            }
        }
        lines.push(line.to_string());
    }

    // Append keys that were not present in the original file.
    for (k, v) in env {
        if !written.contains(k.as_str()) {
            lines.push(format!("{}={}", k, v.replace('\n', "\\n")));
        }
    }

    let mut content = lines.join("\n");
    if !content.ends_with('\n') { content.push('\n'); }
    let _ = fs::write(path, content);
}

/// Read one line from stdin.
///
/// Returns `None` on:
/// - Ctrl+C  (SIGINT interrupts the blocking read → `Err(Interrupted)`)
/// - Ctrl+D  (EOF → `Ok(0)` bytes read)
/// - Any other I/O error
///
/// Returns `Some(line)` otherwise, with the trailing newline stripped.
fn read_line_or_interrupt() -> Option<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = unsafe {
            libc::read(libc::STDIN_FILENO, byte.as_mut_ptr() as *mut libc::c_void, 1)
        };
        if n <= 0 {
            return None;
        }
        match byte[0] {
            b'\n' => break,
            b'\r' => continue, // skip CR from CRLF terminals (ICRNL may produce double newline)
            b => buf.push(b),
        }
    }
    String::from_utf8(buf).ok().map(|s| s.trim_end().to_string())
}

/// Prompt the user for a single env variable value using a Ratatui text-area
/// overlay.  Returns `Some(value)` on confirm or `None` on abort (Esc / Ctrl+C).
///
/// For **single-line** vars: Enter confirms.
/// For **multiline** vars (e.g. PEM certs): Alt+Enter *or* Ctrl+D confirms —
/// the user can paste freely without needing a sentinel line.
fn read_value_tui(v: &EnvVar, step: usize, total: usize) -> Option<String> {
    use ratatui::{
        backend::CrosstermBackend,
        layout::{Constraint, Layout},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Paragraph},
        Terminal,
    };

    let _guard = TuiGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).ok()?;

    // Style the textarea.
    let mut ta = TextArea::default();
    let border_style = Style::default().fg(Color::Cyan);
    let input_title  = if v.multiline {
        format!(" {} — Alt+Enter or Ctrl+D to confirm ", v.key)
    } else {
        format!(" {} — Enter to confirm ", v.key)
    };
    ta.set_block(Block::default().borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(input_title.clone(), Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD))));
    ta.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    ta.set_style(Style::default());
    if !v.multiline {
        // Prevent newlines in single-line mode.
        ta.set_hard_tab_indent(false);
    }

    let abort_hint = "  Esc / Ctrl+C  abort";
    let footer_style = Style::default().fg(Color::DarkGray);

    loop {
        let term = &mut terminal;
        // Ignore draw errors (terminal too small, etc.)
        let _ = term.draw(|f| {
            let area  = f.area();
            let cols  = area.width  as usize;
            let rows  = area.height as usize;

            // Header: BUILD_VERSION + progress
            let hdr = Line::from(vec![
                Span::styled(
                    format!("  {}  —  env wizard  ({}/{})",
                        BUILD_VERSION, step, total),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
            ]);

            // Info lines: label + hint
            let info = vec![
                Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(v.label, Style::default().add_modifier(Modifier::BOLD)),
                ]),
                Line::from(vec![
                    Span::styled(
                        format!("  {}", v.hint),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
            ];

            // Textarea height: for multiline allocate up to 60% of rows, min 5.
            let ta_h = if v.multiline {
                ((rows as f32 * 0.6) as u16).max(5)
            } else {
                3
            };

            let footer_text = if v.multiline {
                format!("  Alt+Enter or Ctrl+D  confirm · {abort_hint}")
            } else {
                format!("  Enter  confirm · {abort_hint}")
            };

            let header_h: u16 = 2;
            let info_h:   u16 = (info.len() as u16) + 1; // +1 blank line
            let footer_h: u16 = 1;

            let chunks = Layout::vertical([
                Constraint::Length(header_h),
                Constraint::Length(info_h),
                Constraint::Length(ta_h),
                Constraint::Length(footer_h),
                Constraint::Min(0),
            ]).split(area);

            f.render_widget(Paragraph::new(hdr), chunks[0]);
            f.render_widget(Paragraph::new(info), chunks[1]);
            f.render_widget(&ta, chunks[2]);
            f.render_widget(
                Paragraph::new(footer_text).style(footer_style),
                chunks[3],
            );

            // Suppress unused warning
            let _ = cols;
        });

        if event::poll(Duration::from_millis(20)).unwrap_or(false) {
            let Ok(ev) = event::read() else { continue };
            let Event::Key(ke) = ev else { continue };

            // Confirm
            let is_confirm = if v.multiline {
                // Alt+Enter
                (ke.code == KeyCode::Enter && ke.modifiers.contains(KeyModifiers::ALT))
                // Ctrl+D
                || (ke.code == KeyCode::Char('d') && ke.modifiers.contains(KeyModifiers::CONTROL))
            } else {
                ke.code == KeyCode::Enter && ke.modifiers == KeyModifiers::NONE
            };
            if is_confirm {
                let value = ta.lines().join("\n");
                return Some(value);
            }

            // Abort
            let is_abort =
                ke.code == KeyCode::Esc
                || (ke.code == KeyCode::Char('c') && ke.modifiers.contains(KeyModifiers::CONTROL));
            if is_abort { return None; }

            // Pass everything else to the textarea.
            ta.input(ke);
        }
    }
}

/// Interactive env setup wizard for a single `.env` file.
///
/// Shows an audit table of `vars`, then prompts the user to fill in any that
/// are missing or still hold placeholder values.  Writes the result back to
/// `env_path`.  No-ops silently when stdin is not a TTY (CI / piped input).
/// Interactive platform-mode selector shown when Copilot runs standalone (no OpenCTI).
///
/// Reads the current PLATFORM_MODE from `env_path` and lets the user change it
/// with a single keypress before services are launched.  Writes back to the env
/// file if the selection differs from the current value.
fn run_platform_mode_selector(env_path: &Path, stopping: &Arc<AtomicBool>) {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 { return; }
    ensure_cooked_output();

    let mut map = parse_env_file(env_path);
    let current = map.get("PLATFORM_MODE").cloned().unwrap_or_else(|| "xtm_one".to_string());

    // (env value, display name, description)
    let options: &[(&str, &str, &str)] = &[
        ("xtm_one", "XTM One",         "open platform — XTM One UI, EE features via license"),
        ("copilot", "Filigran Copilot", "enterprise — Copilot UI, license required"),
        ("dev",     "Dev",              "Copilot UI + XTM One seeding (testing)"),
    ];

    let mut cursor = options.iter().position(|(v, _, _)| *v == current.as_str()).unwrap_or(0);

    // Number of terminal lines the menu block occupies:
    //   1 header + 1 blank + N options + 1 blank = N+3
    let block_lines = options.len() + 3;

    // Render the menu block.  Must be called in raw mode so \r\n is emitted
    // literally (no ONLCR double-processing).  The erase sequence is omitted
    // on the very first call; callers must print "\x1b[{block_lines}A\x1b[0J"
    // before re-rendering.
    let render_raw = |cur: usize| {
        print!("  {BOLD}Platform mode{R}  {DIM}↑↓  Enter to confirm  Esc to cancel{R}\r\n\r\n");
        for (i, (val, name, desc)) in options.iter().enumerate() {
            let (arrow, name_fmt) = if i == cur {
                (format!("{CYN}▸{R}"), format!("{BOLD}{CYN}{name}{R}"))
            } else {
                (" ".into(), format!("{DIM}{name}{R}"))
            };
            let cur_tag = if *val == current.as_str() && i != cur {
                format!("  {DIM}(current){R}")
            } else {
                String::new()
            };
            print!("  {arrow} {name_fmt}  {DIM}{desc}{R}{cur_tag}\r\n");
        }
        print!("\r\n");
        let _ = io::stdout().flush();
    };

    // Print the initial menu in cooked mode using plain println! so line endings
    // are handled normally, then switch to raw mode for key handling.
    println!("  {BOLD}Platform mode{R}  {DIM}↑↓  Enter to confirm  Esc to cancel{R}");
    println!();
    for (i, (val, name, desc)) in options.iter().enumerate() {
        let (arrow, name_fmt) = if i == cursor {
            ("▸".to_string(), format!("{BOLD}{CYN}{name}{R}"))
        } else {
            (" ".to_string(), format!("{DIM}{name}{R}"))
        };
        let cur_tag = if *val == current.as_str() && i != cursor {
            format!("  {DIM}(current){R}")
        } else {
            String::new()
        };
        println!("  {arrow} {name_fmt}  {DIM}{desc}{R}{cur_tag}");
    }
    println!();
    let _ = io::stdout().flush();

    let _ = enable_raw_mode();
    let confirmed = loop {
        if stopping.load(Ordering::Relaxed) { break false; }
        if !event::poll(Duration::from_millis(50)).unwrap_or(false) { continue; }
        if let Ok(Event::Key(k)) = event::read() {
            match k.code {
                KeyCode::Up   | KeyCode::Char('k') => {
                    if cursor > 0 { cursor -= 1; }
                    print!("\x1b[{}A\x1b[0J", block_lines);
                    render_raw(cursor);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if cursor < options.len() - 1 { cursor += 1; }
                    print!("\x1b[{}A\x1b[0J", block_lines);
                    render_raw(cursor);
                }
                KeyCode::Enter => break true,
                KeyCode::Esc   => break false,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    stopping.store(true, Ordering::Relaxed);
                    break false;
                }
                _ => {}
            }
        }
    };
    let _ = disable_raw_mode();
    ensure_cooked_output();

    let selected = options[cursor].0;
    if confirmed && selected != current.as_str() {
        let name = options[cursor].1;
        println!("  {GRN}✓{R}  PLATFORM_MODE → {BOLD}{selected}{R}  {DIM}({name}){R}");
        map.insert("PLATFORM_MODE".to_string(), selected.to_string());
        write_env_file(env_path, &map);
    } else if !confirmed {
        println!("  {DIM}Cancelled — keeping {current}{R}");
    } else {
        println!("  {DIM}Unchanged — {current}{R}");
    }
    println!();
}

///
/// Escapable at every prompt:
///   - Ctrl+C / Ctrl+D  →  aborts the wizard immediately
///   - `q` at the confirmation prompt  →  skips the wizard
fn run_env_wizard(env_path: &Path, vars: &[EnvVar], service_label: &str) {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 { return; }
    // Each call to read_value_tui() creates and drops a TuiGuard; restore
    // cooked output flags before every println! in this function.
    ensure_cooked_output();

    let mut env = parse_env_file(env_path);

    // ── Audit ─────────────────────────────────────────────────────────────────
    let missing: Vec<&EnvVar> = vars.iter()
        .filter(|v| is_placeholder(env.get(v.key).map(|s| s.as_str()).unwrap_or("")))
        .collect();

    println!("  {BOLD}{service_label}{R}");
    for v in vars {
        let cur = env.get(v.key).map(|s| s.as_str()).unwrap_or("");
        let (icon, display) = if is_placeholder(cur) {
            (format!("{RED}✗{R}"), format!("{RED}not set{R}"))
        } else if v.secret {
            (format!("{GRN}✓{R}"), format!("{GRN}[set]{R}"))
        } else {
            let preview: String = cur.chars().take(48).collect();
            (format!("{GRN}✓{R}"), format!("{GRN}{preview}{R}"))
        };
        println!("  {icon}  {:<38} {display}", v.key);
    }
    println!();

    if missing.is_empty() {
        println!("  {GRN}All required variables are set.{R}");
        println!();
        return;
    }

    // ── Confirmation prompt ───────────────────────────────────────────────────
    println!("  {YLW}{} variable{} not set.{R}",
        missing.len(), if missing.len() == 1 { " is" } else { "s are" });
    print!("  Configure {} now? {DIM}[Y]es  [a]uto-generate  [n]o  [q]uit{R}  ",
        if missing.len() == 1 { "it" } else { "them" });
    let _ = io::stdout().flush();

    let answer = match read_line_or_interrupt() {
        None => {
            println!("\n  {YLW}Interrupted — skipping {service_label}.{R}\n");
            return;
        }
        Some(a) => a,
    };

    match answer.trim().to_ascii_lowercase().as_str() {
        "n" => {
            println!("  {YLW}Skipped — {service_label} will fail until these are set.{R}\n");
            return;
        }
        "q" => {
            println!("  {YLW}Wizard aborted.{R}\n");
            return;
        }
        "a" => {
            println!();
            auto_generate_missing(&mut env, &missing, &global_prefs_path());
            println!();
            write_env_file(env_path, &env);
            let prefs_path = global_prefs_path();
            println!("  {GRN}Saved → {}{R}", env_path.display());
            println!("  {DIM}Preferences → {}{R}", prefs_path.display());
            println!();
            return;
        }
        _ => {}
    }
    println!();

    // ── Per-variable prompts (Ratatui text-area overlay) ──────────────────────
    let total = missing.len();
    let mut changed = false;
    for (step, v) in missing.iter().enumerate() {
        let raw_value = match read_value_tui(v, step + 1, total) {
            None => {
                ensure_cooked_output();
                println!("  {YLW}Wizard aborted.{R}\n");
                if changed { write_env_file(env_path, &env); }
                return;
            }
            Some(s) => s,
        };
        ensure_cooked_output();

        let final_value = if raw_value.trim().is_empty() && v.auto_uuid {
            let uuid = gen_uuid();
            println!("  {GRN}Auto-generated:{R}  {DIM}{uuid}{R}");
            uuid
        } else if raw_value.trim().is_empty() && v.auto_b64 {
            let key = gen_base64_key();
            println!("  {GRN}Auto-generated:{R}  {DIM}{key}{R}");
            key
        } else {
            raw_value
        };

        if final_value.trim().is_empty() {
            println!("  {YLW}No input — {}{R} left unset.", v.key);
        } else {
            env.insert(v.key.to_string(), final_value);
            changed = true;
            println!("  {GRN}✓{R}  {}", v.key);
        }
    }
    println!();

    // ── Persist ───────────────────────────────────────────────────────────────
    if changed {
        write_env_file(env_path, &env);
        println!("  {GRN}Saved → {}{R}", env_path.display());
    } else {
        println!("  {YLW}Nothing changed.{R}");
    }
    println!();
}

// ── Product selector ─────────────────────────────────────────────────────────

struct ProductChoice {
    label:     &'static str,
    desc:      &'static str,
    repo:      &'static str,  // bare repo dir name: "filigran-copilot", "opencti", …
    /// Currently checked by the user.
    enabled:   bool,
    /// The directory for this product exists on disk (or will be created).
    available: bool,
    /// Branch to launch this product on. May differ per product.
    branch:    String,
}

fn build_product_selector_lines(slug: &str, choices: &[ProductChoice], cursor: usize) -> Vec<String> {
    let sep = "─".repeat(72);
    let mut out = Vec::new();
    out.push(format!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}\n"));
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!("  {BOLD}Launch configuration{R}  {DIM}— pick what to start{R}"));
    out.push(format!("  {DIM}{sep}{R}\n"));

    for (i, c) in choices.iter().enumerate() {
        let marker = if i == cursor { format!("{CYN}{BOLD}▶{R} ") } else { "  ".to_string() };

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

        out.push(format!("  {marker}{checkbox}  {:<22}{:<26}{branch_col}", name, desc));
    }

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!("  {DIM}↑↓ / j k  navigate   Space toggle   b branch   Enter start   c clean start   q quit{R}"));
    out.push(String::new());
    out
}

/// Interactive product-selection screen.
///
/// Returns `LaunchMode::Normal` on Enter, `LaunchMode::Clean` on `c` (wipe Docker first),
/// `LaunchMode::Quit` on q/Esc.
/// Runs in raw mode internally; terminal is restored before returning.
/// Press `b` to set a custom branch for the highlighted product (worktree created on launch).
fn run_product_selector(slug: &str, choices: &mut Vec<ProductChoice>) -> LaunchMode {
    // No-op in non-interactive environments — caller uses the defaults.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return LaunchMode::Normal;
    }

    let mut raw = TuiGuard::enter();
    let mut cursor = 0usize;
    // Start cursor on the first available product.
    if let Some(first) = choices.iter().position(|c| c.available || !c.branch.is_empty()) {
        cursor = first;
    }

    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(tui, &build_product_selector_lines(slug, choices, cursor));
        }

        if !event::poll(Duration::from_millis(20)).unwrap_or(false) { continue; }
        let Ok(Event::Key(ke)) = event::read() else { continue; };
        if ke.kind != crossterm::event::KeyEventKind::Press { continue; }

        match ke.code {
            // Navigate up
            KeyCode::Up | KeyCode::Char('k') => {
                cursor = cursor.saturating_sub(1);
            }
            // Navigate down
            KeyCode::Down | KeyCode::Char('j') => {
                if cursor + 1 < choices.len() { cursor += 1; }
            }
            // Toggle — if the product is available, toggle enabled.
            // If unavailable and no branch set, fall through to the branch prompt
            // (same as pressing 'b') so the user can enable it via a worktree.
            KeyCode::Char(' ') => {
                if choices[cursor].available || !choices[cursor].branch.is_empty() {
                    choices[cursor].enabled = !choices[cursor].enabled;
                } else {
                    // Product dir doesn't exist yet — ask for a branch name so a worktree
                    // can be created, which makes the product available.
                    drop(raw.take());
                    print!("\n  Branch for {} : ", choices[cursor].label);
                    let _ = io::stdout().flush();
                    if let Some(input) = read_line_or_interrupt() {
                        let trimmed = input.trim().to_string();
                        if !trimmed.is_empty() {
                            choices[cursor].branch   = trimmed;
                            choices[cursor].enabled  = true;
                            choices[cursor].available = true;
                        }
                    }
                    raw = TuiGuard::enter();
                }
            }
            // Edit branch for highlighted product
            KeyCode::Char('b') | KeyCode::Char('B') => {
                // Exit raw mode so we can read a normal line.
                drop(raw.take());
                let current = &choices[cursor].branch;
                if current.is_empty() {
                    print!("\n  Branch for {} : ", choices[cursor].label);
                } else {
                    print!("\n  Branch for {} (Enter to keep {current}): ", choices[cursor].label);
                }
                let _ = io::stdout().flush();
                if let Some(input) = read_line_or_interrupt() {
                    let trimmed = input.trim().to_string();
                    if !trimmed.is_empty() {
                        choices[cursor].branch  = trimmed;
                        choices[cursor].enabled  = true;
                        choices[cursor].available = true; // worktree will be created
                    }
                }
                // Re-enter raw mode.
                raw = TuiGuard::enter();
            }
            // Confirm — normal start
            KeyCode::Enter => {
                return LaunchMode::Normal;
            }
            // Clean start — wipe Docker containers + volumes then launch
            KeyCode::Char('c') | KeyCode::Char('C') => {
                return LaunchMode::Clean;
            }
            // Quit
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                // Clear screen before exiting so the terminal is clean.
                print!("\x1b[H\x1b[2J");
                let _ = io::stdout().flush();
                return LaunchMode::Quit;
            }
            _ => {}
        }
    }
}

// ── Feature flag selector ─────────────────────────────────────────────────────

struct FlagChoice {
    name:    String,
    enabled: bool,
}

/// Walk `dir` recursively, collect every unique flag name passed to
/// `isFeatureEnabled()` (graphql) or `isFeatureEnable()` (frontend).
/// Skips `node_modules` and non-.ts/.js/.tsx files.
#[allow(dead_code)]
fn discover_feature_flags(dir: &Path) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = Default::default();
    discover_flags_in_dir(dir, &mut set);
    set.into_iter().collect()
}

fn discover_flags_in_dir(dir: &Path, out: &mut std::collections::BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().map_or(false, |n| n == "node_modules" || n == ".git") {
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
    let Ok(content) = fs::read_to_string(path) else { return };
    // Match both `isFeatureEnabled(` (graphql/backend) and `isFeatureEnable(` (frontend)
    for needle in &["isFeatureEnabled(", "isFeatureEnable("] {
        extract_flag_calls(&content, needle, out);
    }
}

fn extract_flag_calls(content: &str, needle: &str, out: &mut std::collections::BTreeSet<String>) {
    let mut search = content;
    while let Some(idx) = search.find(needle) {
        search = &search[idx + needle.len()..];
        // Deduplicate: skip `isFeatureEnabled(` when we're iterating `isFeatureEnable(`
        // by ignoring a match where the very next char is 'd' (already covered by the longer needle).
        if needle == "isFeatureEnable(" {
            if search.starts_with('d') { continue; }
        }
        // Find the opening quote right after the '('
        let rest = search.trim_start_matches(' ');
        let quote = match rest.chars().next() {
            Some('\'') => '\'',
            Some('"')  => '"',
            _           => continue,
        };
        let inner = &rest[1..];
        if let Some(end) = inner.find(quote) {
            let flag = &inner[..end];
            // Flag names are alphanumeric + underscores
            if !flag.is_empty() && flag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                out.insert(flag.to_string());
            }
        }
    }
}

/// Parse the `APP__ENABLED_DEV_FEATURES` JSON array from an env file.
fn read_active_flags(env_file: &Path) -> Vec<String> {
    let map = parse_env_file(env_file);
    let raw = map.get("APP__ENABLED_DEV_FEATURES").cloned().unwrap_or_default();
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.is_empty() { return vec![]; }
    trimmed.split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Write `flags` back into `APP__ENABLED_DEV_FEATURES` in the env file.
fn write_active_flags(env_file: &Path, flags: &[String]) {
    let mut map = parse_env_file(env_file);
    let val = if flags.is_empty() {
        "[]".to_string()
    } else {
        let inner = flags.iter().map(|f| format!("\"{f}\"")).collect::<Vec<_>>().join(",");
        format!("[{inner}]")
    };
    map.insert("APP__ENABLED_DEV_FEATURES".to_string(), val);
    write_env_file(env_file, &map);
}

fn build_flag_selector_lines(slug: &str, product: &str, choices: &[FlagChoice], cursor: usize) -> Vec<String> {
    let sep = "─".repeat(56);
    let mut out = Vec::new();
    out.push(format!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}\n"));
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!("  {BOLD}Feature flags{R}  {DIM}— {product}{R}"));
    out.push(format!("  {DIM}{sep}{R}\n"));

    for (i, c) in choices.iter().enumerate() {
        let marker = if i == cursor { format!("{CYN}{BOLD}▶{R} ") } else { "  ".to_string() };
        let checkbox = if c.enabled { format!("{GRN}[{BOLD}✓{R}{GRN}]{R}") } else { format!("{DIM}[ ]{R}") };
        let name = if i == cursor { format!("{BOLD}{}{R}", c.name) } else { c.name.clone() };
        out.push(format!("  {marker}{checkbox}  {name}"));
    }

    out.push(String::new());
    out.push(format!("  {DIM}{sep}{R}"));
    out.push(format!("  {DIM}↑↓ / j k  navigate   Space  toggle   Enter  confirm   q  skip{R}"));
    out.push(String::new());
    out
}

/// Interactive feature-flag selector for one product.
///
/// Modifies `choices` in-place. Returns when the user presses Enter (confirm)
/// or q/Esc (skip — keeping whatever state was set). Always returns `true` so
/// the caller can decide whether to proceed.
fn run_flag_selector(slug: &str, product: &str, choices: &mut Vec<FlagChoice>) {
    if choices.is_empty() || unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return;
    }
    let mut raw = TuiGuard::enter();
    let mut cursor = 0usize;
    loop {
        if let Some(tui) = raw.as_mut() {
            draw_ansi_lines(tui, &build_flag_selector_lines(slug, product, choices, cursor));
        }
        if event::poll(Duration::from_millis(20)).unwrap_or(false) {
            let Ok(Event::Key(ke)) = event::read() else { continue; };
            match ke.code {
                KeyCode::Up   | KeyCode::Char('k') => { cursor = cursor.saturating_sub(1); }
                KeyCode::Down | KeyCode::Char('j') => { if cursor + 1 < choices.len() { cursor += 1; } }
                KeyCode::Char(' ') => { choices[cursor].enabled = !choices[cursor].enabled; }
                KeyCode::Enter => { return; }
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => { return; }
                _ => {}
            }
        }
    }
}

// ── Workspace helpers ─────────────────────────────────────────────────────────

/// Build initial product choices with no specific branches (uses main repo dirs).
fn default_product_choices(workspace_root: &Path) -> Vec<ProductChoice> {
    PRODUCTS.iter().map(|(repo, label, _, desc)| {
        let main_dir = workspace_root.join(repo);
        let available = main_dir.is_dir();
        let branch = if available { current_branch(&main_dir) } else { String::new() };
        ProductChoice { label, desc, repo, enabled: available, available, branch }
    }).collect()
}

/// Build workspace entries from explicit branch/commit/worktree CLI args.
///
/// Priority per product: explicit branch > commit flag > worktree path (branch derived
/// from the worktree's HEAD).  A worktree on a detached HEAD is stored as `commit:<hash>`.
fn build_entries_from_branches(args: &Args) -> Vec<WorkspaceEntry> {
    PRODUCTS.iter().map(|(repo, _, key, _)| {
        // Determine the branch value to store: explicit branch, commit ref, or derived from worktree.
        let branch: Option<String> = match *key {
            "copilot" => {
                args.copilot_branch.clone()
                    .or_else(|| args.copilot_commit.as_ref().map(|c| format!("{COMMIT_PREFIX}{c}")))
                    .or_else(|| args.copilot_worktree.as_ref().and_then(|p| derive_branch_from_path(p)))
            }
            "opencti" => {
                args.opencti_branch.clone()
                    .or_else(|| args.opencti_commit.as_ref().map(|c| format!("{COMMIT_PREFIX}{c}")))
                    .or_else(|| args.opencti_worktree.as_ref().and_then(|p| derive_branch_from_path(p)))
            }
            "openaev" => {
                args.openaev_branch.clone()
                    .or_else(|| args.openaev_commit.as_ref().map(|c| format!("{COMMIT_PREFIX}{c}")))
                    .or_else(|| args.openaev_worktree.as_ref().and_then(|p| derive_branch_from_path(p)))
            }
            "connector" => {
                args.connector_branch.clone()
                    .or_else(|| args.connector_commit.as_ref().map(|c| format!("{COMMIT_PREFIX}{c}")))
                    .or_else(|| args.connector_worktree.as_ref().and_then(|p| derive_branch_from_path(p)))
            }
            _ => None,
        };
        WorkspaceEntry { repo: repo.to_string(), enabled: branch.is_some(), branch: branch.unwrap_or_default() }
    }).collect()
}

/// Read the current branch from a worktree path.  If the HEAD is detached, returns
/// `Some("commit:<short-hash>")`.  Returns `None` only if the path is not a git repo.
fn derive_branch_from_path(path: &PathBuf) -> Option<String> {
    if !path.is_dir() { return None; }
    let branch = current_branch(path);
    if !branch.is_empty() {
        return Some(branch);
    }
    let commit = current_commit_short(path);
    if !commit.is_empty() {
        return Some(format!("{COMMIT_PREFIX}{commit}"));
    }
    None
}

/// Show the product selector for a new workspace, save the config, return it + choices.
fn build_new_workspace_interactive(
    workspace_root: &Path,
    ws_dir: &Path,
) -> (WorkspaceConfig, Vec<ProductChoice>, bool) {
    let mut choices = default_product_choices(workspace_root);
    let clean = match run_product_selector("new", &mut choices) {
        LaunchMode::Quit => std::process::exit(0),
        LaunchMode::Clean => true,
        LaunchMode::Normal => false,
    };
    let cfg = choices_to_workspace(&choices);
    save_workspace(ws_dir, &cfg);
    (cfg, choices, clean)
}

/// Resolve which workspace to use.
///
/// Priority:
/// 1. `--workspace <hash>` — load by hash, skip product selector
/// 2. `--*-branch` flags — build from branches, compute hash, skip selector
/// 3. Interactive — workspace list → pick/create → product selector → save
fn resolve_workspace(
    args: &Args,
    workspace_root: &Path,
    ws_dir: &Path,
) -> (WorkspaceConfig, Vec<ProductChoice>, bool /* clean_start */) {
    if let Some(hash) = &args.workspace {
        match load_workspace(ws_dir, hash) {
            Some(cfg) => {
                let choices = workspace_to_choices(&cfg, workspace_root);
                (cfg, choices, false)
            }
            None => {
                eprintln!("Workspace '{}' not found in {}.", hash, ws_dir.display());
                std::process::exit(1);
            }
        }
    } else if args.copilot_branch.is_some()   || args.opencti_branch.is_some()
           || args.openaev_branch.is_some()   || args.connector_branch.is_some()
           || args.copilot_commit.is_some()   || args.opencti_commit.is_some()
           || args.openaev_commit.is_some()   || args.connector_commit.is_some()
           || args.copilot_worktree.is_some() || args.opencti_worktree.is_some()
           || args.openaev_worktree.is_some() || args.connector_worktree.is_some()
    {
        // Build from branch flags — find or create workspace.
        let entries = build_entries_from_branches(args);
        let hash = compute_workspace_hash(&entries);
        let cfg = load_workspace(ws_dir, &hash).unwrap_or_else(|| {
            let c = WorkspaceConfig { hash: hash.clone(), created: today(), entries };
            save_workspace(ws_dir, &c);
            c
        });
        let choices = workspace_to_choices(&cfg, workspace_root);
        (cfg, choices, false)
    } else {
        // Interactive: workspace list → product selector.
        // Loops back to the selector after a Delete so the user can continue
        // managing workspaces without restarting the tool.
        let mut choices = 'selector: loop {
            let workspaces = list_workspaces(ws_dir);
            if workspaces.is_empty() {
                return build_new_workspace_interactive(workspace_root, ws_dir);
            }
            // Drain any pending crossterm events (e.g. the Enter from a delete
            // confirmation) so they don't accidentally fire in run_workspace_selector.
            drain_input_events();
            match run_workspace_selector(&workspaces) {
                WorkspaceAction::Delete(cfg) => {
                    run_workspace_delete(&cfg, workspace_root, ws_dir);
                    // list_workspaces will exclude tombstoned entries on next iteration
                    continue 'selector;
                }
                WorkspaceAction::Open(cfg) => {
                    break workspace_to_choices(&cfg, workspace_root);
                }
                WorkspaceAction::CreateNew => {
                    break default_product_choices(workspace_root);
                }
                WorkspaceAction::Quit => {
                    print!("\x1b[H\x1b[2J");
                    let _ = io::stdout().flush();
                    std::process::exit(0);
                }
            }
        };
        // Drain any residual key events (e.g. Enter release from workspace selector)
        // before entering the product selector.
        drain_input_events();
        // Product selector pre-filled with selected workspace (or fresh defaults).
        let clean = match run_product_selector("", &mut choices) {
            LaunchMode::Quit  => { print!("\x1b[H\x1b[2J"); let _ = io::stdout().flush(); std::process::exit(0); }
            LaunchMode::Clean => true,
            LaunchMode::Normal => false,
        };
        let cfg = choices_to_workspace(&choices);
        save_workspace(ws_dir, &cfg);
        (cfg, choices, clean)
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    // Clear the terminal immediately so the previous session's shutdown screen
    // does not bleed through while the new launch is initialising.
    print!("\x1b[H\x1b[2J");
    let _ = io::stdout().flush();

    let workspace_root = resolve_workspace_root(&args);
    let ws_dir = workspaces_dir(&workspace_root);

    // ── Workspace + product selection ─────────────────────────────────────────
    let (workspace_cfg, choices, clean_start) = resolve_workspace(&args, &workspace_root, &ws_dir);
    // The workspace/product selectors run inside a TuiGuard (raw mode).
    // Explicitly restore cooked-mode output flags so every println! in the
    // startup phase below renders with proper \r\n conversion.
    ensure_cooked_output();
    let slug = workspace_cfg.hash.clone();

    let logs_dir = args.logs_dir.clone()
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/dev-feature-logs/{}", slug)));
    fs::create_dir_all(&logs_dir).expect("cannot create logs_dir");

    // ── Per-product worktree path overrides (--copilot-worktree etc.) ────────────
    // Returns the override path for a repo, if the user passed --*-worktree.
    let get_worktree_override = |repo: &str| -> Option<&PathBuf> {
        match repo {
            "filigran-copilot" => args.copilot_worktree.as_ref(),
            "opencti"          => args.opencti_worktree.as_ref(),
            "openaev"          => args.openaev_worktree.as_ref(),
            "connectors"       => args.connector_worktree.as_ref(),
            _                  => None,
        }
    };

    // ── Create missing worktrees ───────────────────────────────────────────────
    {
        let sep = "─".repeat(56);
        // Products with an explicit --*-worktree path don't need worktree creation.
        let need_worktrees = choices.iter().any(|c| {
            c.enabled && !c.branch.is_empty() && get_worktree_override(c.repo).is_none() && {
                let target = workspace_root.join(format!("{}-{}", c.repo, branch_to_slug(&c.branch)));
                let main   = workspace_root.join(c.repo);
                !target.is_dir()
                    && main.is_dir()
                    && current_branch(&main) != c.branch.as_str()
            }
        });
        if need_worktrees {
            println!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}");
            println!("\n  {DIM}{sep}{R}");
            println!("  {BOLD}Setting up worktrees{R}");
            println!("  {DIM}{sep}{R}\n");
        }
        for c in choices.iter().filter(|c| c.enabled && !c.branch.is_empty()) {
            if get_worktree_override(c.repo).is_none() {
                ensure_worktree(&workspace_root, c.repo, &c.branch);
            }
        }
        if need_worktrees { println!(); }
    }

    // Rebuild paths so feature flags, env wizard, and services use the right dirs.
    // An explicit --*-worktree path overrides the computed worktree location.
    let paths = {
        let resolve_path = |repo: &str, branch: &str, override_path: Option<&PathBuf>| -> PathBuf {
            if let Some(p) = override_path {
                return p.clone();
            }
            if branch.is_empty() { return workspace_root.join(repo); }
            let slug = branch_to_slug(branch);
            let wt   = workspace_root.join(format!("{}-{}", repo, slug));
            if wt.is_dir() { return wt; }
            // If the main checkout is already on this branch (no worktree was
            // created), use it directly.
            let main = workspace_root.join(repo);
            if current_branch(&main) == branch { return main; }
            main
        };
        Paths {
            copilot:   resolve_path(choices[0].repo, &choices[0].branch, get_worktree_override(choices[0].repo)),
            opencti:   resolve_path(choices[1].repo, &choices[1].branch, get_worktree_override(choices[1].repo)),
            openaev:   resolve_path(choices[2].repo, &choices[2].branch, get_worktree_override(choices[2].repo)),
            connector: resolve_path(choices[3].repo, &choices[3].branch, get_worktree_override(choices[3].repo))
                           .join("internal-import-file/import-document-ai"),
        }
    };

    // Derive activity flags from workspace choices.
    let no_copilot       = !(choices[0].enabled && paths.copilot.is_dir());
    let no_opencti       = !(choices[1].enabled && paths.opencti.is_dir());
    let no_openaev       = !(choices[2].enabled && paths.openaev.is_dir());
    let no_connector     = !(choices[3].enabled && paths.connector.is_dir());
    let no_opencti_front = no_opencti || args.no_opencti_front;
    let no_openaev_front = no_openaev || args.no_openaev_front;

    // ════════════════════════════════════════════════════════════════════════
    //  Feature flags  —  per-product interactive selector
    // ════════════════════════════════════════════════════════════════════════

    // OpenCTI: scan src/ for isFeatureEnabled() calls, let user toggle them.
    if !no_opencti && paths.opencti.is_dir() {
        let gql_dir  = paths.opencti.join("opencti-platform/opencti-graphql");
        let env_file = gql_dir.join(".env.dev");
        if gql_dir.is_dir() {
            // Scan both the graphql backend and the frontend for flag usages.
            let front_dir = paths.opencti.join("opencti-platform/opencti-front/src");
            let mut flag_set: std::collections::BTreeSet<String> = Default::default();
            discover_flags_in_dir(&gql_dir.join("src"), &mut flag_set);
            if front_dir.is_dir() {
                discover_flags_in_dir(&front_dir, &mut flag_set);
            }
            let discovered: Vec<String> = flag_set.into_iter().collect();
            if !discovered.is_empty() {
                // Ensure .env.dev exists before we read/write it.
                ensure_opencti_env(&gql_dir);
                let active = read_active_flags(&env_file);
                let mut flag_choices: Vec<FlagChoice> = discovered.iter()
                    .map(|f| FlagChoice { name: f.clone(), enabled: active.contains(f) })
                    .collect();
                run_flag_selector(&slug, "OpenCTI", &mut flag_choices);
                let selected: Vec<String> = flag_choices.into_iter()
                    .filter(|f| f.enabled)
                    .map(|f| f.name)
                    .collect();
                write_active_flags(&env_file, &selected);
            }
        }
    }

    // Load optional LLM config for crash diagnosis.
    let llm_cfg: Option<LlmConfig> = {
        let dev_cfg = load_config();
        resolve_llm_config(dev_cfg.as_ref())
    };
    if llm_cfg.is_some() {
        println!("  {DIM}LLM diagnosis enabled.{R}");
    }

    let state:    State              = Arc::new(Mutex::new(Vec::new()));
    let mut procs: Vec<Proc>         = Vec::new();
    let stopping: Arc<AtomicBool>    = Arc::new(AtomicBool::new(false));

    // Diagnosis channel: background threads → main loop.
    let (diag_tx, diag_rx) = mpsc::sync_channel::<DiagEvent>(32);
    // Track which service indices have already had diagnosis dispatched.
    let mut diagnosed: HashSet<usize> = HashSet::new();

    // ── Signal handlers ──────────────────────────────────────────────────────
    // ctrlc handles SIGINT + SIGTERM (with the "termination" feature).
    // We add SIGHUP separately so that closing the terminal window triggers
    // the same clean shutdown path instead of orphaning all child processes.
    {
        let stopping = Arc::clone(&stopping);
        ctrlc::set_handler(move || {
            stopping.store(true, Ordering::Relaxed);
        }).expect("failed to set Ctrl+C handler");
    }
    unsafe { libc::signal(libc::SIGHUP, sighup_handler as *const () as libc::sighandler_t); }

    // ── Orphan recovery ──────────────────────────────────────────────────────
    // If the previous session was SIGKILL'd (or crashed), it left a PID file.
    // Kill any surviving processes before we try to bind their ports.
    kill_orphaned_pids(&slug);

    // ════════════════════════════════════════════════════════════════════════
    //  Step 1 / 2  —  Environment
    // ════════════════════════════════════════════════════════════════════════
    let sep = "─".repeat(56);
    println!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}");
    println!("\n  {DIM}{sep}{R}");
    println!("  {BOLD}Step 1 / 2  —  Environment{R}");
    println!("  {DIM}{sep}{R}\n");

    // ── Workspace .env directory (ws_dir/<hash>/) ─────────────────────────────
    // All product env files are stored here.  At startup we initialise any
    // missing files from templates, run wizards to fill in placeholder values,
    // then deploy copies into the relevant worktree paths so the services and
    // Docker Compose can find them.
    let ws_env_dir = ws_dir.join(&slug);
    let _ = fs::create_dir_all(&ws_env_dir); // defensive — should already exist

    // Copilot
    if !no_copilot && paths.copilot.is_dir() {
        let env_path = ws_env_path(&ws_env_dir, "copilot");
        let templates: Vec<PathBuf> = [".env.sample", ".env.example"]
            .iter().map(|f| paths.copilot.join(f)).collect();
        init_workspace_env(
            &env_path,
            Some(&paths.copilot.join(".env")),
            &templates,
            "# Copilot dev environment\nADMIN_EMAIL=admin@example.com\nADMIN_PASSWORD=ChangeMe\n",
        );
        let compose_dev = paths.copilot.join("docker-compose.dev.yml");
        // Pre-flight: align env ports with docker-compose host-port mappings.
        // Runs every launch; prints a line for every correction so mismatches are visible.
        preflight_port_checks(&env_path, &compose_dev, &[
            PortCheck { label: "REDIS_URL",   env_key: "REDIS_URL",   default_value: "redis://localhost:6379", container_port: 6379 },
            PortCheck { label: "S3_ENDPOINT", env_key: "S3_ENDPOINT", default_value: "localhost:9000",         container_port: 9000 },
        ]);
        // Align BASE_URL / FRONTEND_URL: .env.sample defaults (8000/3000) →
        // dev-feature ports (8100/3100). Custom user ports are left untouched.
        patch_url_default(&env_path, "BASE_URL",     8000, 8100);
        patch_url_default(&env_path, "FRONTEND_URL", 3000, 3100);
        run_platform_mode_selector(&env_path, &stopping);
        run_env_wizard(&env_path, COPILOT_ENV_VARS, "Copilot");
    } else if no_copilot {
        println!("  {DIM}Copilot skipped.{R}\n");
    }

    // OpenCTI
    if !no_opencti && paths.opencti.is_dir() {
        let env_path = ws_env_path(&ws_env_dir, "opencti");
        let gql_dir  = paths.opencti.join("opencti-platform/opencti-graphql");
        init_workspace_env(
            &env_path,
            Some(&gql_dir.join(".env.dev")),
            &[],
            "# OpenCTI graphql dev environment — generated by dev-feature\n\
# Leave TOKEN and ENCRYPTION_KEY as ChangeMe; the wizard will auto-generate them.\n\
APP__ADMIN__EMAIL=admin@opencti.io\n\
APP__ADMIN__PASSWORD=ChangeMe\n\
APP__ADMIN__TOKEN=ChangeMe\n\
APP__ENCRYPTION_KEY=ChangeMe\n",
        );
        run_env_wizard(&env_path, OPENCTI_ENV_VARS, "OpenCTI");
    } else if no_opencti {
        println!("  {DIM}OpenCTI skipped.{R}\n");
    }

    // OpenAEV — no wizard; .env.example has sensible defaults for local dev.
    if !no_openaev && paths.openaev.is_dir() {
        let env_path  = ws_env_path(&ws_env_dir, "openaev");
        let dev_dir   = paths.openaev.join("openaev-dev");
        let templates = vec![dev_dir.join(".env.example")];
        init_workspace_env(
            &env_path,
            Some(&dev_dir.join(".env")),
            &templates,
            "# OpenAEV dev environment\n",
        );
        // No wizard — defaults from .env.example are fine for local dev.
    } else if no_openaev {
        println!("  {DIM}OpenAEV skipped.{R}\n");
    }

    // Connector (init first — needed for auto-propagation below)
    if !no_connector && paths.connector.is_dir() {
        let env_path = ws_env_path(&ws_env_dir, "connector");
        init_workspace_env(
            &env_path,
            Some(&paths.connector.join(".env.dev")),
            &[],
            "# Connector dev environment — fill in before running\n\
OPENCTI_URL=http://localhost:4000\n\
OPENCTI_TOKEN=ChangeMe\n\
CONNECTOR_TYPE=INTERNAL_IMPORT_FILE\n\
CONNECTOR_ID=54263257-26dc-4cca-8c45-deea44cdecf1\n\
CONNECTOR_NAME=ImportDocumentAI\n\
CONNECTOR_SCOPE=application/pdf,text/plain,text/html,text/markdown\n\
CONNECTOR_AUTO=false\n\
CONNECTOR_LOG_LEVEL=debug\n\
CONNECTOR_WEB_SERVICE_URL=https://importdoc.ariane.testing.filigran.io\n\
IMPORT_DOCUMENT_CREATE_INDICATOR=false\n\
IMPORT_DOCUMENT_INCLUDE_RELATIONSHIPS=true\n\
CONNECTOR_LICENCE_KEY_PEM=\n",
        );
    }

    // Auto-propagate APP__ADMIN__TOKEN → OPENCTI_TOKEN (workspace files only).
    if !no_opencti && !no_connector {
        let opencti_env  = ws_env_path(&ws_env_dir, "opencti");
        let connector_env = ws_env_path(&ws_env_dir, "connector");
        if opencti_env.exists() && connector_env.exists() {
            if let Some(token) = parse_env_file(&opencti_env).get("APP__ADMIN__TOKEN").cloned() {
                if !token.is_empty() && token != "ChangeMe" {
                    let mut cenv = parse_env_file(&connector_env);
                    cenv.insert("OPENCTI_TOKEN".to_string(), token);
                    write_env_file(&connector_env, &cenv);
                    println!("  {GRN}✓{R}  OPENCTI_TOKEN synced from OpenCTI admin token");
                }
            }
        }
    }

    // Connector wizard (after auto-propagation so OPENCTI_TOKEN may already be filled).
    if !no_connector && paths.connector.is_dir() {
        run_env_wizard(
            &ws_env_path(&ws_env_dir, "connector"),
            CONNECTOR_ENV_VARS,
            "ImportDocumentAI connector",
        );
    } else if no_connector {
        println!("  {DIM}Connector skipped.{R}\n");
    }

    // ── Deploy workspace .env files → repo destinations ───────────────────────
    // Must happen before Docker Compose so that docker-compose --env-file can
    // find the file (e.g. openaev-dev/.env), and before service spawn so the
    // processes see the right values.
    if !no_copilot   { deploy_workspace_env(&ws_env_path(&ws_env_dir, "copilot"),   &paths.copilot.join(".env")); }
    if !no_opencti   { deploy_workspace_env(&ws_env_path(&ws_env_dir, "opencti"),   &paths.opencti.join("opencti-platform/opencti-graphql/.env.dev")); }
    if !no_openaev   { deploy_workspace_env(&ws_env_path(&ws_env_dir, "openaev"),   &paths.openaev.join("openaev-dev/.env")); }
    if !no_connector { deploy_workspace_env(&ws_env_path(&ws_env_dir, "connector"), &paths.connector.join(".env.dev")); }

    // ════════════════════════════════════════════════════════════════════════
    //  Step 2 / 2  —  Starting services
    // ════════════════════════════════════════════════════════════════════════
    println!("  {DIM}{sep}{R}");
    println!("  {BOLD}Step 2 / 2  —  Starting services{R}");
    println!("  {DIM}{sep}{R}\n");

    // ── Bootstrap: Corepack ──────────────────────────────────────────────────
    // All JS projects in this workspace use yarn@4 via the `packageManager` field.
    // Corepack must be enabled so the system `yarn` shim dispatches to the right version.
    print!("  Checking Corepack… ");
    let _ = io::stdout().flush();
    ensure_corepack();

    // ── Clean start: wipe Docker before bringing anything up ─────────────────
    if clean_start {
        clean_docker_for_workspace(&slug, &paths, no_copilot, no_opencti, no_openaev);
    }

    // ── Docker deps (blocking) ────────────────────────────────────────────────
    // Each workspace gets its own Docker project name (`{base}-{ws_hash[..8]}`).
    // A compose override file is generated in /tmp that renames explicit
    // `container_name:` directives with the same suffix, giving full container
    // isolation between workspaces even when the compose files use hardcoded names.
    //
    // docker_compose_up() is idempotent: running containers are left alone,
    // stopped containers are restarted, missing ones are created.
    print!("  Checking Docker… ");
    let _ = io::stdout().flush();
    let docker_ok = docker_available();
    if docker_ok {
        println!("{GRN}running{R}");
    } else {
        println!("{RED}not reachable{R}");
        println!("  {YLW}Start Docker Desktop (or the Docker daemon) before launching the stack.{R}");
        println!("  {DIM}Services that need infrastructure containers will start in Degraded state.{R}\n");
    }

    // Read Copilot ports from the workspace env (set by preflight above).
    // These override whatever the cached .dev-launcher.conf contains.
    let copilot_env_path      = ws_env_path(&ws_env_dir, "copilot");
    let copilot_backend_port  = read_env_url_port(&copilot_env_path, "BASE_URL",     8100);
    let copilot_frontend_port = read_env_url_port(&copilot_env_path, "FRONTEND_URL", 3100);

    // Load repo manifests (docker-compose discovery + .dev-launcher.conf)
    let copilot_manifest = if !no_copilot && paths.copilot.is_dir() {
        let mut m = load_repo_manifest(&paths.copilot, "Copilot");
        patch_manifest_ports(&mut m, copilot_backend_port, copilot_frontend_port);
        Some(m)
    } else { None };
    let opencti_manifest  = if !no_opencti   && paths.opencti.is_dir()    { Some(load_repo_manifest(&paths.opencti,   "OpenCTI"))   } else { None };
    let openaev_manifest  = if !no_openaev   && paths.openaev.is_dir()    { Some(load_repo_manifest(&paths.openaev,   "OpenAEV"))   } else { None };
    let _connector_manifest = if !no_connector && paths.connector.is_dir() { Some(load_repo_manifest(&paths.connector, "Connector")) } else { None };

    let mut copilot_docker_ok = true;
    let mut opencti_docker_ok = true;
    let mut openaev_docker_ok = true;
    // Collect every compose project brought up so we can tear them down on exit.
    let mut docker_projects: Vec<DockerProject> = Vec::new();

    if docker_ok {
        if !no_copilot && paths.copilot.is_dir() {
            let (dc, project) = if let Some(ref m) = copilot_manifest {
                let f = paths.copilot.join(m.docker.compose_dev.as_deref().unwrap_or("docker-compose.dev.yml"));
                (f, resolve_docker_project(&paths.copilot, m, &slug))
            } else {
                let f = paths.copilot.join("docker-compose.dev.yml");
                (f, ws_docker_project("copilot-dev", &slug))
            };
            if dc.exists() {
                let ov      = write_compose_override(&dc, &slug);
                let ov_str  = ov.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                let extra: Vec<&str> = if ov.is_some() { vec!["-f", &ov_str] } else { vec![] };
                copilot_docker_ok = docker_compose_up("Copilot", &project, &dc, &paths.copilot, &extra);
                docker_projects.push(DockerProject {
                    label: "Copilot".into(), project, compose_file: dc,
                    work_dir: paths.copilot.clone(), override_file: ov,
                });
            }
        }
        if !no_opencti && paths.opencti.is_dir() {
            let dc = paths.opencti.join("opencti-platform/opencti-dev/docker-compose.yml");
            if dc.exists() {
                let base = opencti_manifest.as_ref()
                    .and_then(|m| m.docker.project.clone())
                    .unwrap_or_else(|| "opencti-dev".to_string());
                let project = ws_docker_project(&base, &slug);
                let ov      = write_compose_override(&dc, &slug);
                let ov_str  = ov.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                let extra: Vec<&str> = if ov.is_some() { vec!["-f", &ov_str] } else { vec![] };
                opencti_docker_ok = docker_compose_up("OpenCTI", &project, &dc, &paths.opencti, &extra);
                docker_projects.push(DockerProject {
                    label: "OpenCTI".into(), project, compose_file: dc,
                    work_dir: paths.opencti.clone(), override_file: ov,
                });
            }
        }
        if !no_openaev && paths.openaev.is_dir() {
            let dev_dir = paths.openaev.join("openaev-dev");
            let dc = dev_dir.join("docker-compose.yml");
            if dc.exists() {
                let env_file = dev_dir.join(".env").to_string_lossy().into_owned();
                let base = openaev_manifest.as_ref()
                    .and_then(|m| m.docker.project.clone())
                    .unwrap_or_else(|| "openaev-dev".to_string());
                let project = ws_docker_project(&base, &slug);
                let ov      = write_compose_override(&dc, &slug);
                let ov_str  = ov.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                let mut extra: Vec<&str> = vec!["--env-file", &env_file];
                if ov.is_some() { extra.extend_from_slice(&["-f", &ov_str]); }
                openaev_docker_ok = docker_compose_up("OpenAEV", &project, &dc, &dev_dir, &extra);
                docker_projects.push(DockerProject {
                    label: "OpenAEV".into(), project, compose_file: dc,
                    work_dir: dev_dir, override_file: ov,
                });
            }
        }
    } else {
        copilot_docker_ok = false;
        opencti_docker_ok = false;
        openaev_docker_ok = false;
    }
    println!();

    // ── Spawn services ────────────────────────────────────────────────────────
    {
        let mut svcs = state.lock().unwrap();

        macro_rules! try_spawn {
            ($svc:expr, $prog:expr, $argv:expr, $dir:expr, $env:expr) => {{
                // Store spawn command so the service can be restarted via R key.
                $svc.spawn_cmd = Some(SpawnCmd {
                    prog:            $prog.to_string(),
                    args:            $argv.iter().map(|s| s.to_string()).collect(),
                    dir:             $dir.to_path_buf(),
                    env:             $env.clone(),
                    requires_docker: false,
                });
                // Pre-launch port gate: refuse to spawn if the target port is occupied.
                let port_conflict: Option<String> = $svc.url.as_deref()
                    .and_then(extract_url_port)
                    .and_then(port_in_use);
                if let Some(conflict) = port_conflict {
                    $svc.health = Health::Degraded(conflict);
                    svcs.push($svc);
                } else {
                let idx = svcs.len();
                match spawn_svc($prog, $argv, $dir, $env, &$svc.log_path) {
                    Ok((child, pgid)) => {
                        record_pid(&slug, child.id()); // track for orphan recovery
                        $svc.pid        = Some(child.id());
                        $svc.started_at = Some(Instant::now());
                        $svc.health     = if $svc.url.is_some() { Health::Launching } else { Health::Running };
                        svcs.push($svc);
                        procs.push(Proc { idx, pgid, child });
                    }
                    Err(e) => {
                        $svc.health = Health::Degraded(e.to_string());
                        svcs.push($svc);
                    }
                }
                } // end port-conflict else
            }};
        }

        // Copilot
        if !no_copilot && paths.copilot.is_dir() {
            let uses_manifest = copilot_manifest.as_ref().map_or(false, |m| !m.services.is_empty());
            if uses_manifest {
                let m = copilot_manifest.as_ref().unwrap();
                let _bootstrap_ok = run_manifest_bootstrap(&paths.copilot, m);
                let backend_env = copilot_backend_env(&paths.copilot);
                for def in &m.services {
                    let log_path = logs_dir.join(
                        def.log_name.clone().unwrap_or_else(|| format!("copilot-{}.log", def.name))
                    );
                    let (url, health_path) = split_health_url_parts(def.health.as_deref());
                    let mut svc = Svc::new(
                        format!("copilot-{}", def.name),
                        url, health_path, def.timeout_secs, log_path,
                    );
                    // Propagate requires from manifest (prefix with product for cross-product deps).
                    svc.requires = def.requires.clone();
                    if def.requires_docker && !copilot_docker_ok {
                        svc.health = Health::Degraded("Docker deps not running — start Docker first".into());
                        svcs.push(svc);
                        continue;
                    }
                    let work_dir = if def.cwd.is_empty() {
                        paths.copilot.clone()
                    } else {
                        paths.copilot.join(&def.cwd)
                    };
                    if def.args.is_empty() || !work_dir.is_dir() { svcs.push(svc); continue; }
                    let prog = if def.args[0].starts_with('.') {
                        work_dir.join(&def.args[0]).to_string_lossy().into_owned()
                    } else {
                        def.args[0].clone()
                    };
                    if (prog.starts_with('/') || prog.starts_with("./") || prog.contains("/."))
                       && !PathBuf::from(&prog).exists()
                    {
                        svc.health = Health::Degraded(format!("{} not found — run ./dev.sh once", &def.args[0]));
                        svcs.push(svc);
                        continue;
                    }
                    let rest: Vec<&str> = def.args[1..].iter().map(|s| s.as_str()).collect();
                    let empty_env: HashMap<String, String> = HashMap::new();
                    let env = if def.cwd == "backend" { &backend_env } else { &empty_env };
                    // Check requires before spawning — defer if deps aren't healthy yet.
                    if !def.requires.is_empty() {
                        let unmet: Vec<&str> = def.requires.iter()
                            .filter(|r| !svcs.iter().any(|s| &s.name == *r && s.is_healthy()))
                            .map(|s| s.as_str()).collect();
                        if !unmet.is_empty() {
                            svc.spawn_cmd = Some(SpawnCmd {
                                prog: prog.clone(), args: rest.iter().map(|s| s.to_string()).collect(),
                                dir: work_dir.clone(), env: env.clone(), requires_docker: def.requires_docker,
                            });
                            svc.health = Health::Degraded(format!("Waiting for {}…", unmet.join(", ")));
                            svcs.push(svc);
                            continue;
                        }
                    }
                    try_spawn!(svc, &prog, &rest, &work_dir, env);
                }
            } else {
                // Fallback: Copilot spawn using ports from workspace env.
                let backend_port_str  = copilot_backend_port.to_string();
                let backend_url       = format!("http://localhost:{copilot_backend_port}");
                let frontend_url      = format!("http://localhost:{copilot_frontend_port}");
                let backend_dir = paths.copilot.join("backend");
                let python = backend_dir.join(".venv/bin/python");
                let backend_env = copilot_backend_env(&paths.copilot);
                let mut svc = Svc::new("copilot-backend", Some(&backend_url), "/api/health", 120, logs_dir.join("copilot-backend.log"));
                if !copilot_docker_ok {
                    svc.health = Health::Degraded("Docker deps not running — start Docker first".into());
                    svcs.push(svc);
                } else if python.exists() {
                    try_spawn!(svc, python.to_str().unwrap(),
                        &["-m", "uvicorn", "app.main:application",
                          "--reload", "--host", "0.0.0.0", "--port", &backend_port_str,
                          "--timeout-graceful-shutdown", "3"],
                        &backend_dir, &backend_env);
                } else {
                    svc.health = Health::Degraded("venv missing — run ./dev.sh once to set up".into());
                    svcs.push(svc);
                }
                let mut svc = Svc::new("copilot-worker", None::<String>, "", 10, logs_dir.join("copilot-worker.log"));
                if !copilot_docker_ok {
                    svc.health = Health::Degraded("Docker deps not running".into());
                    svcs.push(svc);
                } else if python.exists() {
                    try_spawn!(svc, python.to_str().unwrap(), &["-m", "saq", "app.worker.settings"], &backend_dir, &backend_env);
                } else {
                    svc.health = Health::Degraded("venv missing".into());
                    svcs.push(svc);
                }
                let fe_dir = paths.copilot.join("frontend");
                ensure_copilot_fe_deps(&fe_dir);
                let mut svc = Svc::new("copilot-frontend", Some(&frontend_url), "", 90, logs_dir.join("copilot-frontend.log"));
                if fe_dir.is_dir() {
                    try_spawn!(svc, "yarn", &["dev"], &fe_dir, &HashMap::new());
                }
            }
        }

        // OpenCTI
        if !no_opencti && paths.opencti.is_dir() {
            let gql_dir = paths.opencti.join("opencti-platform/opencti-graphql");
            let mut gql_env: HashMap<String, String> = HashMap::new();
            if gql_dir.is_dir() {
                if !gql_dir.join("node_modules").is_dir() {
                    println!("  Installing OpenCTI graphql node deps…");
                    run_blocking("yarn", &["install"], &gql_dir);
                }
                if let Some(pypath) = ensure_opencti_graphql_python_deps(&gql_dir) {
                    gql_env.insert("PYTHONPATH".into(), pypath);
                }
                // Inject admin credentials from .env.dev so OpenCTI can initialise
                // the admin user on first boot (password/token must not be "ChangeMe").
                let env_file = gql_dir.join(".env.dev");
                if env_file.exists() {
                    for (k, v) in parse_env_file(&env_file) {
                        gql_env.insert(k, v);
                    }
                }
            }
            // Validate that the admin password has been changed from the default.
            // OpenCTI refuses to start with APP__ADMIN__PASSWORD=ChangeMe and the
            // crash message is buried in logs — catch it here with a clear hint.
            let opencti_password_ok = gql_env
                .get("APP__ADMIN__PASSWORD")
                .map(|p| !p.is_empty() && p != "ChangeMe")
                .unwrap_or(false);
            let mut svc = Svc::new("opencti-graphql", Some("http://localhost:4000"), "/health", 300, logs_dir.join("opencti-graphql.log"));
            if !opencti_docker_ok {
                svc.health = Health::Degraded("Docker deps not running — start Docker first".into());
                svcs.push(svc);
            } else if !opencti_password_ok {
                svc.health = Health::Degraded("APP__ADMIN__PASSWORD not set — run dev-feature again to fill in credentials".into());
                svcs.push(svc);
            } else if gql_dir.is_dir() {
                if !no_copilot && paths.copilot.is_dir() {
                    // Defer until copilot-backend is healthy so we can inject
                    // XTM__XTM_ONE_URL / TOKEN before opencti-graphql starts.
                    svc.requires  = vec!["copilot-backend".to_string()];
                    svc.spawn_cmd = Some(SpawnCmd {
                        prog: "yarn".to_string(),
                        args: vec!["start".to_string()],
                        dir:  gql_dir.clone(),
                        env:  gql_env.clone(),
                        requires_docker: true,
                    });
                    svc.health = Health::Degraded("Waiting for copilot-backend…".into());
                    svcs.push(svc);
                } else {
                    // Pre-flight: wipe any stale OpenCTI ES indices left over from a
                    // previous session so the init sequence succeeds without crashing.
                    wipe_opencti_es_indices_if_stale(9200);
                    try_spawn!(svc, "yarn", &["start"], &gql_dir, &gql_env);
                }
            }

            if !no_opencti_front {
                let front_dir = paths.opencti.join("opencti-platform/opencti-front");
                ensure_opencti_fe_deps(&front_dir);
                let mut svc = Svc::new("opencti-frontend", Some("http://localhost:3000"), "", 120, logs_dir.join("opencti-frontend.log"));
                if front_dir.is_dir() {
                    if !no_copilot && paths.copilot.is_dir() {
                        svc.requires  = vec!["copilot-backend".to_string()];
                        svc.spawn_cmd = Some(SpawnCmd {
                            prog: "yarn".to_string(),
                            args: vec!["dev".to_string()],
                            dir:  front_dir.clone(),
                            env:  HashMap::new(),
                            requires_docker: false,
                        });
                        svc.health = Health::Degraded("Waiting for copilot-backend…".into());
                        svcs.push(svc);
                    } else {
                        try_spawn!(svc, "yarn", &["dev"], &front_dir, &HashMap::new());
                    }
                }
            }
        }

        // OpenAEV
        if !no_openaev && paths.openaev.is_dir() {
            let mvn = maven_cmd(&paths.openaev);
            let api_dir = paths.openaev.join("openaev-api");

            // The health endpoint requires the configured key (default "ChangeMe").
            let mut svc = Svc::new(
                "openaev-backend",
                Some("http://localhost:8080"),
                "/api/health?health_access_key=ChangeMe",
                180,
                logs_dir.join("openaev-backend.log"),
            );
            if !openaev_docker_ok {
                svc.health = Health::Degraded("Docker deps not running — start Docker first".into());
                svcs.push(svc);
            } else if api_dir.is_dir() {
                if !no_copilot && paths.copilot.is_dir() {
                    // Defer until copilot-backend is healthy so we can inject
                    // OPENAEV_XTM_ONE_* before openaev-backend starts.
                    svc.requires  = vec!["copilot-backend".to_string()];
                    svc.spawn_cmd = Some(SpawnCmd {
                        prog: mvn.clone(),
                        args: ["spring-boot:run", "-Pdev", "-pl", "openaev-api",
                               "-Dspring-boot.run.profiles=dev"]
                                .iter().map(|s| s.to_string()).collect(),
                        dir:  paths.openaev.clone(),
                        env:  HashMap::new(),
                        requires_docker: true,
                    });
                    svc.health = Health::Degraded("Waiting for copilot-backend…".into());
                    svcs.push(svc);
                } else {
                    try_spawn!(svc, &mvn,
                        &["spring-boot:run", "-Pdev", "-pl", "openaev-api",
                          "-Dspring-boot.run.profiles=dev"],
                        &paths.openaev, &HashMap::new());
                }
            } else {
                svc.health = Health::Degraded("openaev-api/ not found".into());
                svcs.push(svc);
            }

            if !no_openaev_front {
                let fe_dir = paths.openaev.join("openaev-front");
                ensure_openaev_fe_deps(&fe_dir);
                let mut svc = Svc::new(
                    "openaev-frontend",
                    Some("http://localhost:3001"),
                    "",
                    90,
                    logs_dir.join("openaev-frontend.log"),
                );
                if fe_dir.is_dir() {
                    if !no_copilot && paths.copilot.is_dir() {
                        svc.requires  = vec!["copilot-backend".to_string()];
                        svc.spawn_cmd = Some(SpawnCmd {
                            prog: "yarn".to_string(),
                            args: vec!["start".to_string()],
                            dir:  fe_dir.clone(),
                            env:  HashMap::new(),
                            requires_docker: false,
                        });
                        svc.health = Health::Degraded("Waiting for copilot-backend…".into());
                        svcs.push(svc);
                    } else {
                        try_spawn!(svc, "yarn", &["start"], &fe_dir, &HashMap::new());
                    }
                }
            }
        }

        // Connector
        if !no_connector && paths.connector.is_dir() {
            let env_path = ensure_connector_env(&paths.connector);
            let venv     = ensure_connector_venv(&paths.connector);
            let python   = venv.join("bin/python");
            let src_dir  = paths.connector.join("src");
            let env      = parse_env_file(&env_path);

            let mut svc = Svc::new("connector", None::<String>, "", 30, logs_dir.join("connector.log"));
            // Connector needs OpenCTI graphql healthy before it can connect.
            svc.requires = vec!["opencti-graphql".to_string()];
            if let Some(reason) = validate_connector_env(&env) {
                // Fail fast with a clear message instead of letting the connector crash
                // immediately after spawn with a confusing Python traceback.
                svc.health = Health::Degraded(reason);
                svcs.push(svc);
            } else if src_dir.is_dir() && python.exists() {
                // Check if opencti-graphql is already healthy; if not, defer.
                let opencti_ready = svcs.iter().any(|s| s.name == "opencti-graphql" && s.is_healthy());
                if !opencti_ready {
                    let python_str = python.to_str().unwrap().to_string();
                    svc.spawn_cmd = Some(SpawnCmd {
                        prog: python_str.clone(),
                        args: vec!["main.py".to_string()],
                        dir:  src_dir.clone(),
                        env:  env.clone(),
                        requires_docker: false,
                    });
                    svc.health = Health::Degraded("Waiting for opencti-graphql…".into());
                    svcs.push(svc);
                } else {
                    try_spawn!(svc, python.to_str().unwrap(), &["main.py"], &src_dir, &env);
                }
            } else {
                svc.health = Health::Degraded("src/ or venv not found".into());
                svcs.push(svc);
            }
        }
    }

    // ── Health-probe thread ───────────────────────────────────────────────────
    {
        let state    = Arc::clone(&state);
        let stopping = Arc::clone(&stopping);
        thread::spawn(move || {
            loop {
                if stopping.load(Ordering::Relaxed) { return; }
                thread::sleep(Duration::from_secs(1));

                // Snapshot what needs probing — release the lock before any blocking I/O.
                // Without this, each 2-second probe timeout holds the lock and stalls the
                // main thread (render + input) for N × 2 s.
                let to_probe: Vec<(usize, String, bool, u64)> = {
                    let svcs = state.lock().unwrap();
                    svcs.iter().enumerate()
                        .filter(|(_, s)| !s.health.is_done())
                        .filter_map(|(i, s)| {
                            s.health_url().map(|url| {
                                let timed_out = s.started_at
                                    .map(|t| t.elapsed() > s.startup_timeout)
                                    .unwrap_or(false);
                                (i, url, timed_out, s.startup_timeout.as_secs())
                            })
                        })
                        .collect()
                }; // ← lock released here, before any probe() call

                // Probe each URL without holding the state lock.
                for (i, url, timed_out, timeout_secs) in to_probe {
                    let ok = !timed_out && probe(&url);

                    let mut svcs = state.lock().unwrap();
                    if svcs[i].health.is_done() { continue; } // may have crashed while we probed
                    svcs[i].health = if ok {
                        Health::Up
                    } else if timed_out {
                        Health::Degraded(format!("no response after {timeout_secs}s"))
                    } else {
                        match &svcs[i].health {
                            Health::Probing(n) => Health::Probing(n + 1),
                            _                  => Health::Probing(1),
                        }
                    };
                    // ← lock released between probes
                }
            }
        });
    }

    // ── TUI setup ─────────────────────────────────────────────────────────────
    let mut raw_mode: Option<TuiGuard> = TuiGuard::enter();
    let has_tui = raw_mode.is_some();
    let mut mode = Mode::Overview { cursor: 0 };
    let mut creds: Vec<CredEntry> = Vec::new(); // populated on first Credentials entry

    let (tx, rx) = mpsc::sync_channel::<InputEvent>(32);
    if has_tui {
        spawn_input_thread(tx, Arc::clone(&stopping));
    }

    // ── Main loop ─────────────────────────────────────────────────────────────
    // The loop ticks every 20 ms so input feels instant (~20–40 ms latency).
    // A full re-render is triggered immediately on any input event, and on a
    // 500 ms timer for health-status updates. This avoids spamming the terminal
    // with unnecessary clears while staying responsive to keypresses.
    print!("\x1b[2J");
    let render_interval  = Duration::from_millis(500);
    let mut last_render  = Instant::now();
    let mut force_render = true; // draw immediately on first iteration

    loop {
        // ── Shutdown ─────────────────────────────────────────────────────────
        if stopping.load(Ordering::Relaxed) || SIGHUP_STOP.load(Ordering::Relaxed) {
            drop(raw_mode.take()); // restore terminal first

            // Build (name, proc_index_opt) pairs for every visible service.
            // Services that never had an active proc (Degraded/Crashed at spawn)
            // get None — they are shown as "already stopped".
            let pairs: Vec<(String, Option<usize>)> = {
                let svcs = state.lock().unwrap();
                svcs.iter().enumerate()
                    .filter(|(_, s)| s.health != Health::Pending)
                    .map(|(svc_i, s)| {
                        let proc_j = procs.iter().position(|p| p.idx == svc_i);
                        (s.name.clone(), proc_j)
                    })
                    .collect()
            };

            // Send SIGTERM to every active process group.
            eprintln!("[dev-feature] Stopping {} process(es)…", procs.len());
            // Compute per-process kill deadline: opencti-graphql gets 3 min, others 5 s.
            let kill_deadlines: Vec<Instant> = {
                let svcs = state.lock().unwrap();
                procs.iter().map(|p| {
                    let grace_secs = if svcs.get(p.idx).map(|s| s.name.as_str()) == Some("opencti-graphql") {
                        180
                    } else {
                        5
                    };
                    Instant::now() + Duration::from_secs(grace_secs)
                }).collect()
            };
            for p in &mut procs {
                eprintln!("[dev-feature]   SIGTERM → pgid -{} (svc #{})", p.pgid, p.idx);
                p.kill();
            }

            let mut term_status: Vec<TermStatus> = procs.iter()
                .map(|_| TermStatus::Terminating)
                .collect();

            let started  = Instant::now();
            let mut timed_out = false;

            loop {
                // Poll for exited processes.
                for (j, p) in procs.iter_mut().enumerate() {
                    if term_status[j] == TermStatus::Terminating {
                        if let Some(code) = p.try_reap() {
                            term_status[j] = TermStatus::Stopped(code);
                        }
                    }
                }

                // Force-kill each process that has exceeded its individual grace deadline.
                let now = Instant::now();
                for (j, p) in procs.iter_mut().enumerate() {
                    if term_status[j] == TermStatus::Terminating && now >= kill_deadlines[j] {
                        let secs = kill_deadlines[j].duration_since(Instant::now().min(kill_deadlines[j]));
                        let _ = secs; // deadline already passed
                        eprintln!("[dev-feature]   SIGKILL → pgid -{} (grace period exceeded)", p.pgid);
                        unsafe { libc::kill(-p.pgid, libc::SIGKILL); }
                        term_status[j] = TermStatus::Killed;
                        timed_out = true;
                    }
                }

                render_shutdown(&slug, &pairs, &term_status, started.elapsed(), timed_out);

                let all_done = term_status.iter().all(|s| *s != TermStatus::Terminating);
                if all_done {
                    // One final render so the user sees "All processes stopped."
                    render_shutdown(&slug, &pairs, &term_status, started.elapsed(), timed_out);
                    thread::sleep(Duration::from_millis(600));
                    let _ = fs::remove_file(pid_file_path(&slug));
                    eprintln!("[dev-feature] All processes stopped. PID file removed.");
                    // Tear down Docker Compose projects started by this session.
                    if !docker_projects.is_empty() {
                        print!("\r\n");
                        for dp in &docker_projects {
                            docker_compose_down(dp);
                        }
                    }
                    break;
                }

                thread::sleep(Duration::from_millis(100));
            }

            break;
        }

        // ── Crash detection ───────────────────────────────────────────────────
        {
            let mut svcs = state.lock().unwrap();
            for p in &mut procs {
                if let Some(code) = p.try_reap() {
                    let already_crashed = matches!(svcs[p.idx].health, Health::Crashed(_));
                    svcs[p.idx].health = Health::Crashed(code);
                    force_render = true;

                    // Spawn diagnosis for the first crash only.
                    if !already_crashed && !diagnosed.contains(&p.idx) {
                        diagnosed.insert(p.idx);
                        let log_path = svcs[p.idx].log_path.clone();
                        let svc_idx  = p.idx;
                        let tx       = diag_tx.clone();
                        let llm      = llm_cfg.clone();
                        thread::spawn(move || {
                            // Small delay so the log has time to flush.
                            thread::sleep(Duration::from_millis(300));
                            if let Some(msg) = diagnose_crash(&log_path, llm.as_ref()) {
                                let _ = tx.send(DiagEvent::Result { svc_idx, msg });
                            }
                        });

                        // Auto-jump to Diagnose view on first crash when in Overview.
                        // This puts the user directly on the findings screen (with r / fixes)
                        // instead of requiring them to navigate Log → d → Diagnose manually.
                        if has_tui && matches!(mode, Mode::Overview { .. }) {
                            let findings = diagnose_service(&svcs[p.idx], &paths, &ws_env_dir);
                            mode = Mode::Diagnose { svc_idx: p.idx, findings, cursor: 0 };
                        }
                    }
                }
            }
        }

        // ── Receive diagnosis results ─────────────────────────────────────────
        while let Ok(DiagEvent::Result { svc_idx, msg }) = diag_rx.try_recv() {
            let mut svcs = state.lock().unwrap();
            if let Some(svc) = svcs.get_mut(svc_idx) {
                svc.diagnosis = Some(msg);
            }
            force_render = true;
        }

        // ── Auto-spawn services waiting on requires ───────────────────────────
        // Collect candidates + the copilot frontend URL in a single lock pass,
        // then do all blocking work (env injection, ES wipe, spawn) outside it.
        let (spawn_candidates, copilot_frontend_url): (Vec<(usize, String, SpawnCmd, PathBuf)>, Option<String>) = {
            let svcs = state.lock().unwrap();
            let url = svcs.iter()
                .find(|s| s.name == "copilot-frontend")
                .and_then(|s| s.url.clone());
            let candidates = svcs.iter().enumerate()
                .filter(|(_, s)| s.is_waiting_for_requires())
                .filter(|(_, s)| {
                    s.requires.iter().all(|req| svcs.iter().any(|o| &o.name == req && o.is_healthy()))
                })
                .filter_map(|(i, s)| {
                    s.spawn_cmd.clone().map(|cmd| (i, s.name.clone(), cmd, s.log_path.clone()))
                })
                .collect();
            (candidates, url)
        };
        for (idx, svc_name, mut cmd, log_path) in spawn_candidates {
            // Pre-spawn hooks: inject XTM-One integration env vars into OCTI / OAEV
            // as soon as copilot-frontend is healthy and its URL is known.
            if let Some(ref url) = copilot_frontend_url {
                match svc_name.as_str() {
                    "opencti-graphql" => {
                        // ES pre-flight: wipe stale indices before graphql starts.
                        wipe_opencti_es_indices_if_stale(9200);
                        // Update workspace copy + re-deploy so restarts also pick it up.
                        let ws_file   = ws_env_path(&ws_env_dir, "opencti");
                        let repo_file = paths.opencti.join("opencti-platform/opencti-graphql/.env.dev");
                        if ws_file.exists() {
                            let mut fenv = parse_env_file(&ws_file);
                            fenv.insert("XTM__XTM_ONE_URL".to_string(),   url.clone());
                            fenv.insert("XTM__XTM_ONE_TOKEN".to_string(), "xtm-default-registration-token".to_string());
                            write_env_file(&ws_file, &fenv);
                            deploy_workspace_env(&ws_file, &repo_file);
                        }
                        // Also inject into the SpawnCmd env so this run picks it up.
                        cmd.env.insert("XTM__XTM_ONE_URL".to_string(),   url.clone());
                        cmd.env.insert("XTM__XTM_ONE_TOKEN".to_string(), "xtm-default-registration-token".to_string());
                    }
                    "openaev-backend" => {
                        // Update workspace copy + re-deploy.
                        let ws_file   = ws_env_path(&ws_env_dir, "openaev");
                        let repo_file = paths.openaev.join("openaev-dev/.env");
                        if ws_file.exists() {
                            let mut fenv = parse_env_file(&ws_file);
                            fenv.insert("OPENAEV_XTM_ONE_ENABLE".to_string(), "true".to_string());
                            fenv.insert("OPENAEV_XTM_ONE_URL".to_string(),    url.clone());
                            fenv.insert("OPENAEV_XTM_ONE_TOKEN".to_string(),  "xtm-default-registration-token".to_string());
                            write_env_file(&ws_file, &fenv);
                            deploy_workspace_env(&ws_file, &repo_file);
                        }
                        cmd.env.insert("OPENAEV_XTM_ONE_ENABLE".to_string(), "true".to_string());
                        cmd.env.insert("OPENAEV_XTM_ONE_URL".to_string(),    url.clone());
                        cmd.env.insert("OPENAEV_XTM_ONE_TOKEN".to_string(),  "xtm-default-registration-token".to_string());
                    }
                    _ => {}
                }
            }
            let args: Vec<&str> = cmd.args.iter().map(|s| s.as_str()).collect();
            match spawn_svc(&cmd.prog, &args, &cmd.dir, &cmd.env, &log_path) {
                Ok((child, pgid)) => {
                    record_pid(&slug, child.id());
                    let mut svcs = state.lock().unwrap();
                    let has_url = svcs[idx].url.is_some();
                    svcs[idx].health     = if has_url { Health::Launching } else { Health::Running };
                    svcs[idx].pid        = Some(child.id());
                    svcs[idx].started_at = Some(Instant::now());
                    procs.push(Proc { idx, pgid, child });
                }
                Err(e) => {
                    let mut svcs = state.lock().unwrap();
                    svcs[idx].health = Health::Degraded(e.to_string());
                }
            }
            force_render = true;
        }

        // ── Input handling ────────────────────────────────────────────────────
        let mut got_input = false;
        if has_tui {
            let visible_count = state.lock().unwrap()
                .iter().filter(|s| s.health != Health::Pending).count();

            while let Ok(event) = rx.try_recv() {
                got_input = true;
                match &mut mode {
                    Mode::Overview { cursor } => match event {
                        InputEvent::Up   => { *cursor = cursor.saturating_sub(1); }
                        InputEvent::Down => {
                            if visible_count > 0 {
                                *cursor = (*cursor + 1).min(visible_count - 1);
                            }
                        }
                        InputEvent::Enter => {
                            let svcs = state.lock().unwrap();
                            let visible: Vec<usize> = svcs.iter().enumerate()
                                .filter(|(_, s)| s.health != Health::Pending)
                                .map(|(i, _)| i)
                                .collect();
                            if let Some(&svc_idx) = visible.get(*cursor) {
                                drop(svcs);
                                mode = Mode::LogView { svc_idx, scroll: 0, follow: true };
                            }
                        }
                        // d — open diagnosis directly from the overview (skip the log view)
                        InputEvent::Diagnose => {
                            let svcs = state.lock().unwrap();
                            let visible: Vec<usize> = svcs.iter().enumerate()
                                .filter(|(_, s)| s.health != Health::Pending)
                                .map(|(i, _)| i)
                                .collect();
                            if let Some(&idx) = visible.get(*cursor) {
                                let findings = diagnose_service(&svcs[idx], &paths, &ws_env_dir);
                                drop(svcs);
                                mode = Mode::Diagnose { svc_idx: idx, findings, cursor: 0 };
                            }
                        }
                        // R — kill and re-spawn the highlighted service
                        InputEvent::Restart => {
                            let visible: Vec<usize> = {
                                let svcs = state.lock().unwrap();
                                svcs.iter().enumerate()
                                    .filter(|(_, s)| s.health != Health::Pending)
                                    .map(|(i, _)| i)
                                    .collect()
                            };
                            if let Some(&idx) = visible.get(*cursor) {
                                let (cmd, log_path) = {
                                    let svcs = state.lock().unwrap();
                                    (svcs[idx].spawn_cmd.clone(), svcs[idx].log_path.clone())
                                };
                                if let Some(cmd) = cmd {
                                    // Kill existing process if still running.
                                    if let Some(pos) = procs.iter().position(|p| p.idx == idx) {
                                        unsafe { libc::kill(-(procs[pos].pgid as i32), libc::SIGKILL); }
                                        procs.remove(pos);
                                    }
                                    // Re-check Docker if the service needs it.
                                    let docker_ok = !cmd.requires_docker || docker_available();
                                    if !docker_ok {
                                        let mut svcs = state.lock().unwrap();
                                        svcs[idx].health = Health::Degraded("Docker not running — start Docker first".into());
                                    } else {
                                        let args: Vec<&str> = cmd.args.iter().map(|s| s.as_str()).collect();
                                        match spawn_svc(&cmd.prog, &args, &cmd.dir, &cmd.env, &log_path) {
                                            Ok((child, pgid)) => {
                                                let mut svcs = state.lock().unwrap();
                                                let has_url = svcs[idx].url.is_some();
                                                record_pid(&slug, child.id());
                                                svcs[idx].health     = if has_url { Health::Launching } else { Health::Running };
                                                svcs[idx].pid        = Some(child.id());
                                                svcs[idx].started_at = Some(Instant::now());
                                                svcs[idx].diagnosis  = None;
                                                procs.push(Proc { idx, pgid, child });
                                            }
                                            Err(e) => {
                                                let mut svcs = state.lock().unwrap();
                                                svcs[idx].health = Health::Degraded(e.to_string());
                                            }
                                        }
                                    }
                                    force_render = true;
                                }
                            }
                        }
                        InputEvent::Back        => { stopping.store(true, Ordering::Relaxed); }
                        InputEvent::Credentials => {
                            creds = gather_credentials(&ws_env_dir, &paths);
                            mode  = Mode::Credentials;
                        }
                        _ => {}
                    },
                    Mode::LogView { svc_idx, scroll, follow } => match event {
                        InputEvent::Back     => { mode = Mode::Overview { cursor: 0 }; }
                        InputEvent::Up       => { *scroll += 5;  *follow = false; }
                        InputEvent::Down     => {
                            *scroll = scroll.saturating_sub(5);
                            if *scroll == 0 { *follow = true; }
                        }
                        InputEvent::PageUp   => { *scroll += 20; *follow = false; }
                        InputEvent::PageDown => {
                            *scroll = scroll.saturating_sub(20);
                            if *scroll == 0 { *follow = true; }
                        }
                        InputEvent::Follow   => { *scroll = 0; *follow = true; }
                        InputEvent::Diagnose => {
                            let idx = *svc_idx;
                            let findings = {
                                let svcs = state.lock().unwrap();
                                svcs.get(idx).map(|svc| diagnose_service(svc, &paths, &ws_env_dir))
                                    .unwrap_or_default()
                            };
                            mode = Mode::Diagnose { svc_idx: idx, findings, cursor: 0 };
                        }
                        _ => {}
                    },
                    Mode::Diagnose { cursor, findings, svc_idx } => match event {
                        InputEvent::Back => {
                            let idx = *svc_idx;
                            mode = Mode::LogView { svc_idx: idx, scroll: 0, follow: true };
                        }
                        InputEvent::Up => {
                            *cursor = cursor.saturating_sub(1);
                        }
                        InputEvent::Down => {
                            if *cursor + 1 < findings.len() { *cursor += 1; }
                        }
                        InputEvent::Enter => {
                            // Run the fix for the current finding, if any.
                            let idx        = *svc_idx;
                            let cur        = *cursor;
                            let fix_action = findings.get(cur).and_then(|f| f.fix.clone());
                            if let Some(action) = fix_action {
                                let wants_restart = action.restart_after();
                                // Restore terminal, run the fix with visible output.
                                drop(raw_mode.take());
                                ensure_cooked_output();
                                print!("\x1b[H\x1b[2J");
                                let _ = io::stdout().flush();
                                let fix_ok = run_fix_action(&action);

                                // If the fix requested a service restart and all steps
                                // succeeded, re-spawn the service immediately.
                                if fix_ok && wants_restart {
                                    let (cmd, log_path) = {
                                        let svcs = state.lock().unwrap();
                                        svcs.get(idx).map(|s| (s.spawn_cmd.clone(), s.log_path.clone()))
                                            .unwrap_or_default()
                                    };
                                    if let Some(cmd) = cmd {
                                        // Kill any still-running process.
                                        if let Some(pos) = procs.iter().position(|p| p.idx == idx) {
                                            unsafe { libc::kill(-(procs[pos].pgid as i32), libc::SIGKILL); }
                                            procs.remove(pos);
                                        }
                                        let args: Vec<&str> = cmd.args.iter().map(|s| s.as_str()).collect();
                                        match spawn_svc(&cmd.prog, &args, &cmd.dir, &cmd.env, &log_path) {
                                            Ok((child, pgid)) => {
                                                let mut svcs = state.lock().unwrap();
                                                let has_url = svcs[idx].url.is_some();
                                                record_pid(&slug, child.id());
                                                svcs[idx].health     = if has_url { Health::Launching } else { Health::Running };
                                                svcs[idx].pid        = Some(child.id());
                                                svcs[idx].started_at = Some(Instant::now());
                                                svcs[idx].diagnosis  = None;
                                                procs.push(Proc { idx, pgid, child });
                                                println!("\n  {GRN}✓{R}  Service restarted — returning to overview.");
                                            }
                                            Err(e) => {
                                                let mut svcs = state.lock().unwrap();
                                                svcs[idx].health = Health::Degraded(e.to_string());
                                                println!("\n  {RED}✗{R}  Restart failed: {e}");
                                            }
                                        }
                                    }
                                    println!();
                                    thread::sleep(Duration::from_millis(1200));
                                    raw_mode = TuiGuard::enter();
                                    mode = Mode::Overview { cursor: 0 };
                                    force_render = true;
                                } else {
                                    println!();
                                    thread::sleep(Duration::from_millis(1200));
                                    raw_mode = TuiGuard::enter();
                                    // Re-run diagnosis; advance cursor to the first still-actionable finding.
                                    let new_findings = {
                                        let svcs = state.lock().unwrap();
                                        svcs.get(idx).map(|svc| diagnose_service(svc, &paths, &ws_env_dir))
                                            .unwrap_or_default()
                                    };
                                    let new_cursor = new_findings.iter()
                                        .position(|f| f.fix.is_some() && !f.resolved)
                                        .unwrap_or(0);
                                    mode = Mode::Diagnose {
                                        svc_idx:  idx,
                                        findings: new_findings,
                                        cursor:   new_cursor,
                                    };
                                    force_render = true;
                                }
                            }
                        }
                        InputEvent::Report => {
                            let idx = *svc_idx;
                            let cur = *cursor;
                            let finding = findings.get(cur).cloned();
                            if let Some(f) = finding {
                                if needs_recipe(&f) {
                                    let ctx = {
                                        let svcs = state.lock().unwrap();
                                        svcs.get(idx).map(|s| IssueContext {
                                            health:      s.health.label_plain(),
                                            uptime_secs: s.secs(),
                                            log_path:    s.log_path.clone(),
                                            spawn_cmd:   s.spawn_cmd.as_ref().map(|c| {
                                                let mut parts = vec![c.prog.clone()];
                                                parts.extend(c.args.iter().cloned());
                                                parts.join(" ")
                                            }),
                                        })
                                    };
                                    let Some(ctx) = ctx else { continue };
                                    let svc_name = {
                                        let svcs = state.lock().unwrap();
                                        svcs.get(idx).map(|s| s.name.clone()).unwrap_or_default()
                                    };
                                    let log_tail = tail_file(&ctx.log_path, 15);
                                    drop(raw_mode.take());
                                    let _ = disable_raw_mode();
                                    ensure_cooked_output();

                                    // Use \r\n throughout — safe regardless of raw mode state.
                                    let p = |s: &str| { print!("{s}\r\n"); };
                                    print!("\x1b[H\x1b[2J\r");
                                    let _ = io::stdout().flush();
                                    p(&format!("\r\n  {BOLD}Report missing recipe{R}"));
                                    p("");
                                    p(&format!("  Service : {CYN}{svc_name}{R}"));
                                    p(&format!("  Health  : {}", ctx.health));
                                    if ctx.uptime_secs > 0 {
                                        p(&format!("  Uptime  : {}s", ctx.uptime_secs));
                                    }
                                    if let Some(ref cmd) = ctx.spawn_cmd {
                                        let short: String = cmd.chars().take(72).collect();
                                        let ellipsis = if cmd.len() > 72 { "…" } else { "" };
                                        p(&format!("  Command : {DIM}{short}{ellipsis}{R}"));
                                    }
                                    p(&format!("  Finding : {}", f.title));
                                    p(&format!("  Kind    : {DIM}{}{R}", f.kind));
                                    if !log_tail.is_empty() {
                                        p("");
                                        p(&format!("  {DIM}Logs ({} lines):{R}", log_tail.len()));
                                        let preview: Vec<_> = log_tail.iter().rev().take(8).rev().collect();
                                        for line in &preview {
                                            p(&format!("  {DIM}│{R} {line}"));
                                        }
                                        if log_tail.len() > 8 {
                                            p(&format!("  {DIM}  … ({} more lines in issue){R}", log_tail.len() - 8));
                                        }
                                    }
                                    p("");
                                    p("  This will open an issue at AreDee-Bangs/dev-launcher");
                                    p("  so the recipe can be implemented.");
                                    p("");
                                    p(&format!("  {CYN}Enter{R} create issue   {DIM}q / Esc{R} cancel"));
                                    let _ = io::stdout().flush();

                                    // Enable raw mode only — no alternate screen — so the text
                                    // above stays visible while we wait for the keypress.
                                    let _ = enable_raw_mode();
                                    let confirmed = loop {
                                        if stopping.load(Ordering::Relaxed) { break false; }
                                        if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                                            if let Ok(Event::Key(k)) = event::read() {
                                                match k.code {
                                                    KeyCode::Enter => break true,
                                                    KeyCode::Char('q') | KeyCode::Esc => break false,
                                                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                                                        stopping.store(true, Ordering::Relaxed);
                                                        break false;
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                    };
                                    let _ = disable_raw_mode();

                                    if confirmed {
                                        print!("\r\n  Creating issue…\r\n");
                                        let _ = io::stdout().flush();
                                        match create_github_issue(f.kind, &svc_name, &f.title, &f.body, &log_tail, &ctx) {
                                            Ok(url)  => print!("\r  {GRN}✓{R}  Issue created: {url}\r\n"),
                                            Err(err) => print!("\r  {RED}✗{R}  Failed: {err}\r\n"),
                                        }
                                    } else {
                                        print!("\r\n  Cancelled.\r\n");
                                    }
                                    // Brief pause so the user can read the result, then
                                    // re-enter the TUI. We cannot use wait_for_any_key()
                                    // here because the input background thread is also
                                    // calling event::read() and would race to steal events.
                                    print!("\r\n  Returning to diagnosis…\r\n");
                                    let _ = io::stdout().flush();
                                    thread::sleep(Duration::from_millis(1500));
                                    raw_mode = TuiGuard::enter();
                                    let new_findings = {
                                        let svcs = state.lock().unwrap();
                                        svcs.get(idx).map(|svc| diagnose_service(svc, &paths, &ws_env_dir))
                                            .unwrap_or_default()
                                    };
                                    mode = Mode::Diagnose {
                                        svc_idx:  idx,
                                        findings: new_findings,
                                        cursor:   cur.min(findings.len().saturating_sub(1)),
                                    };
                                    force_render = true;
                                }
                            }
                        }
                        _ => {}
                    },
                    Mode::Credentials => match event {
                        InputEvent::Back => { mode = Mode::Overview { cursor: 0 }; }
                        _ => {}
                    },
                }
            }

            // Clamp cursor after service list may have changed.
            if let Mode::Overview { cursor } = &mut mode {
                if visible_count > 0 { *cursor = (*cursor).min(visible_count - 1); }
            }
        }

        // ── Render ────────────────────────────────────────────────────────────
        // Re-render immediately on any input event; otherwise on the periodic timer.
        if got_input || force_render || last_render.elapsed() >= render_interval {
            if let Some(tui) = raw_mode.as_mut() {
                let svcs = state.lock().unwrap();
                let lines = match &mode {
                    Mode::Overview { cursor } =>
                        build_overview_lines(&svcs, &slug, &logs_dir, *cursor, has_tui),
                    Mode::LogView { svc_idx, scroll, follow } =>
                        svcs.get(*svc_idx)
                            .map(|svc| build_log_view_lines(svc, *scroll, *follow))
                            .unwrap_or_default(),
                    Mode::Diagnose { svc_idx, findings, cursor } =>
                        svcs.get(*svc_idx)
                            .map(|svc| build_diagnose_lines(svc, findings, *cursor))
                            .unwrap_or_default(),
                    Mode::Credentials =>
                        build_credentials_lines(&creds, &slug),
                };
                drop(svcs);
                draw_ansi_lines(tui, &lines);
            }
            last_render  = Instant::now();
            force_render = false;
        }

        thread::sleep(Duration::from_millis(20));
    }
}
