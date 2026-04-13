//! dev-feature — multi-product stack launcher with process-tree management and health monitoring.
//!
//! Spawns every service in its own process group, polls health endpoints concurrently,
//! and terminates the entire tree on Ctrl+C — no orphan processes.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use std::thread;

use clap::Parser;

// ── ANSI ──────────────────────────────────────────────────────────────────────

const R:    &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM:  &str = "\x1b[2m";
const GRN:  &str = "\x1b[32m";
const YLW:  &str = "\x1b[33m";
const RED:  &str = "\x1b[31m";
const CYN:  &str = "\x1b[36m";

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name    = "dev-feature",
    about   = "Launch the full multi-product dev stack for a feature branch.\n\
               Each service runs in its own process group; Ctrl+C kills the entire tree.",
    version
)]
struct Args {
    /// Branch slug, e.g. `importdocai-extraction-selection`.
    /// Auto-detected from the HEAD of filigran-copilot if omitted.
    branch: Option<String>,

    /// Skip Filigran Copilot (backend, worker, frontend).
    #[arg(long)] no_copilot: bool,

    /// Skip OpenCTI (docker deps + graphql + frontend).
    #[arg(long)] no_opencti: bool,

    /// Skip the OpenCTI React frontend only.
    #[arg(long)] no_opencti_front: bool,

    /// Skip the import-document-ai connector.
    #[arg(long)] no_connector: bool,

    /// Directory for per-service log files.
    #[arg(long, default_value = "/tmp/dev-feature-logs")]
    logs_dir: PathBuf,
}

// ── Health state ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Health {
    Pending,
    Launching,
    Probing(u32),   // attempt count
    Up,
    Running,        // alive with no HTTP endpoint to probe
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

    fn is_done(&self) -> bool {
        matches!(self, Health::Up | Health::Running | Health::Degraded(_) | Health::Crashed(_))
    }
}

// ── Service display state (shared with health thread) ─────────────────────────

#[derive(Debug)]
struct Svc {
    name:            &'static str,
    /// Base URL shown in the table and used to build the health probe URL.
    url:             Option<&'static str>,
    /// Path appended to `url` for the health probe.
    health_path:     &'static str,
    health:          Health,
    pid:             Option<u32>,
    started_at:      Option<Instant>,
    startup_timeout: Duration,
    log_path:        PathBuf,
}

impl Svc {
    fn new(
        name: &'static str,
        url: Option<&'static str>,
        health_path: &'static str,
        timeout_secs: u64,
        log_path: PathBuf,
    ) -> Self {
        Self {
            name,
            url,
            health_path,
            health: Health::Pending,
            pid: None,
            started_at: None,
            startup_timeout: Duration::from_secs(timeout_secs),
            log_path,
        }
    }

    fn health_url(&self) -> Option<String> {
        self.url.map(|b| format!("{b}{}", self.health_path))
    }

    fn secs(&self) -> u64 {
        self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0)
    }
}

type State = Arc<Mutex<Vec<Svc>>>;

// ── Managed children (owned by main thread) ───────────────────────────────────

struct Proc {
    idx:  usize,  // index into the shared State Vec
    pgid: i32,
    child: Child,
}

impl Proc {
    /// Send SIGTERM to the entire process group.
    fn kill(&mut self) {
        unsafe { libc::kill(-self.pgid, libc::SIGTERM); }
    }

    /// Non-blocking exit check; returns Some(exit_code) if the process has terminated.
    fn try_reap(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().map(|s| s.code().unwrap_or(-1))
    }
}

// ── Paths ─────────────────────────────────────────────────────────────────────

struct Paths {
    copilot:   PathBuf,
    opencti:   PathBuf,
    connector: PathBuf,
}

impl Paths {
    fn resolve(workspace: &Path, slug: &str) -> Self {
        let pick = |product: &str| -> PathBuf {
            let feature = workspace.join(format!("{}-{}", product, slug));
            if feature.is_dir() { feature } else { workspace.join(product) }
        };
        let connector_root = pick("connectors")
            .join("internal-import-file/import-document-ai");
        Self {
            copilot:   pick("filigran-copilot"),
            opencti:   pick("opencti"),
            connector: connector_root,
        }
    }
}

// ── Low-level helpers ─────────────────────────────────────────────────────────

/// Parse a KEY=VALUE env file; ignores blanks and `#` comments.
fn parse_env_file(path: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(f) = File::open(path) else { return out };
    for line in io::BufReader::new(f).lines().flatten() {
        let line = line.trim().to_string();
        if line.is_empty() || line.starts_with('#') { continue; }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.into(), v.trim_matches('"').trim_matches('\'').into());
        }
    }
    out
}

/// Open (or create/append) a log file.
fn open_log(path: &Path) -> File {
    OpenOptions::new().create(true).append(true).open(path)
        .unwrap_or_else(|_| panic!("cannot open log {}", path.display()))
}

/// Run a blocking command (e.g. docker compose up -d) and return its exit code.
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

/// Spawn a long-running process in a **new process group**, piping stdout/stderr
/// to `log`. Returns the child and its process-group ID.
fn spawn(
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

    // Create a new process group — pgid == child pid after process_group(0).
    cmd.process_group(0);

    let child = cmd.spawn()?;
    let pgid  = child.id() as i32;
    Ok((child, pgid))
}

/// Single non-blocking HTTP probe. Returns true when the server responds with
/// any status below 500 (even a 403 means it is alive).
fn probe(url: &str) -> bool {
    ureq::get(url)
        .timeout(Duration::from_secs(2))
        .call()
        .map(|r| r.status() < 500)
        .unwrap_or(false)
}

/// Detect branch slug from git HEAD of the main worktrees.
fn detect_slug(workspace: &Path) -> Option<String> {
    for repo in &["filigran-copilot", "opencti"] {
        let dir = workspace.join(repo);
        if !dir.is_dir() { continue; }
        if let Ok(out) = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&dir)
            .output()
        {
            let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let slug   = branch.split_once('/').map_or(branch.as_str(), |(_, s)| s).to_string();
            if !slug.is_empty() && slug != "main" && slug != "master" {
                return Some(slug);
            }
        }
    }
    None
}

/// Find the workspace root by walking up from the running binary until we find
/// a directory that contains `filigran-copilot/`.
fn find_workspace() -> PathBuf {
    std::env::current_exe().ok()
        .and_then(|p| {
            p.ancestors()
                .find(|a| a.join("filigran-copilot").is_dir())
                .map(|p| p.to_path_buf())
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap())
}

// ── Connector bootstrapping ───────────────────────────────────────────────────

fn ensure_connector_env(dir: &Path) -> PathBuf {
    let path = dir.join(".env.dev");
    if !path.exists() {
        let _ = fs::write(&path, "\
# Connector dev environment — fill in before running\n\
OPENCTI_URL=http://localhost:4000\n\
OPENCTI_TOKEN=ChangeMe\n\
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
    }
    path
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

// ── Display ───────────────────────────────────────────────────────────────────

fn render(svcs: &[Svc], slug: &str, logs_dir: &Path) {
    // Move cursor to (1,1) and clear screen.
    print!("\x1b[H\x1b[2J");

    println!("\n  {BOLD}{CYN}dev-feature{R}  {DIM}{slug}{R}\n");
    println!("  {BOLD}{:<26}{:<34}{:<7}{R}", "Service", "Status", "PID");
    println!("  {DIM}{}{R}", "─".repeat(67));

    for s in svcs {
        if s.health == Health::Pending { continue; }
        let pid = s.pid.map(|p| p.to_string()).unwrap_or_else(|| "—".to_string());
        let url = s.url.map(|u| format!("  {DIM}{u}{R}")).unwrap_or_default();
        let elapsed = if s.started_at.is_some() {
            format!("  {DIM}{}s{R}", s.secs())
        } else {
            String::new()
        };
        println!("  {:<26}{:<46}{:<7}{url}{elapsed}", s.name, s.health.label(), pid);
    }

    println!();

    let active: Vec<_> = svcs.iter().filter(|s| s.health != Health::Pending).collect();
    let all_up = !active.is_empty()
        && active.iter().all(|s| matches!(s.health, Health::Up | Health::Running));
    let any_bad = active.iter().any(|s| {
        matches!(s.health, Health::Crashed(_) | Health::Degraded(_))
    });

    if any_bad {
        println!("  {RED}{BOLD}One or more services failed.{R}");
    } else if all_up {
        println!("  {GRN}{BOLD}All services up.{R}  Ctrl+C to stop.");
    } else {
        println!("  Waiting for services…  Ctrl+C to stop.");
    }
    println!("  {DIM}tail -f {}/*.log{R}\n", logs_dir.display());

    let _ = io::stdout().flush();
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    let workspace = find_workspace();
    let slug = args.branch.clone()
        .or_else(|| detect_slug(&workspace))
        .unwrap_or_else(|| {
            eprintln!("Cannot auto-detect branch. Pass the slug explicitly:");
            eprintln!("  dev-feature importdocai-extraction-selection");
            std::process::exit(1);
        });

    let paths    = Paths::resolve(&workspace, &slug);
    let logs_dir = &args.logs_dir;
    fs::create_dir_all(logs_dir).expect("cannot create logs_dir");

    // Shared display state — written by health thread, read by render loop.
    let state: State = Arc::new(Mutex::new(Vec::new()));
    // Processes — owned exclusively by the main thread.
    let mut procs: Vec<Proc> = Vec::new();
    // Shutdown flag set by Ctrl+C.
    let stopping = Arc::new(AtomicBool::new(false));

    // ── Ctrl+C: SIGTERM every process group ──────────────────────────────────
    {
        let state    = Arc::clone(&state);
        let stopping = Arc::clone(&stopping);
        ctrlc::set_handler(move || {
            stopping.store(true, Ordering::Relaxed);
            print!("\x1b[H\x1b[2J");
            println!("\n  {YLW}Shutting down…{R}");
            // We cannot access `procs` here (main thread owns it), so we rely on
            // the main loop noticing `stopping` and killing everything before exit.
            let _ = state; // keep alive
        }).expect("failed to set Ctrl+C handler");
    }

    // ── Docker deps (blocking, before long-running services) ─────────────────
    if !args.no_copilot && paths.copilot.is_dir() {
        let dc = paths.copilot.join("docker-compose.dev.yml");
        if dc.exists() {
            println!("  Starting Copilot docker deps…");
            run_blocking("docker", &["compose", "-f", dc.to_str().unwrap(), "up", "-d"], &paths.copilot);
        }
    }
    if !args.no_opencti && paths.opencti.is_dir() {
        let dc = paths.opencti.join("opencti-platform/opencti-dev/docker-compose.yml");
        if dc.exists() {
            println!("  Starting OpenCTI docker deps…");
            run_blocking("docker", &["compose", "-f", dc.to_str().unwrap(), "up", "-d"], &paths.opencti);
        }
    }

    // ── Spawn long-running services ───────────────────────────────────────────
    {
        let mut svcs = state.lock().unwrap();

        macro_rules! try_spawn {
            ($svc:expr, $prog:expr, $argv:expr, $dir:expr, $env:expr) => {{
                let idx = svcs.len();
                match spawn($prog, $argv, $dir, $env, &$svc.log_path) {
                    Ok((child, pgid)) => {
                        $svc.pid        = Some(child.id());
                        $svc.started_at = Some(Instant::now());
                        $svc.health     = if $svc.url.is_some() {
                            Health::Launching
                        } else {
                            Health::Running
                        };
                        svcs.push($svc);
                        procs.push(Proc { idx, pgid, child });
                    }
                    Err(e) => {
                        $svc.health = Health::Degraded(e.to_string());
                        svcs.push($svc);
                    }
                }
            }};
        }

        // ── Filigran Copilot ──────────────────────────────────────────────────
        if !args.no_copilot && paths.copilot.is_dir() {
            let backend_dir = paths.copilot.join("backend");
            let python      = backend_dir.join(".venv/bin/python");

            // Backend
            let mut svc = Svc::new("copilot-backend", Some("http://localhost:8100"), "/api/health", 120, logs_dir.join("copilot-backend.log"));
            if python.exists() {
                try_spawn!(svc,
                    python.to_str().unwrap(),
                    &["-m", "uvicorn", "app.main:application",
                      "--reload", "--host", "0.0.0.0", "--port", "8100",
                      "--timeout-graceful-shutdown", "3"],
                    &backend_dir, &HashMap::new()
                );
            } else {
                svc.health = Health::Degraded("venv missing — run ./dev.sh once to set up".into());
                svcs.push(svc);
            }

            // Worker (no health URL — queue consumer)
            let mut svc = Svc::new("copilot-worker", None, "", 10, logs_dir.join("copilot-worker.log"));
            if python.exists() {
                try_spawn!(svc, python.to_str().unwrap(), &["-m", "saq", "app.worker.settings"], &backend_dir, &HashMap::new());
            } else {
                svc.health = Health::Degraded("venv missing".into());
                svcs.push(svc);
            }

            // Frontend
            let fe_dir = paths.copilot.join("frontend");
            ensure_copilot_fe_deps(&fe_dir);
            let mut svc = Svc::new("copilot-frontend", Some("http://localhost:3100"), "", 90, logs_dir.join("copilot-frontend.log"));
            if fe_dir.is_dir() {
                try_spawn!(svc, "yarn", &["dev"], &fe_dir, &HashMap::new());
            }
        }

        // ── OpenCTI graphql ───────────────────────────────────────────────────
        if !args.no_opencti && paths.opencti.is_dir() {
            let gql_dir = paths.opencti.join("opencti-platform/opencti-graphql");
            let mut svc = Svc::new(
                "opencti-graphql",
                Some("http://localhost:4000"),
                "/health",
                300,  // yarn start compiles TypeScript — can take several minutes
                logs_dir.join("opencti-graphql.log"),
            );
            if gql_dir.is_dir() {
                try_spawn!(svc, "yarn", &["start"], &gql_dir, &HashMap::new());
            }

            // OpenCTI frontend
            if !args.no_opencti_front {
                let front_dir = paths.opencti.join("opencti-platform/opencti-front");
                ensure_opencti_fe_deps(&front_dir);
                let mut svc = Svc::new("opencti-frontend", Some("http://localhost:3000"), "", 120, logs_dir.join("opencti-frontend.log"));
                if front_dir.is_dir() {
                    try_spawn!(svc, "yarn", &["dev"], &front_dir, &HashMap::new());
                }
            }
        }

        // ── Connector ─────────────────────────────────────────────────────────
        if !args.no_connector && paths.connector.is_dir() {
            let env_path = ensure_connector_env(&paths.connector);
            let venv     = ensure_connector_venv(&paths.connector);
            let python   = venv.join("bin/python");
            let src_dir  = paths.connector.join("src");
            let env      = parse_env_file(&env_path);

            let mut svc = Svc::new("connector", None, "", 30, logs_dir.join("connector.log"));
            if src_dir.is_dir() && python.exists() {
                try_spawn!(svc, python.to_str().unwrap(), &["main.py"], &src_dir, &env);
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

                let mut svcs = state.lock().unwrap();
                for svc in svcs.iter_mut() {
                    if svc.health.is_done() { continue; }

                    let Some(url) = svc.health_url() else { continue; };

                    let timed_out = svc.started_at
                        .map(|t| t.elapsed() > svc.startup_timeout)
                        .unwrap_or(false);

                    if probe(&url) {
                        svc.health = Health::Up;
                    } else if timed_out {
                        svc.health = Health::Degraded(
                            format!("no response after {}s", svc.startup_timeout.as_secs())
                        );
                    } else {
                        svc.health = match &svc.health {
                            Health::Probing(n) => Health::Probing(n + 1),
                            _                  => Health::Probing(1),
                        };
                    }
                }
            }
        });
    }

    // ── Main loop: render + crash detection ───────────────────────────────────
    print!("\x1b[2J"); // clear once before entering the loop
    loop {
        // Poll for Ctrl+C
        if stopping.load(Ordering::Relaxed) {
            for p in &mut procs { p.kill(); }
            thread::sleep(Duration::from_millis(300));
            break;
        }

        // Check for crashed processes and update display state
        {
            let mut svcs = state.lock().unwrap();
            for p in &mut procs {
                if let Some(code) = p.try_reap() {
                    svcs[p.idx].health = Health::Crashed(code);
                }
            }
            render(&svcs, &slug, logs_dir);
        }

        thread::sleep(Duration::from_millis(500));
    }
}
