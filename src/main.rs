//! dev-launcher — multi-product stack launcher with process-tree management, health monitoring,
//! and an interactive TUI for diving into per-service logs.

pub mod args;
pub mod config;
pub mod control;
pub mod diagnosis;
pub mod launcher_log;
pub mod services;
pub mod tui;
pub mod workspace;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::disable_raw_mode,
};
use serde::Serialize;

// ── Public re-exports from submodules ─────────────────────────────────────────

use args::{
    Args, Command as CliCommand, WorkspaceAction as CliWorkspaceAction,
    WorkspaceCommand as CliWorkspaceCommand,
};
use config::{load_config, read_line_or_interrupt, resolve_workspace_root};
use control::{
    clear_runtime_files, publish_response, publish_snapshot, queue_request, read_snapshot,
    take_request, wait_for_response, ControlAction, ControlResponse, ServiceRuntimeSnapshot,
    WorkspaceRuntimeSnapshot,
};
use diagnosis::{create_github_issue, diagnose_service, needs_recipe, DiagEvent, IssueContext};
use services::{
    alive_pid_count, compress_rotated_logs, detached_marker_path, docker_available,
    docker_compose_down, docker_compose_up, docker_down_workspace, docker_running_for_workspace,
    ensure_opencti_env, ensure_opencti_graphql_python_deps, kill_orphaned_pids, load_repo_manifest,
    mark_detached, opensearch_ready, patch_manifest_ports, pid_file_path, probe,
    read_compose_postgres_password, read_worker_pid, record_pid, remove_worker_pid,
    resolve_docker_project, rotate_log, run_blocking, run_manifest_bootstrap,
    shutdown_detached_session, sighup_handler, spawn_svc, split_health_url_parts,
    wait_for_opensearch, wipe_opencti_es_indices_if_stale, workspace_run_status,
    write_compose_override, write_worker_pid, ws_docker_project, DockerProject, Health, Paths,
    Proc, SpawnCmd, State, WorkspaceRunStatus, SIGHUP_STOP,
};
use tui::{
    build_credentials_lines, build_diagnose_lines, build_log_view_lines, build_overview_lines,
    drain_input_events, draw_ansi_lines, ensure_cooked_output, gather_credentials, render_shutdown,
    spawn_input_thread, tail_file, CredEntry, InputEvent, InputPauseGuard, Mode, TermStatus,
    TuiGuard, BOLD, BUILD_VERSION, CYN, DIM, GRN, R, RED, YLW,
};
use workspace::{
    apply_port_offset_to_env, base_ports_for, branch_to_slug, choices_to_workspace,
    compute_workspace_hash, current_branch, current_commit_short, default_product_choices,
    deploy_workspace_env, discover_flags_in_dir, ensure_worktree, extract_url_port,
    find_free_port_offset, find_pem_candidates, gen_api_token, init_workspace_env,
    inject_selected_pems, list_workspaces, load_workspace, parse_env_file, patch_url_default,
    pem_search_dirs, port_in_use, preflight_port_checks, read_active_flags, read_env_url_port,
    run_env_wizard, run_flag_selector, run_pem_selector, run_platform_mode_selector,
    run_product_selector, run_workspace_delete, run_workspace_selector, save_workspace, today,
    workspace_to_choices, workspaces_dir, write_active_flags, write_env_file, ws_env_path,
    FlagChoice, LaunchMode, PortCheck, ProductChoice, WorkspaceAction as SelectorWorkspaceAction,
    WorkspaceConfig, WorkspaceEntry, COMMIT_PREFIX, CONNECTOR_ENV_VARS, COPILOT_ENV_VARS,
    OPENCTI_ENV_VARS,
};

// ── Per-product startup helpers ───────────────────────────────────────────────

fn ensure_connector_env(dir: &Path) -> PathBuf {
    let path = dir.join(".env.dev");
    if !path.exists() {
        let _ = fs::write(
            &path,
            "\
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
",
        );
        println!("  {YLW}Created connector env template — edit before starting:{R}");
        println!("  {DIM}{}{R}\n", path.display());
    } else {
        let mut env = parse_env_file(&path);
        if !env.contains_key("CONNECTOR_TYPE") {
            env.insert(
                "CONNECTOR_TYPE".to_string(),
                "INTERNAL_IMPORT_FILE".to_string(),
            );
            write_env_file(&path, &env);
            println!(
                "  {GRN}✓{R}  Added missing CONNECTOR_TYPE=INTERNAL_IMPORT_FILE to connector env"
            );
        }
    }
    path
}

fn ensure_connector_venv(dir: &Path) -> PathBuf {
    let venv = dir.join(".venv");
    if !venv.join("bin/python").exists() {
        println!("  Creating connector Python venv…");
        run_blocking("python3", &["-m", "venv", ".venv"], dir);
        let pip = venv.join("bin/pip").to_string_lossy().into_owned();
        let reqs = dir
            .join("src/requirements.txt")
            .to_string_lossy()
            .into_owned();
        run_blocking(&pip, &["install", "-q", "-r", &reqs], dir);
    }
    venv
}

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
            println!("  {DIM}Fix: npm install -g corepack   then re-run dev-launcher{R}");
            println!("  {DIM}     (or: sudo corepack enable if node is installed system-wide){R}");
            println!();
        }
    }
}

fn validate_connector_env(env: &HashMap<String, String>) -> Option<String> {
    let token = env.get("OPENCTI_TOKEN").map(|s| s.as_str()).unwrap_or("");
    if token.is_empty() || token == "ChangeMe" {
        return Some("OPENCTI_TOKEN not set — edit .env.dev before running".into());
    }
    None
}

fn copilot_backend_env(copilot_dir: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let repo_env = parse_env_file(&copilot_dir.join(".env"));
    for key in ["DATABASE_URL", "REDIS_URL", "S3_ENDPOINT"] {
        if let Some(value) = repo_env.get(key).filter(|v| !v.trim().is_empty()) {
            env.insert(key.to_string(), value.clone());
        }
    }

    if !env.contains_key("DATABASE_URL") {
        let compose = copilot_dir.join("docker-compose.dev.yml");
        if let Some(password) = read_compose_postgres_password(&compose) {
            env.insert(
                "DATABASE_URL".into(),
                format!("postgresql+asyncpg://copilot:{password}@localhost:5432/copilot"),
            );
        }
    }
    env
}

fn ensure_openaev_workspace_env(
    ws_env_path: &Path,
    repo_env_path: &Path,
    backend_port: u16,
) -> HashMap<String, String> {
    let mut env = parse_env_file(ws_env_path);
    let mut changed = false;

    let mut set_default = |key: &str, value: String| {
        if env.get(key).is_none_or(|v| v.trim().is_empty()) {
            env.insert(key.to_string(), value);
            changed = true;
        }
    };

    set_default("OPENAEV_URL", format!("http://localhost:{backend_port}"));
    set_default("OPENAEV_ADMIN_EMAIL", "admin@openaev.io".to_string());
    set_default("OPENAEV_ADMIN_PASSWORD", "admin".to_string());
    set_default("OPENAEV_ADMIN_TOKEN", gen_api_token());
    set_default(
        "OPENAEV_ADMIN_ENCRYPTION_KEY",
        format!("{}{}", gen_api_token(), gen_api_token()),
    );
    set_default("OPENAEV_ADMIN_ENCRYPTION_SALT", gen_api_token());
    set_default("OPENAEV_HEALTHCHECK_KEY", "ChangeMe".to_string());

    if changed {
        write_env_file(ws_env_path, &env);
        deploy_workspace_env(ws_env_path, repo_env_path);
    }

    env
}

fn openaev_backend_env(
    ws_env_dir: &Path,
    openaev_dir: &Path,
    port_offset: u16,
    backend_port: u16,
) -> HashMap<String, String> {
    let ws_env = ensure_openaev_workspace_env(
        &ws_env_path(ws_env_dir, "openaev"),
        &openaev_dir.join("openaev-dev/.env"),
        backend_port,
    );

    let pg_user = ws_env
        .get("POSTGRES_USER")
        .cloned()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "openaev".to_string());
    let pg_password = ws_env
        .get("POSTGRES_PASSWORD")
        .cloned()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "openaev".to_string());
    let openaev_url = ws_env
        .get("OPENAEV_URL")
        .cloned()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("http://localhost:{backend_port}"));
    let healthcheck_key = ws_env
        .get("OPENAEV_HEALTHCHECK_KEY")
        .cloned()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "ChangeMe".to_string());

    let mut env = ws_env;
    env.insert("SERVER_PORT".to_string(), backend_port.to_string());
    env.insert("OPENAEV_BASE_URL".to_string(), openaev_url);
    env.insert(
        "SPRING_DATASOURCE_URL".to_string(),
        format!(
            "jdbc:postgresql://localhost:{}/openaev",
            5432u16.saturating_add(port_offset)
        ),
    );
    env.insert("SPRING_DATASOURCE_USERNAME".to_string(), pg_user);
    env.insert("SPRING_DATASOURCE_PASSWORD".to_string(), pg_password);
    env.insert(
        "ENGINE_URL".to_string(),
        format!("http://localhost:{}", 9200u16.saturating_add(port_offset)),
    );
    env.insert("MINIO_ENDPOINT".to_string(), "localhost".to_string());
    env.insert(
        "MINIO_PORT".to_string(),
        10000u16.saturating_add(port_offset).to_string(),
    );
    env.insert("MINIO_BUCKET".to_string(), "openaev".to_string());
    env.insert("MINIO_ACCESS_KEY".to_string(), "minioadmin".to_string());
    env.insert("MINIO_ACCESS_SECRET".to_string(), "minioadmin".to_string());
    env.insert(
        "OPENAEV_RABBITMQ_HOSTNAME".to_string(),
        "localhost".to_string(),
    );
    env.insert(
        "OPENAEV_RABBITMQ_PORT".to_string(),
        5672u16.saturating_add(port_offset).to_string(),
    );
    env.insert("OPENAEV_RABBITMQ_USER".to_string(), "guest".to_string());
    env.insert("OPENAEV_RABBITMQ_PASS".to_string(), "guest".to_string());
    env.insert("OPENAEV_RABBITMQ_VHOST".to_string(), "/".to_string());
    env.insert(
        "OPENAEV_RABBITMQ_MANAGEMENT_PORT".to_string(),
        15672u16.saturating_add(port_offset).to_string(),
    );
    env.insert("OPENAEV_HEALTHCHECK_KEY".to_string(), healthcheck_key);
    env
}

/// Dev-only packages injected into the Copilot backend venv after the main
/// requirements are installed.  Not in requirements.txt so they don't affect
/// production images, but required locally to match what the CI test suite runs.
const COPILOT_DEV_DEPS: &[&str] = &[
    "pytest",
    "pytest-cov",
    "pytest-asyncio",
    "mypy",
    "black",
    "pyright",
];

fn ensure_copilot_backend_venv(backend_dir: &Path) {
    let venv = backend_dir.join(".venv");
    if venv.join("bin/python").exists() {
        return;
    }
    println!("  Creating Copilot backend Python venv…");
    let _ = io::stdout().flush();
    run_blocking("python3", &["-m", "venv", ".venv"], backend_dir);
    let pip = venv.join("bin/pip").to_string_lossy().into_owned();
    if backend_dir.join("requirements.txt").exists() {
        println!("  Installing Python dependencies…");
        let _ = io::stdout().flush();
        let reqs = backend_dir
            .join("requirements.txt")
            .to_string_lossy()
            .into_owned();
        run_blocking(&pip, &["install", "-r", &reqs], backend_dir);
    } else if backend_dir.join("pyproject.toml").exists() {
        println!("  Installing Python dependencies…");
        let _ = io::stdout().flush();
        run_blocking(&pip, &["install", "-e", "."], backend_dir);
    }
}

/// Install dev-only tools into the Copilot backend venv when they are absent.
/// Uses `venv/bin/pytest` as a sentinel — if it exists all deps are assumed present.
/// Safe to call on every launch; runs only once after the venv is first created.
fn ensure_copilot_backend_dev_deps(backend_dir: &Path) {
    let venv = backend_dir.join(".venv");
    if !venv.join("bin/python").exists() {
        return;
    }
    if venv.join("bin/pytest").exists() {
        return;
    }
    println!("  Installing dev tools (pytest · mypy · black · pyright)…");
    let _ = io::stdout().flush();
    let pip = venv.join("bin/pip").to_string_lossy().into_owned();
    let mut args = vec!["install"];
    args.extend_from_slice(COPILOT_DEV_DEPS);
    run_blocking(&pip, &args, backend_dir);
    println!("  {GRN}✓{R}  Dev tools ready");
}

/// Ensure infinity-emb is installed and healthy in its own isolated venv.
/// Validates the install by running `infinity_emb --help` — not just checking
/// that the binary exists. A broken install (missing typer, bad optimum, etc.)
/// is detected here and repaired before the service ever starts.
fn ensure_infinity_emb_isolated(infinity_dir: &Path) -> bool {
    let bin = infinity_dir.join(".venv/bin/infinity_emb");
    let pip = infinity_dir.join(".venv/bin/pip");

    let is_healthy = || -> bool {
        // Binary must exist and respond to --help (catches typer/CLI issues).
        // Also require einops explicitly — it's needed by nomic-embed-text-v1.5
        // but is not pulled in transitively by infinity-emb's own extras.
        bin.exists()
            && Command::new(&bin)
                .arg("--help")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            && pip.exists()
            && Command::new(&pip)
                .args(["show", "einops"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
    };

    if is_healthy() {
        return true;
    }

    if !pip.exists() {
        println!("  Creating isolated infinity-emb venv…");
        let _ = fs::create_dir_all(infinity_dir);
        run_blocking("python3", &["-m", "venv", ".venv"], infinity_dir);
        if !pip.exists() {
            return false;
        }
        println!(
            "  Installing infinity-emb[server,torch,transformers] (first run — may take a minute)…"
        );
    } else {
        println!("  infinity-emb install is unhealthy — repairing…");
    }

    // [server] includes fastapi + uvicorn + typer (required by the v2 CLI entrypoint).
    // click<8.2 keeps typer 0.12 working (click>=8.2 broke the CLI bootstrap).
    // einops is required by nomic-embed-text-v1.5 (dynamic module import).
    run_blocking(
        pip.to_str().unwrap_or("pip"),
        &[
            "install",
            "infinity-emb[server,torch,transformers]",
            "click<8.2",
            "einops",
        ],
        infinity_dir,
    );

    // optimum/optimum-onnx creates an `optimum/` namespace that tricks
    // CHECK_OPTIMUM.is_available into returning True, but bettertransformer
    // was removed from optimum >= 1.21 — causing NameError at startup.
    // Uninstall proactively; they are not needed for torch-based inference.
    let pip_str = pip.to_str().unwrap_or("pip");
    run_blocking(
        pip_str,
        &["uninstall", "-y", "optimum", "optimum-onnx"],
        infinity_dir,
    );

    is_healthy()
}

/// Ensure the autoresearch setup is ready:
/// 1. uv is installed (required to run the autoresearch repo)
/// 2. The autoresearch repo is cloned into ar_dir/repo/
/// 3. uv sync has been run (repo .venv exists)
/// 4. A separate service venv exists with fastapi + uvicorn for runner.py
///
/// torch and all ML deps are managed by uv inside the repo — we do NOT pip-install
/// them separately.  The repo URL is chosen by the caller based on platform.
fn ensure_autoresearch_isolated(ar_dir: &Path, repo_url: &str) -> bool {
    let _ = fs::create_dir_all(ar_dir);

    // ── 1. Check uv ──────────────────────────────────────────────────────────
    let uv_bin = resolve_uv_bin();
    let uv_ok = Command::new(&uv_bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !uv_ok {
        println!("  Installing uv package manager…");
        let installed = Command::new("sh")
            .args(["-c", "curl -LsSf https://astral.sh/uv/install.sh | sh"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !installed {
            eprintln!("  autoresearch: failed to install uv — install it manually: https://docs.astral.sh/uv/");
            return false;
        }
    }

    // ── 2. Clone repo ────────────────────────────────────────────────────────
    let repo_dir = ar_dir.join("repo");
    if !repo_dir.is_dir() {
        println!("  Cloning autoresearch repo ({repo_url})…");
        run_blocking("git", &["clone", repo_url, "repo"], ar_dir);
        if !repo_dir.is_dir() {
            eprintln!("  autoresearch: git clone failed");
            return false;
        }
    }

    // ── 3. uv sync (installs torch + all ML deps into repo/.venv) ────────────
    if !repo_dir.join(".venv").is_dir() {
        println!("  Running uv sync in autoresearch repo (first run — downloads torch, may take a few minutes)…");
        run_blocking(&uv_bin, &["sync"], &repo_dir);
        if !repo_dir.join(".venv").is_dir() {
            eprintln!("  autoresearch: uv sync failed");
            return false;
        }
    }

    // ── 4. Runner service venv (fastapi + uvicorn only — lightweight) ─────────
    let svc_pip = ar_dir.join(".venv/bin/pip");

    if !svc_pip.exists() {
        println!("  Creating runner service venv…");
        run_blocking("python3", &["-m", "venv", ".venv"], ar_dir);
        if !svc_pip.exists() {
            eprintln!("  autoresearch: python3 -m venv failed");
            return false;
        }
    }

    // Install (or repair) service deps regardless of whether the venv is new.
    // A previous broken run may have left the venv without uvicorn.
    let svc_pip_str = svc_pip.to_str().unwrap_or("pip");
    let uvicorn_ok = Command::new(svc_pip_str)
        .args(["show", "uvicorn"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !uvicorn_ok {
        println!("  Installing runner service deps (fastapi, uvicorn, sse-starlette)…");
        run_blocking(
            svc_pip_str,
            &[
                "install",
                "fastapi",
                "uvicorn[standard]",
                "sse-starlette",
                "python-multipart",
            ],
            ar_dir,
        );
    }

    // Final health check
    Command::new(svc_pip_str)
        .args(["show", "uvicorn"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Resolve the uv binary path.
/// Checks known install locations before falling back to bare "uv" (PATH lookup).
/// Covers: official uv installer (~/.local/bin), cargo install (~/.cargo/bin),
/// Homebrew on Apple Silicon (/opt/homebrew/bin), Homebrew on Intel (/usr/local/bin).
fn resolve_uv_bin() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{home}/.local/bin/uv"),
        format!("{home}/.cargo/bin/uv"),
        "/opt/homebrew/bin/uv".to_string(),
        "/usr/local/bin/uv".to_string(),
        "uv".to_string(),
    ];
    for candidate in &candidates {
        if Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return candidate.clone();
        }
    }
    "uv".to_string()
}

/// Returns `true` when frontend deps are ready to use, `false` only if every
/// recovery attempt failed and the service truly cannot start.
///
/// `yarn install` is always run — it is idempotent and completes in ~1-2 s when
/// nothing needs to change. Checking marker files (`.yarn/install-state.gz`,
/// `node_modules/.yarn-integrity`) is intentionally avoided: a stale install-state
/// from a previous branch or an incomplete install leaves yarn's internal binary
/// resolution out of sync with the actual `node_modules` tree, causing scripts to
/// fail with "command not found" even though the binary exists on disk. Only yarn
/// itself can reliably judge its own install state.
///
/// Recovery strategy (mirrors `ensure_infinity_emb_isolated`):
///   1. `yarn install`  — covers fresh worktrees, lockfile drift, and link-step skew
///   2. If that fails and `node_modules/` exists (truly corrupted state):
///      wipe `node_modules/` + `.yarn/install-state.gz` and retry once.
fn ensure_fe_deps(dir: &Path, label: &str) -> bool {
    println!("  Syncing {label} frontend deps…");
    if run_blocking("yarn", &["install"], dir) == 0 {
        return true;
    }

    // Install failed. If node_modules is present it may be in a state that
    // prevents yarn from making progress. Wipe and retry from a clean slate.
    if dir.join("node_modules").is_dir() {
        println!("  {YLW}yarn install failed — wiping node_modules and retrying…{R}");
        let _ = fs::remove_dir_all(dir.join("node_modules"));
        let _ = fs::remove_file(dir.join(".yarn").join("install-state.gz"));
    }

    if run_blocking("yarn", &["install"], dir) == 0 {
        return true;
    }

    println!("  {YLW}{label} frontend deps could not be installed after clean retry.{R}");
    println!("  {DIM}Fix: cd {} && yarn install{R}", dir.display());
    false
}

fn ensure_copilot_fe_deps(dir: &Path) -> bool {
    ensure_fe_deps(dir, "Copilot")
}
fn ensure_opencti_fe_deps(dir: &Path) -> bool {
    ensure_fe_deps(dir, "OpenCTI")
}
fn ensure_openaev_fe_deps(dir: &Path) -> bool {
    ensure_fe_deps(dir, "OpenAEV")
}

fn maven_cmd(openaev_root: &Path) -> String {
    let wrapper = openaev_root.join("mvnw");
    if wrapper.exists() {
        wrapper.to_string_lossy().into_owned()
    } else {
        "mvn".to_string()
    }
}

fn parse_java_major(version_output: &str) -> Option<u16> {
    let raw = version_output
        .split_whitespace()
        .find(|part| part.starts_with('"'))
        .map(|part| part.trim_matches('"'))?;
    let mut parts = raw.split(['.', '-']);
    let first = parts.next()?;
    if first == "1" {
        parts.next()?.parse().ok()
    } else {
        first.parse().ok()
    }
}

fn current_java_major() -> Option<u16> {
    let output = Command::new("java").arg("-version").output().ok()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_java_major(if stderr.trim().is_empty() {
        stdout.as_ref()
    } else {
        stderr.as_ref()
    })
}

fn java_home_for_major(major: u16) -> Option<PathBuf> {
    let java_home = Command::new("/usr/libexec/java_home")
        .args(["-v", &major.to_string()])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    if let Some(home) = java_home {
        if home.join("bin/java").exists() {
            return Some(home);
        }
    }

    let candidates = [
        format!("/opt/homebrew/opt/openjdk@{major}/libexec/openjdk.jdk/Contents/Home"),
        format!("/usr/local/opt/openjdk@{major}/libexec/openjdk.jdk/Contents/Home"),
        format!("/Library/Java/JavaVirtualMachines/openjdk-{major}.jdk/Contents/Home"),
        format!("/Library/Java/JavaVirtualMachines/temurin-{major}.jdk/Contents/Home"),
    ];

    candidates
        .iter()
        .map(PathBuf::from)
        .find(|home| home.join("bin/java").exists())
}

fn openaev_java_env() -> Result<HashMap<String, String>, String> {
    const REQUIRED_JAVA_MAJOR: u16 = 21;

    if current_java_major() == Some(REQUIRED_JAVA_MAJOR) {
        return Ok(HashMap::new());
    }

    if let Some(home) = java_home_for_major(REQUIRED_JAVA_MAJOR) {
        let mut env = HashMap::new();
        env.insert("JAVA_HOME".to_string(), home.to_string_lossy().into_owned());
        if let Ok(path) = std::env::var("PATH") {
            env.insert(
                "PATH".to_string(),
                format!("{}:{path}", home.join("bin").display()),
            );
        }
        return Ok(env);
    }

    let detected = current_java_major()
        .map(|v| format!("Java {v} detected"))
        .unwrap_or_else(|| "no Java runtime detected".to_string());
    Err(format!(
        "OpenAEV requires JDK 21 for backend compilation — {detected}. Install with 'brew install openjdk@21'."
    ))
}

#[cfg(test)]
mod java_tests {
    use super::parse_java_major;

    #[test]
    fn parses_modern_java_versions() {
        assert_eq!(
            parse_java_major("openjdk version \"25.0.2\" 2026-01-20"),
            Some(25)
        );
        assert_eq!(
            parse_java_major("openjdk version \"21.0.7\" 2025-04-15 LTS"),
            Some(21)
        );
    }

    #[test]
    fn parses_legacy_java_versions() {
        assert_eq!(parse_java_major("java version \"1.8.0_442\""), Some(8));
    }
}

fn bootstrap_infra_dir(dir: &Path, repo: &str) {
    let is_new = !dir.is_dir();
    if is_new {
        println!("  Bootstrapping {repo} infra directory…");
    }
    match repo {
        "grafana" => {
            fs::create_dir_all(dir.join("provisioning/datasources"))
                .expect("cannot create grafana dir");
            if is_new {
                let _ = fs::write(
                    dir.join("docker-compose.dev.yml"),
                    include_str!("infra/grafana/docker-compose.dev.yml"),
                );
                let _ = fs::write(
                    dir.join("loki-config.yml"),
                    include_str!("infra/grafana/loki-config.yml"),
                );
                let _ = fs::write(
                    dir.join("promtail-config.yml"),
                    include_str!("infra/grafana/promtail-config.yml"),
                );
                let _ = fs::write(
                    dir.join("provisioning/datasources/loki.yml"),
                    include_str!("infra/grafana/provisioning/datasources/loki.yml"),
                );
            }
            let env_path = dir.join(".env");
            if !env_path.exists() {
                let _ = fs::write(
                    &env_path,
                    "\
# Grafana dev environment — edit to override defaults from docker-compose.dev.yml
# Uncomment and change any value you want to customise.

#GRAFANA_PORT=3200
#LOKI_PORT=3101

# By default Grafana runs with anonymous Admin access (no login required).
# To enable the login form, set the three variables below.
#GF_AUTH_ANONYMOUS_ENABLED=false
#GF_AUTH_DISABLE_LOGIN_FORM=false
#GF_SECURITY_ADMIN_USER=admin
#GF_SECURITY_ADMIN_PASSWORD=admin
",
                );
            }
        }
        "langfuse" => {
            fs::create_dir_all(dir).expect("cannot create langfuse dir");
            let dc_content = include_str!("infra/langfuse/docker-compose.dev.yml");
            let dc_path = dir.join("docker-compose.dev.yml");
            let needs_write =
                !dc_path.exists() || fs::read_to_string(&dc_path).unwrap_or_default() != dc_content;
            if needs_write {
                let _ = fs::write(&dc_path, dc_content);
            }
            let env_path = dir.join(".env");
            if !env_path.exists() {
                let _ = fs::write(
                    &env_path,
                    "\
# Langfuse dev environment — edit to override defaults from docker-compose.dev.yml
# Uncomment and change any value you want to customise.

#LANGFUSE_PORT=3201
#LANGFUSE_DB_PORT=5433
#LANGFUSE_DB_PASSWORD=langfuse_dev

#LANGFUSE_ADMIN_EMAIL=admin@example.com
#LANGFUSE_ADMIN_PASSWORD=changeme
#LANGFUSE_ADMIN_NAME=Admin

#LANGFUSE_PROJECT_NAME=filigran-dev
#LANGFUSE_PUBLIC_KEY=lf_pk_dev_changeme_publickey
#LANGFUSE_SECRET_KEY=lf_sk_dev_changeme_secretkey

# Secrets — change these if you expose this instance beyond localhost.
#LANGFUSE_NEXTAUTH_SECRET=langfuse_dev_nextauth_secret_changeme
#LANGFUSE_SALT=langfuse_dev_salt_changeme
",
                );
            }
        }
        _ => {}
    }
}

fn clean_docker_for_workspace(
    slug: &str,
    paths: &Paths,
    no_copilot: bool,
    no_opencti: bool,
    no_openaev: bool,
    no_grafana: bool,
    no_langfuse: bool,
) {
    use services::{resolve_product_docker_for_down, run_blocking_logged};

    let sep = "─".repeat(72);
    println!("  {DIM}{sep}{R}");
    println!("  {BOLD}Clean start  {DIM}—  wiping Docker containers + volumes for {slug}{R}");
    println!("  {DIM}{sep}{R}\n");

    let products: &[(&str, &Path, bool)] = &[
        ("filigran-copilot", paths.copilot.as_path(), no_copilot),
        ("opencti", paths.opencti.as_path(), no_opencti),
        ("openaev", paths.openaev.as_path(), no_openaev),
        ("grafana", paths.grafana.as_path(), no_grafana),
        ("langfuse", paths.langfuse.as_path(), no_langfuse),
    ];

    for &(repo, dir, skip) in products {
        println!("  {BOLD}{repo}{R}");

        if skip {
            println!("    {DIM}skipped (product disabled){R}");
            continue;
        }
        if !dir.is_dir() {
            println!(
                "    {DIM}skipped (directory not found: {}){R}",
                dir.display()
            );
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

        let before = Command::new("docker")
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("label=com.docker.compose.project={ws_proj}"),
                "--format",
                "{{.Names}}  {{.Status}}",
            ])
            .stdin(Stdio::null())
            .output();
        let before_str = before
            .ok()
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

        println!("    {DIM}─ (a) workspace-scoped down -v{R}");
        let ws_override = write_compose_override(&compose_file, slug, 0);
        let mut argv: Vec<&str> = vec!["compose", "-p", &ws_proj, "-f", file_str];
        let ov_str: String;
        if let Some(ref ov) = ws_override {
            ov_str = ov.to_string_lossy().into_owned();
            println!("    {DIM}    override file: {ov_str}{R}");
            argv.extend_from_slice(&["-f", &ov_str]);
        }
        argv.extend_from_slice(&["down", "-v"]);
        run_blocking_logged("docker", &argv, dir);

        println!("    {DIM}─ (b) base-project down -v{R}");
        run_blocking_logged(
            "docker",
            &["compose", "-p", &base_proj, "-f", file_str, "down", "-v"],
            dir,
        );

        println!("    {GRN}done{R}");
        println!();
    }
    println!();
}

// ── Workspace resolution helpers ──────────────────────────────────────────────

fn derive_branch_from_path(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return None;
    }
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

fn build_entries_from_branches(args: &Args) -> Vec<WorkspaceEntry> {
    use workspace::PRODUCTS;

    PRODUCTS
        .iter()
        .map(|(repo, _, key, _)| {
            let branch: Option<String> = match *key {
                "copilot" => args
                    .copilot_branch
                    .clone()
                    .or_else(|| {
                        args.copilot_commit
                            .as_ref()
                            .map(|c| format!("{COMMIT_PREFIX}{c}"))
                    })
                    .or_else(|| {
                        args.copilot_worktree
                            .as_ref()
                            .and_then(|p| derive_branch_from_path(p))
                    }),
                "opencti" => args
                    .opencti_branch
                    .clone()
                    .or_else(|| {
                        args.opencti_commit
                            .as_ref()
                            .map(|c| format!("{COMMIT_PREFIX}{c}"))
                    })
                    .or_else(|| {
                        args.opencti_worktree
                            .as_ref()
                            .and_then(|p| derive_branch_from_path(p))
                    }),
                "openaev" => args
                    .openaev_branch
                    .clone()
                    .or_else(|| {
                        args.openaev_commit
                            .as_ref()
                            .map(|c| format!("{COMMIT_PREFIX}{c}"))
                    })
                    .or_else(|| {
                        args.openaev_worktree
                            .as_ref()
                            .and_then(|p| derive_branch_from_path(p))
                    }),
                "connector" => args
                    .connector_branch
                    .clone()
                    .or_else(|| {
                        args.connector_commit
                            .as_ref()
                            .map(|c| format!("{COMMIT_PREFIX}{c}"))
                    })
                    .or_else(|| {
                        args.connector_worktree
                            .as_ref()
                            .and_then(|p| derive_branch_from_path(p))
                    }),
                _ => None,
            };
            WorkspaceEntry {
                repo: repo.to_string(),
                enabled: branch.is_some(),
                branch: branch.unwrap_or_default(),
            }
        })
        .collect()
}

// ── Subprocess session management ────────────────────────────────────────────

struct StoppedSession {
    hash: String,
    pid: u32,
    /// True when the worker was inherited from a previous selector run and is
    /// therefore not a child of this process — `waitpid` will not work on it.
    /// Reattach uses a polling-based wait instead.
    adopted: bool,
}

fn spawn_session_worker(exe: &Path, hash: &str, clean: bool) -> u32 {
    let mut cmd = Command::new(exe);
    cmd.arg("--session-worker").arg("--workspace").arg(hash);
    if clean {
        cmd.arg("--clean-start");
    }
    let child = cmd.spawn().expect("failed to spawn session worker");
    let pid = child.id();
    std::mem::forget(child);
    pid
}

fn wait_for_session(pid: u32, hash: &str, stopped: &mut Vec<StoppedSession>) -> bool {
    loop {
        let mut status: libc::c_int = 0;
        let ret = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WUNTRACED) };
        if ret <= 0 {
            return false;
        }
        if libc::WIFEXITED(status) {
            return libc::WEXITSTATUS(status) == 0;
        }
        if libc::WIFSIGNALED(status) {
            return false;
        }
        if libc::WIFSTOPPED(status) {
            stopped.push(StoppedSession {
                hash: hash.to_string(),
                pid,
                adopted: false,
            });
            return true;
        }
    }
}

/// Poll-based replacement for `wait_for_session` for adopted workers
/// (we can't `waitpid` on a non-child). Returns true when the worker re-detaches
/// itself; pushes it back into `stopped` so the selector can offer reattach again.
fn wait_for_adopted_session(pid: u32, hash: &str, stopped: &mut Vec<StoppedSession>) -> bool {
    let marker = detached_marker_path(hash);
    let alive = || unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;

    // Phase 1 — wait for the worker to reattach (it removes the marker on resume).
    // If the worker dies or never wakes up, give up after a short window.
    let phase1_deadline = Instant::now() + Duration::from_secs(10);
    while marker.exists() && Instant::now() < phase1_deadline {
        if !alive() {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !alive() {
        return false;
    }

    // Phase 2 — wait for the worker to detach again (it rewrites the marker
    // before SIGSTOPping itself) or to exit cleanly.
    loop {
        if !alive() {
            return false;
        }
        if marker.exists() {
            stopped.push(StoppedSession {
                hash: hash.to_string(),
                pid,
                adopted: true,
            });
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn startup_orphan_check(ws_dir: &Path, _workspace_root: &Path, adopted: &mut Vec<StoppedSession>) {
    for ws in list_workspaces(ws_dir) {
        if !detached_marker_path(&ws.hash).exists() {
            continue;
        }

        if let Some(worker_pid) = read_worker_pid(&ws.hash) {
            let alive = unsafe { libc::kill(worker_pid as libc::pid_t, 0) } == 0;
            if alive {
                // Adopt the orphaned worker rather than killing it. The user
                // will be able to reattach via the workspace selector; the
                // worker is currently SIGSTOPped so we can SIGCONT it later.
                eprintln!(
                    "  [dev-launcher] Adopting detached session {} from a previous launcher.",
                    ws.hash
                );
                adopted.push(StoppedSession {
                    hash: ws.hash.clone(),
                    pid: worker_pid,
                    adopted: true,
                });
                continue;
            } else {
                remove_worker_pid(&ws.hash);
            }
        }

        match workspace_run_status(&ws.hash) {
            WorkspaceRunStatus::Running | WorkspaceRunStatus::Degraded => {
                let alive = alive_pid_count(&ws.hash);
                println!(
                    "\n  Detached session still running: {}  {GRN}● {alive} process(es) alive{R}",
                    ws.summary()
                );
                print!("  [s] stop   [Enter] ignore  ");
                let _ = io::stdout().flush();
                let mut answer = String::new();
                let _ = io::stdin().read_line(&mut answer);
                if answer.trim().eq_ignore_ascii_case("s") {
                    shutdown_detached_session(&ws.hash);
                }
                let _ = fs::remove_file(detached_marker_path(&ws.hash));
            }
            WorkspaceRunStatus::Failed | WorkspaceRunStatus::NotRunning => {
                if docker_running_for_workspace(&ws.hash) {
                    println!(
                        "\n  Detached session is dirty: {}  {YLW}● host processes stopped, Docker containers still running{R}",
                        ws.summary()
                    );
                    print!("  [c] clean up Docker   [Enter] ignore  ");
                    let _ = io::stdout().flush();
                    let mut answer = String::new();
                    let _ = io::stdin().read_line(&mut answer);
                    if answer.trim().eq_ignore_ascii_case("c") {
                        docker_down_workspace(&ws.hash);
                    }
                }
                let _ = fs::remove_file(detached_marker_path(&ws.hash));
                let _ = fs::remove_file(pid_file_path(&ws.hash));
            }
        }
    }
}

fn open_dir_in_vscode(dir: &Path) {
    let candidates = [
        "/usr/local/bin/code-insiders",
        "/opt/homebrew/bin/code-insiders",
        "/usr/local/bin/code",
        "/opt/homebrew/bin/code",
    ];
    let launched = candidates.iter().any(|bin| {
        std::path::Path::new(bin).exists()
            && std::process::Command::new(bin)
                .args(["--new-window", dir.to_str().unwrap_or(".")])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
    });
    if !launched {
        for app in &["Visual Studio Code - Insiders", "Visual Studio Code"] {
            if std::process::Command::new("open")
                .args([
                    "-n",
                    "-a",
                    app,
                    "--args",
                    "--new-window",
                    dir.to_str().unwrap_or("."),
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
            {
                break;
            }
        }
    }
}

fn workspace_code_dir(cfg: &WorkspaceConfig, workspace_root: &Path) -> Option<std::path::PathBuf> {
    use crate::workspace::git::branch_to_slug;
    use crate::workspace::is_infra_product;
    cfg.entries
        .iter()
        .filter(|e| e.enabled && !is_infra_product(e.repo.as_str()))
        .find_map(|e| {
            let path = if !e.branch.is_empty() {
                let slug = branch_to_slug(&e.branch);
                let wt = workspace_root.join(format!("{}-{}", e.repo, slug));
                if wt.is_dir() {
                    wt
                } else {
                    workspace_root.join(&e.repo)
                }
            } else {
                workspace_root.join(&e.repo)
            };
            path.is_dir().then_some(path)
        })
}

#[derive(Serialize)]
struct WorkspaceListRow {
    hash: String,
    created: String,
    /// Runtime port offset, only known when a session is running.
    port_offset: Option<u16>,
    summary: String,
    runtime_state: String,
    detached: bool,
    worker_pid: Option<u32>,
    snapshot_age_secs: Option<u64>,
}

#[derive(Serialize)]
struct WorkspaceStatusRow {
    hash: String,
    created: String,
    port_offset: Option<u16>,
    summary: String,
    runtime_state: String,
    detached: bool,
    worker_pid: Option<u32>,
    snapshot_age_secs: Option<u64>,
    services: Vec<ServiceRuntimeSnapshot>,
}

fn worker_pid_if_alive(slug: &str) -> Option<u32> {
    read_worker_pid(slug).filter(|pid| unsafe { libc::kill(*pid as libc::pid_t, 0) == 0 })
}

fn snapshot_age_secs(snapshot: &WorkspaceRuntimeSnapshot) -> u64 {
    control::now_ms().saturating_sub(snapshot.updated_at_ms) / 1000
}

fn workspace_runtime_state(slug: &str, worker_pid: Option<u32>) -> String {
    if worker_pid.is_some() {
        if detached_marker_path(slug).exists() {
            "detached".to_string()
        } else {
            "running".to_string()
        }
    } else {
        match workspace_run_status(slug) {
            WorkspaceRunStatus::NotRunning => "not_running".to_string(),
            WorkspaceRunStatus::Running => "running".to_string(),
            WorkspaceRunStatus::Degraded => "degraded".to_string(),
            WorkspaceRunStatus::Failed => "failed".to_string(),
        }
    }
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn print_json_pretty<T: Serialize>(value: &T) -> Result<(), String> {
    let out = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    println!("{out}");
    Ok(())
}

fn require_workspace(ws_dir: &Path, hash: &str) -> Result<WorkspaceConfig, String> {
    load_workspace(ws_dir, hash).ok_or_else(|| format!("workspace {hash} not found"))
}

fn request_runtime_action(
    hash: &str,
    action: ControlAction,
    timeout: Duration,
) -> Result<ControlResponse, String> {
    let req = queue_request(hash, action).map_err(|e| e.to_string())?;
    match wait_for_response(hash, &req.id, timeout).map_err(|e| e.to_string())? {
        Some(resp) if resp.ok => Ok(resp),
        Some(resp) => Err(resp.message),
        None => Err(format!(
            "timed out waiting for workspace {hash} to acknowledge the operation"
        )),
    }
}

fn wait_for_worker_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    (unsafe { libc::kill(pid as libc::pid_t, 0) }) != 0
}

fn run_workspace_command(cmd: &CliWorkspaceCommand, ws_dir: &Path) -> Result<(), String> {
    match &cmd.action {
        CliWorkspaceAction::List { json } => run_workspace_list(ws_dir, *json),
        CliWorkspaceAction::Status { hash, json } => run_workspace_status(ws_dir, hash, *json),
        CliWorkspaceAction::Stop { hash } => run_workspace_stop(ws_dir, hash),
        CliWorkspaceAction::Restart { hash, service, .. } => {
            run_workspace_restart(ws_dir, hash, service.clone())
        }
    }
}

fn run_workspace_list(ws_dir: &Path, json: bool) -> Result<(), String> {
    let workspaces = list_workspaces(ws_dir);
    let rows: Vec<WorkspaceListRow> = workspaces
        .iter()
        .map(|ws| {
            let worker_pid = worker_pid_if_alive(&ws.hash);
            let snapshot = read_snapshot(&ws.hash);
            WorkspaceListRow {
                hash: ws.hash.clone(),
                created: ws.created.clone(),
                port_offset: snapshot.as_ref().map(|s| s.port_offset),
                summary: ws.summary(),
                runtime_state: workspace_runtime_state(&ws.hash, worker_pid),
                detached: detached_marker_path(&ws.hash).exists(),
                worker_pid,
                snapshot_age_secs: snapshot.as_ref().map(snapshot_age_secs),
            }
        })
        .collect();

    if json {
        return print_json_pretty(&rows);
    }

    if rows.is_empty() {
        println!("No saved workspaces.");
        return Ok(());
    }

    println!(
        "{:<10} {:<12} {:<7} {:<8} SUMMARY",
        "HASH", "STATE", "PORT", "WORKER"
    );
    for row in rows {
        let worker = row
            .worker_pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_string());
        let port = row
            .port_offset
            .map(|n| format!("+{n}"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<10} {:<12} {:<7} {:<8} {}",
            row.hash, row.runtime_state, port, worker, row.summary
        );
    }
    Ok(())
}

fn run_workspace_status(ws_dir: &Path, hash: &str, json: bool) -> Result<(), String> {
    let ws = require_workspace(ws_dir, hash)?;
    let worker_pid = worker_pid_if_alive(hash);
    let snapshot = read_snapshot(hash);
    let row = WorkspaceStatusRow {
        hash: ws.hash.clone(),
        created: ws.created.clone(),
        port_offset: snapshot.as_ref().map(|s| s.port_offset),
        summary: ws.summary(),
        runtime_state: workspace_runtime_state(hash, worker_pid),
        detached: detached_marker_path(hash).exists(),
        worker_pid,
        snapshot_age_secs: snapshot.as_ref().map(snapshot_age_secs),
        services: snapshot.map(|s| s.services).unwrap_or_default(),
    };

    if json {
        return print_json_pretty(&row);
    }

    println!("Workspace  {}", row.hash);
    println!("Created    {}", row.created);
    println!(
        "Ports      {}",
        row.port_offset
            .map(|n| format!("+{n}"))
            .unwrap_or_else(|| "-  (chosen at launch)".to_string())
    );
    println!("State      {}", row.runtime_state);
    println!("Detached   {}", if row.detached { "yes" } else { "no" });
    if let Some(pid) = row.worker_pid {
        println!("Worker PID {}", pid);
    }
    println!("Summary    {}", row.summary);

    if let Some(age) = row.snapshot_age_secs {
        println!("Snapshot   {}", format_age(age));
    }

    if row.services.is_empty() {
        println!("Services   no live runtime snapshot available");
        return Ok(());
    }

    println!();
    println!("{:<24} {:<22} {:<8} URL", "SERVICE", "HEALTH", "PID");
    for svc in row.services {
        let pid = svc
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<24} {:<22} {:<8} {}",
            svc.name,
            svc.health,
            pid,
            svc.url.unwrap_or_default()
        );
    }
    Ok(())
}

fn run_workspace_stop(ws_dir: &Path, hash: &str) -> Result<(), String> {
    let _ = require_workspace(ws_dir, hash)?;
    let Some(worker_pid) = worker_pid_if_alive(hash) else {
        return Err(format!("workspace {hash} is not running"));
    };

    if detached_marker_path(hash).exists() {
        unsafe {
            libc::kill(worker_pid as libc::pid_t, libc::SIGTERM);
        }
        if !wait_for_worker_exit(worker_pid, Duration::from_secs(20)) {
            unsafe {
                libc::kill(worker_pid as libc::pid_t, libc::SIGKILL);
            }
        }
        clear_runtime_files(hash);
        remove_worker_pid(hash);
        println!("workspace {hash} stopped");
        return Ok(());
    }

    let response =
        request_runtime_action(hash, ControlAction::StopWorkspace, Duration::from_secs(5))
            .or_else(|_| {
                unsafe {
                    libc::kill(worker_pid as libc::pid_t, libc::SIGTERM);
                }
                Ok::<ControlResponse, String>(ControlResponse {
                    id: "signal-fallback".to_string(),
                    ok: true,
                    message: "SIGTERM sent directly to the worker".to_string(),
                    completed_at_ms: control::now_ms(),
                })
            })?;

    let stopped = wait_for_worker_exit(worker_pid, Duration::from_secs(20));
    if !stopped {
        unsafe {
            libc::kill(worker_pid as libc::pid_t, libc::SIGKILL);
        }
    }
    clear_runtime_files(hash);
    remove_worker_pid(hash);
    println!("workspace {hash} stopped ({})", response.message);
    Ok(())
}

fn run_workspace_restart(ws_dir: &Path, hash: &str, service: Option<String>) -> Result<(), String> {
    let _ = require_workspace(ws_dir, hash)?;
    let Some(worker_pid) = worker_pid_if_alive(hash) else {
        return Err(format!("workspace {hash} is not running"));
    };
    let detached = detached_marker_path(hash).exists();

    let action = match service {
        Some(service) => ControlAction::RestartService { service },
        None => ControlAction::RestartWorkspace,
    };
    let timeout = match action {
        ControlAction::RestartService { .. } => Duration::from_secs(30),
        _ => Duration::from_secs(120),
    };

    let req = queue_request(hash, action).map_err(|e| e.to_string())?;
    if detached {
        // Worker is suspended via SIGSTOP — wake it briefly so it can pick up the
        // request file. The worker re-suspends itself once the request is handled
        // (see the post-SIGCONT block in run_session_loop).
        unsafe {
            libc::kill(worker_pid as libc::pid_t, libc::SIGCONT);
        }
    }
    let response = match wait_for_response(hash, &req.id, timeout).map_err(|e| e.to_string())? {
        Some(resp) if resp.ok => resp,
        Some(resp) => return Err(resp.message),
        None => {
            return Err(format!(
                "timed out waiting for workspace {hash} to acknowledge the operation"
            ));
        }
    };
    println!("{}", response.message);
    Ok(())
}

fn publish_runtime_snapshot_for_state(slug: &str, state: &State, port_offset: u16) {
    let snapshot = {
        let svcs = state.lock().unwrap();
        WorkspaceRuntimeSnapshot {
            workspace_hash: slug.to_string(),
            worker_pid: std::process::id(),
            detached: detached_marker_path(slug).exists(),
            updated_at_ms: control::now_ms(),
            port_offset,
            services: svcs
                .iter()
                .filter(|svc| !matches!(svc.health, Health::Pending))
                .map(|svc| ServiceRuntimeSnapshot {
                    name: svc.name.clone(),
                    health: svc.health.label_plain(),
                    pid: svc.pid,
                    url: svc.url.clone(),
                    log_path: svc.log_path.display().to_string(),
                    startup_secs: svc.secs(),
                    diagnosis: svc.diagnosis.clone(),
                })
                .collect(),
        }
    };
    let _ = publish_snapshot(slug, &snapshot);
}

fn kill_proc_for_service(procs: &mut Vec<Proc>, idx: usize) {
    if let Some(pos) = procs.iter().position(|p| p.idx == idx) {
        unsafe {
            libc::kill(-procs[pos].pgid, libc::SIGKILL);
        }
        procs.remove(pos);
    }
}

fn stop_service_at_idx(state: &State, procs: &mut Vec<Proc>, idx: usize) -> Result<String, String> {
    let svc_name = {
        let svcs = state.lock().unwrap();
        svcs.get(idx)
            .map(|svc| svc.name.clone())
            .ok_or_else(|| format!("service index {idx} not found"))?
    };

    kill_proc_for_service(procs, idx);

    let mut svcs = state.lock().unwrap();
    let svc = svcs
        .get_mut(idx)
        .ok_or_else(|| format!("service index {idx} not found"))?;
    svc.health = Health::Stopped;
    svc.pid = None;
    svc.diagnosis = None;
    Ok(format!("service {svc_name} stopped"))
}

fn restart_service_at_idx(
    state: &State,
    procs: &mut Vec<Proc>,
    diagnosed: &mut HashSet<usize>,
    slug: &str,
    idx: usize,
    es_port: u16,
) -> Result<String, String> {
    let (cmd, log_path, svc_name) = {
        let svcs = state.lock().unwrap();
        let svc = svcs
            .get(idx)
            .ok_or_else(|| format!("service index {idx} not found"))?;
        (
            svc.spawn_cmd.clone(),
            svc.log_path.clone(),
            svc.name.clone(),
        )
    };

    let Some(cmd) = cmd else {
        return Err(format!(
            "service {svc_name} cannot be restarted from the CLI"
        ));
    };

    kill_proc_for_service(procs, idx);

    if cmd.requires_docker && !docker_available() {
        let mut svcs = state.lock().unwrap();
        if let Some(svc) = svcs.get_mut(idx) {
            svc.health = Health::Degraded("Docker not running — start Docker first".into());
        }
        return Err("Docker not running — start Docker first".to_string());
    }

    if svc_name == "opencti-graphql" && !opensearch_ready(es_port) {
        let mut svcs = state.lock().unwrap();
        if let Some(svc) = svcs.get_mut(idx) {
            svc.health = Health::Degraded("Waiting for OpenSearch/ES…".into());
        }
        return Err("OpenSearch is not ready yet".to_string());
    }

    let args: Vec<&str> = cmd.args.iter().map(|s| s.as_str()).collect();
    match spawn_svc(&cmd.prog, &args, &cmd.dir, &cmd.env, &log_path) {
        Ok((child, pgid)) => {
            record_pid(slug, child.id());
            diagnosed.remove(&idx);
            let mut svcs = state.lock().unwrap();
            if let Some(svc) = svcs.get_mut(idx) {
                let has_url = svc.url.is_some();
                svc.health = if has_url {
                    Health::Launching
                } else {
                    Health::Running
                };
                svc.pid = Some(child.id());
                svc.started_at = Some(Instant::now());
                svc.restarted_at = Some(Instant::now());
                svc.diagnosis = None;
            }
            procs.push(Proc { idx, pgid, child });
            Ok(format!("service {svc_name} restarted"))
        }
        Err(e) => {
            let mut svcs = state.lock().unwrap();
            if let Some(svc) = svcs.get_mut(idx) {
                svc.health = Health::Degraded(e.to_string());
            }
            Err(e.to_string())
        }
    }
}

fn restart_service_by_name(
    state: &State,
    procs: &mut Vec<Proc>,
    diagnosed: &mut HashSet<usize>,
    slug: &str,
    service: &str,
    es_port: u16,
) -> Result<String, String> {
    let idx = {
        let svcs = state.lock().unwrap();
        svcs.iter()
            .enumerate()
            .find(|(_, svc)| svc.name == service)
            .map(|(idx, _)| idx)
            .ok_or_else(|| format!("service {service} not found"))?
    };
    restart_service_at_idx(state, procs, diagnosed, slug, idx, es_port)
}

fn restart_workspace_runtime(
    state: &State,
    procs: &mut Vec<Proc>,
    diagnosed: &mut HashSet<usize>,
    slug: &str,
    docker_projects: &[DockerProject],
) -> Result<String, String> {
    for proc in procs.iter_mut() {
        unsafe {
            libc::kill(-proc.pgid, libc::SIGKILL);
        }
    }
    procs.clear();
    diagnosed.clear();

    {
        let mut svcs = state.lock().unwrap();
        for svc in svcs.iter_mut() {
            if !matches!(svc.health, Health::Pending) {
                svc.pid = None;
                svc.diagnosis = None;
                if svc.spawn_cmd.is_some() {
                    svc.health = Health::Launching;
                }
            }
        }
    }

    for dp in docker_projects {
        let proj = dp.project.as_str();
        run_blocking("docker", &["compose", "-p", proj, "restart"], &dp.work_dir);
    }

    let spawn_targets: Vec<(usize, SpawnCmd, PathBuf)> = {
        let svcs = state.lock().unwrap();
        svcs.iter()
            .enumerate()
            .filter_map(|(i, svc)| {
                svc.spawn_cmd
                    .clone()
                    .map(|cmd| (i, cmd, svc.log_path.clone()))
            })
            .collect()
    };

    for (idx, cmd, log_path) in spawn_targets {
        let args: Vec<&str> = cmd.args.iter().map(|s| s.as_str()).collect();
        match spawn_svc(&cmd.prog, &args, &cmd.dir, &cmd.env, &log_path) {
            Ok((child, pgid)) => {
                record_pid(slug, child.id());
                let mut svcs = state.lock().unwrap();
                if let Some(svc) = svcs.get_mut(idx) {
                    let has_url = svc.url.is_some();
                    svc.health = if has_url {
                        Health::Launching
                    } else {
                        Health::Running
                    };
                    svc.pid = Some(child.id());
                    svc.started_at = Some(Instant::now());
                    svc.restarted_at = Some(Instant::now());
                    svc.diagnosis = None;
                }
                procs.push(Proc { idx, pgid, child });
            }
            Err(e) => {
                let mut svcs = state.lock().unwrap();
                if let Some(svc) = svcs.get_mut(idx) {
                    svc.health = Health::Degraded(e.to_string());
                }
            }
        }
    }

    Ok("workspace restart completed".to_string())
}

/// Collect every host base port the enabled products will try to bind on
/// this launch and pick the smallest offset (step 10) for which they're all
/// free on the host. Pure dynamic — nothing is read from or saved to the
/// workspace config.
fn compute_dynamic_port_offset(
    paths: &Paths,
    no_copilot: bool,
    no_opencti: bool,
    no_openaev: bool,
    no_connector: bool,
) -> u16 {
    let mut bases: Vec<u16> = Vec::new();
    if !no_copilot && paths.copilot.is_dir() {
        bases.extend(base_ports_for(
            "copilot",
            Some(&paths.copilot.join("docker-compose.dev.yml")),
        ));
    }
    if !no_opencti && paths.opencti.is_dir() {
        bases.extend(base_ports_for(
            "opencti",
            Some(
                &paths
                    .opencti
                    .join("opencti-platform/opencti-dev/docker-compose.yml"),
            ),
        ));
    }
    if !no_openaev && paths.openaev.is_dir() {
        bases.extend(base_ports_for(
            "openaev",
            Some(&paths.openaev.join("openaev-dev/docker-compose.yml")),
        ));
    }
    if !no_connector && paths.connector.is_dir() {
        bases.extend(base_ports_for("connector", None));
    }
    find_free_port_offset(&bases)
}

#[derive(Copy, Clone)]
enum BackChoice {
    Stop,
    Detach,
    Cancel,
}

/// Render a 3-option confirm prompt when the user hits Back from the stack
/// overview. Caller must drop its raw-mode guard before calling and re-enter
/// the TUI afterwards.
fn prompt_back_action(stopping: &Arc<AtomicBool>, input_paused: &Arc<AtomicBool>) -> BackChoice {
    let _input_pause = InputPauseGuard::new(input_paused);
    let options: &[(BackChoice, &str, &str)] = &[
        (
            BackChoice::Stop,
            "Stop the stack",
            "Shut down all services and return to the workspace menu",
        ),
        (
            BackChoice::Detach,
            "Detach",
            "Keep services running and return to the workspace menu",
        ),
        (BackChoice::Cancel, "Cancel", "Stay on the stack overview"),
    ];
    let mut cursor = 0usize;

    ensure_cooked_output();
    let render = |cur: usize| {
        let sep = "─".repeat(64);
        print!("\x1b[H\x1b[2J");
        print!("\r\n  {BOLD}{YLW}⚠  Leave stack overview{R}\r\n");
        print!("\r\n  {DIM}{sep}{R}\r\n\r\n");
        for (i, (_, label, desc)) in options.iter().enumerate() {
            let arrow = if i == cur {
                format!("{CYN}{BOLD}▸{R}")
            } else {
                " ".to_string()
            };
            let label_fmt = if i == cur {
                format!("{BOLD}{CYN}{:<18}{R}", label)
            } else {
                format!("{DIM}{:<18}{R}", label)
            };
            print!("  {arrow} {label_fmt}  {DIM}{desc}{R}\r\n");
        }
        print!("\r\n  {DIM}{sep}{R}\r\n\r\n");
        print!("  {DIM}↑↓ navigate   Enter confirm   s stop   m detach   Esc cancel{R}\r\n");
        let _ = io::stdout().flush();
    };
    render(cursor);

    let _ = crossterm::terminal::enable_raw_mode();
    let choice = loop {
        if stopping.load(Ordering::Relaxed) {
            break BackChoice::Cancel;
        }
        if !event::poll(Duration::from_millis(100)).unwrap_or(false) {
            continue;
        }
        let Ok(Event::Key(k)) = event::read() else {
            continue;
        };
        if k.kind != crossterm::event::KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => {
                cursor = cursor.saturating_sub(1);
                render(cursor);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if cursor + 1 < options.len() {
                    cursor += 1;
                }
                render(cursor);
            }
            KeyCode::Enter => break options[cursor].0,
            KeyCode::Char('s') | KeyCode::Char('S') => break BackChoice::Stop,
            KeyCode::Char('m') | KeyCode::Char('M') => break BackChoice::Detach,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => break BackChoice::Cancel,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                break BackChoice::Cancel;
            }
            _ => {}
        }
    };
    let _ = disable_raw_mode();
    choice
}

fn handle_control_request(
    slug: &str,
    state: &State,
    procs: &mut Vec<Proc>,
    diagnosed: &mut HashSet<usize>,
    docker_projects: &[DockerProject],
    es_port: u16,
    stopping: &Arc<AtomicBool>,
) -> bool {
    let Some(req) = take_request(slug) else {
        return false;
    };

    let (ok, message, should_stop) = match req.action {
        ControlAction::StopWorkspace => (true, "workspace shutdown requested".to_string(), true),
        ControlAction::RestartWorkspace => {
            match restart_workspace_runtime(state, procs, diagnosed, slug, docker_projects) {
                Ok(msg) => (true, msg, false),
                Err(err) => (false, err, false),
            }
        }
        ControlAction::RestartService { service } => {
            match restart_service_by_name(state, procs, diagnosed, slug, &service, es_port) {
                Ok(msg) => (true, msg, false),
                Err(err) => (false, err, false),
            }
        }
    };

    let response = ControlResponse {
        id: req.id,
        ok,
        message,
        completed_at_ms: control::now_ms(),
    };
    let _ = publish_response(slug, &response);
    if should_stop {
        stopping.store(true, Ordering::Relaxed);
    }
    true
}

fn run_as_selector(args: &Args, workspace_root: &Path, ws_dir: &Path) {
    print!("\x1b[H\x1b[2J");
    let _ = io::stdout().flush();

    let selector_stopping = Arc::new(AtomicBool::new(false));
    {
        let f = Arc::clone(&selector_stopping);
        ctrlc::set_handler(move || f.store(true, Ordering::Relaxed))
            .expect("failed to set Ctrl+C handler");
    }

    let exe = std::env::current_exe().expect("cannot determine current executable path");
    let mut stopped: Vec<StoppedSession> = Vec::new();

    let has_direct = args.workspace.is_some()
        || args.copilot_branch.is_some()
        || args.opencti_branch.is_some()
        || args.openaev_branch.is_some()
        || args.connector_branch.is_some()
        || args.copilot_commit.is_some()
        || args.opencti_commit.is_some()
        || args.openaev_commit.is_some()
        || args.connector_commit.is_some()
        || args.copilot_worktree.is_some()
        || args.opencti_worktree.is_some()
        || args.openaev_worktree.is_some()
        || args.connector_worktree.is_some();

    if has_direct {
        let (cfg, _, _) = resolve_workspace(args, workspace_root, ws_dir);
        loop {
            if selector_stopping.load(Ordering::Relaxed) {
                break;
            }
            let prev_stopped = stopped.len();
            let pid = spawn_session_worker(&exe, &cfg.hash, args.clean_start);
            let clean = wait_for_session(pid, &cfg.hash, &mut stopped);
            if selector_stopping.load(Ordering::Relaxed) {
                break;
            }
            let was_detached = stopped.len() > prev_stopped;
            // Clean exit (q pressed) and not a detach → show workspace selector.
            // Any other exit (crash, non-0) loops back and restarts the session.
            if clean && !was_detached {
                break;
            }
        }
        if selector_stopping.load(Ordering::Relaxed) {
            // Ctrl+C — terminate any detached sessions and exit.
            for s in &stopped {
                unsafe {
                    libc::kill(s.pid as libc::pid_t, libc::SIGTERM);
                }
            }
            return;
        }
        // q pressed — fall through to the workspace selector below.
    }

    startup_orphan_check(ws_dir, workspace_root, &mut stopped);

    'selector: loop {
        if selector_stopping.load(Ordering::Relaxed) {
            for s in &stopped {
                unsafe {
                    libc::kill(s.pid as libc::pid_t, libc::SIGTERM);
                }
            }
            print!("\x1b[H\x1b[2J");
            let _ = io::stdout().flush();
            break;
        }

        ensure_cooked_output();
        let stopped_hashes: HashSet<String> = stopped.iter().map(|s| s.hash.clone()).collect();

        let workspaces = list_workspaces(ws_dir);
        if workspaces.is_empty() {
            let (cfg, _, clean) = build_new_workspace_interactive(workspace_root, ws_dir);
            let pid = spawn_session_worker(&exe, &cfg.hash, clean);
            wait_for_session(pid, &cfg.hash, &mut stopped);
            continue 'selector;
        }

        drain_input_events();
        let action = loop {
            let workspaces = list_workspaces(ws_dir);
            match run_workspace_selector(&workspaces, &stopped_hashes) {
                SelectorWorkspaceAction::Delete(cfg) => {
                    if let Some(pos) = stopped.iter().position(|s| s.hash == cfg.hash) {
                        let s = stopped.remove(pos);
                        unsafe {
                            libc::kill(s.pid as libc::pid_t, libc::SIGTERM);
                        }
                        thread::sleep(Duration::from_millis(500));
                        unsafe {
                            libc::kill(s.pid as libc::pid_t, libc::SIGKILL);
                        }
                        let _ = fs::remove_file(detached_marker_path(&s.hash));
                        remove_worker_pid(&s.hash);
                    }
                    run_workspace_delete(&cfg, workspace_root, ws_dir);
                }
                SelectorWorkspaceAction::OpenInCode(cfg) => {
                    if let Some(dir) = workspace_code_dir(&cfg, workspace_root) {
                        open_dir_in_vscode(&dir);
                    }
                }
                other => break other,
            }
        };

        match action {
            SelectorWorkspaceAction::Reattach(cfg) => {
                if let Some(pos) = stopped.iter().position(|s| s.hash == cfg.hash) {
                    let s = stopped.remove(pos);
                    unsafe {
                        libc::kill(s.pid as libc::pid_t, libc::SIGCONT);
                    }
                    if s.adopted {
                        wait_for_adopted_session(s.pid, &s.hash, &mut stopped);
                    } else {
                        wait_for_session(s.pid, &s.hash, &mut stopped);
                    }
                }
            }
            SelectorWorkspaceAction::StopSession(cfg) => {
                if let Some(pos) = stopped.iter().position(|s| s.hash == cfg.hash) {
                    let s = stopped.remove(pos);
                    unsafe {
                        libc::kill(s.pid as libc::pid_t, libc::SIGTERM);
                    }
                    thread::sleep(Duration::from_millis(500));
                    unsafe {
                        libc::kill(s.pid as libc::pid_t, libc::SIGKILL);
                    }
                    let _ = fs::remove_file(detached_marker_path(&s.hash));
                    remove_worker_pid(&s.hash);
                }
            }
            SelectorWorkspaceAction::Open(cfg) => {
                let mut choices = workspace_to_choices(&cfg, workspace_root);
                drain_input_events();
                let clean = match run_product_selector(&cfg.hash, &mut choices) {
                    LaunchMode::Quit => continue 'selector,
                    LaunchMode::Clean => true,
                    LaunchMode::Normal => false,
                };
                let updated_cfg = choices_to_workspace(&choices);
                save_workspace(ws_dir, &updated_cfg);
                let pid = spawn_session_worker(&exe, &updated_cfg.hash, clean);
                wait_for_session(pid, &updated_cfg.hash, &mut stopped);
            }
            SelectorWorkspaceAction::CreateNew => {
                let mut choices = default_product_choices(workspace_root);
                drain_input_events();
                let clean = match run_product_selector("new", &mut choices) {
                    LaunchMode::Quit => continue 'selector,
                    LaunchMode::Clean => true,
                    LaunchMode::Normal => false,
                };
                let cfg = choices_to_workspace(&choices);
                save_workspace(ws_dir, &cfg);
                let pid = spawn_session_worker(&exe, &cfg.hash, clean);
                wait_for_session(pid, &cfg.hash, &mut stopped);
            }
            SelectorWorkspaceAction::Delete(_) | SelectorWorkspaceAction::OpenInCode(_) => {
                // Handled inline in the inner loop above.
            }
            SelectorWorkspaceAction::Quit => {
                if !stopped.is_empty() {
                    println!("\n  Detached session(s) still running:");
                    for s in &stopped {
                        println!("    {CYN}●{R}  {}", s.hash);
                    }
                    print!("\n  Shut them down? [y/N] ");
                    let _ = io::stdout().flush();
                    let mut ans = String::new();
                    let _ = io::stdin().read_line(&mut ans);
                    if ans.trim().eq_ignore_ascii_case("y") {
                        for s in &stopped {
                            unsafe {
                                libc::kill(s.pid as libc::pid_t, libc::SIGTERM);
                            }
                        }
                        thread::sleep(Duration::from_secs(1));
                        for s in &stopped {
                            unsafe {
                                libc::kill(s.pid as libc::pid_t, libc::SIGKILL);
                            }
                        }
                    }
                }
                print!("\x1b[H\x1b[2J");
                let _ = io::stdout().flush();
                return;
            }
        }
    }
}

/// Ask (once per workspace) whether OpenCTI should connect to XTM One / Copilot.
/// Saves `XTM_ONE_ENABLED=true/false` into the opencti workspace env so the
/// answer persists across restarts without re-prompting.
fn prompt_xtm_one_opencti_integration(opencti_env: &Path) {
    let mut env = parse_env_file(opencti_env);
    if env.contains_key("XTM_ONE_ENABLED") {
        return; // Already answered for this workspace
    }
    println!();
    println!("  {BOLD}OpenCTI + XTM One (Copilot) integration{R}");
    println!("  {DIM}When enabled, OpenCTI will connect to the running Copilot instance{R}");
    println!("  {DIM}using its platform registration token once it has started.{R}");
    println!();
    print!("  Enable XTM One integration in OpenCTI? [y/N] ");
    let _ = io::stdout().flush();
    let enabled = match read_line_or_interrupt() {
        Some(l) => l.trim().eq_ignore_ascii_case("y"),
        None => false,
    };
    env.insert(
        "XTM_ONE_ENABLED".to_string(),
        if enabled { "true" } else { "false" }.to_string(),
    );
    write_env_file(opencti_env, &env);
    if enabled {
        println!(
            "  {GRN}✓{R}  Integration enabled — XTM One token will be injected when Copilot is up.\n"
        );
    } else {
        println!("  {DIM}Integration disabled for this workspace.{R}\n");
    }
}

/// Ask (once per workspace) whether OpenAEV should connect to XTM One / Copilot.
/// A sidecar marker is used so legacy workspaces that were auto-enabled still get
/// prompted once after this feature is introduced.
fn prompt_xtm_one_openaev_integration(openaev_env: &Path) {
    let marker = openaev_env
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".openaev-xtm-one-prompted");
    if marker.exists() {
        return;
    }

    let mut env = parse_env_file(openaev_env);
    println!();
    println!("  {BOLD}OpenAEV + XTM One (Copilot) integration{R}");
    println!("  {DIM}When enabled, OpenAEV will connect to the running Copilot instance{R}");
    println!("  {DIM}using its platform registration token once it has started.{R}");
    println!();
    print!("  Enable XTM One integration in OpenAEV? [y/N] ");
    let _ = io::stdout().flush();
    let enabled = match read_line_or_interrupt() {
        Some(l) => l.trim().eq_ignore_ascii_case("y"),
        None => false,
    };

    env.insert(
        "OPENAEV_XTM_ONE_ENABLE".to_string(),
        if enabled { "true" } else { "false" }.to_string(),
    );
    if !enabled {
        env.remove("OPENAEV_XTM_ONE_URL");
        env.remove("OPENAEV_XTM_ONE_TOKEN");
    }
    write_env_file(openaev_env, &env);
    let _ = fs::write(marker, "");

    if enabled {
        println!(
            "  {GRN}✓{R}  Integration enabled — XTM One token will be injected when Copilot is up.\n"
        );
    } else {
        println!("  {DIM}Integration disabled for this workspace.{R}\n");
    }
}

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

fn resolve_workspace(
    args: &Args,
    workspace_root: &Path,
    ws_dir: &Path,
) -> (WorkspaceConfig, Vec<ProductChoice>, bool) {
    if let Some(hash) = &args.workspace {
        match load_workspace(ws_dir, hash) {
            Some(cfg) => {
                let choices = workspace_to_choices(&cfg, workspace_root);
                (cfg, choices, args.clean_start)
            }
            None => {
                eprintln!("Workspace '{}' not found in {}.", hash, ws_dir.display());
                std::process::exit(1);
            }
        }
    } else if args.copilot_branch.is_some()
        || args.opencti_branch.is_some()
        || args.openaev_branch.is_some()
        || args.connector_branch.is_some()
        || args.copilot_commit.is_some()
        || args.opencti_commit.is_some()
        || args.openaev_commit.is_some()
        || args.connector_commit.is_some()
        || args.copilot_worktree.is_some()
        || args.opencti_worktree.is_some()
        || args.openaev_worktree.is_some()
        || args.connector_worktree.is_some()
    {
        let entries = build_entries_from_branches(args);
        let hash = compute_workspace_hash(&entries);
        let cfg = load_workspace(ws_dir, &hash).unwrap_or_else(|| {
            let c = WorkspaceConfig {
                hash: hash.clone(),
                created: today(),
                entries,
            };
            save_workspace(ws_dir, &c);
            c
        });
        let choices = workspace_to_choices(&cfg, workspace_root);
        (cfg, choices, false)
    } else {
        // Interactive path — only reached for direct-args invocations that somehow
        // don't match any branch flag. In normal flow, run_as_selector handles the
        // interactive workspace/product selection before spawning a session worker.
        let mut choices = 'selector: loop {
            let workspaces = list_workspaces(ws_dir);
            if workspaces.is_empty() {
                return build_new_workspace_interactive(workspace_root, ws_dir);
            }
            drain_input_events();
            match run_workspace_selector(&workspaces, &HashSet::new()) {
                SelectorWorkspaceAction::Delete(cfg) => {
                    run_workspace_delete(&cfg, workspace_root, ws_dir);
                    continue 'selector;
                }
                SelectorWorkspaceAction::OpenInCode(cfg) => {
                    if let Some(dir) = workspace_code_dir(&cfg, workspace_root) {
                        open_dir_in_vscode(&dir);
                    }
                    continue 'selector;
                }
                SelectorWorkspaceAction::Open(cfg) => {
                    break workspace_to_choices(&cfg, workspace_root);
                }
                SelectorWorkspaceAction::CreateNew => {
                    break default_product_choices(workspace_root);
                }
                SelectorWorkspaceAction::Reattach(cfg) => {
                    break workspace_to_choices(&cfg, workspace_root);
                }
                SelectorWorkspaceAction::StopSession(_) => {
                    continue 'selector;
                }
                SelectorWorkspaceAction::Quit => {
                    print!("\x1b[H\x1b[2J");
                    let _ = io::stdout().flush();
                    std::process::exit(0);
                }
            }
        };
        drain_input_events();
        let clean = match run_product_selector("", &mut choices) {
            LaunchMode::Quit => {
                print!("\x1b[H\x1b[2J");
                let _ = io::stdout().flush();
                std::process::exit(0);
            }
            LaunchMode::Clean => true,
            LaunchMode::Normal => false,
        };
        let cfg = choices_to_workspace(&choices);
        save_workspace(ws_dir, &cfg);
        (cfg, choices, clean)
    }
}

// ── Worktree scaffold ─────────────────────────────────────────────────────────

/// Scan each enabled product's worktree directory for files that dev-launcher
/// normally generates (docker-compose override, deployed env file).  When a
/// worktree was created externally — e.g. via `gh pr checkout` or GitHub
/// Codespaces — those files are absent.  This step generates them so the
/// worktree is immediately usable without waiting for the full env wizard.
///
/// Silent when everything is already in place; prints a summary section only
/// when at least one file is generated.
#[allow(clippy::too_many_arguments)]
fn scan_and_scaffold_worktrees(
    paths: &Paths,
    no_copilot: bool,
    no_opencti: bool,
    no_openaev: bool,
    no_connector: bool,
    ws_hash: &str,
    port_offset: u16,
    ws_env_dir: &Path,
) {
    struct ProductCheck {
        label: &'static str,
        key: &'static str,
        dir: PathBuf,
        /// Compose file path relative to `dir`, or None for Python-only services.
        compose_rel: Option<&'static str>,
        /// Absolute path where the deployed env file should live in the worktree.
        env_dest: PathBuf,
        enabled: bool,
    }

    let products = [
        ProductCheck {
            label: "Copilot",
            key: "copilot",
            dir: paths.copilot.clone(),
            compose_rel: Some("docker-compose.dev.yml"),
            env_dest: paths.copilot.join(".env"),
            enabled: !no_copilot,
        },
        ProductCheck {
            label: "OpenCTI",
            key: "opencti",
            dir: paths.opencti.clone(),
            compose_rel: Some("opencti-platform/opencti-dev/docker-compose.yml"),
            env_dest: paths
                .opencti
                .join("opencti-platform/opencti-graphql/.env.dev"),
            enabled: !no_opencti,
        },
        ProductCheck {
            label: "OpenAEV",
            key: "openaev",
            dir: paths.openaev.clone(),
            compose_rel: Some("openaev-dev/docker-compose.yml"),
            env_dest: paths.openaev.join("openaev-dev/.env"),
            enabled: !no_openaev,
        },
        ProductCheck {
            label: "Connector",
            key: "connector",
            dir: paths.connector.clone(),
            compose_rel: None,
            env_dest: paths.connector.join(".env.dev"),
            enabled: !no_connector,
        },
    ];

    let sep = "─".repeat(56);
    let mut header_printed = false;

    for p in &products {
        if !p.enabled || !p.dir.is_dir() {
            continue;
        }

        let mut scaffolded: Vec<String> = Vec::new();

        // Docker compose override — refresh it on every launch so port-offset and
        // container-name fixes are propagated to existing workspaces too.
        if let Some(compose_rel) = p.compose_rel {
            let compose = p.dir.join(compose_rel);
            if compose.exists() {
                let override_path = compose
                    .parent()
                    .map(|d| d.join("docker-compose.override-devlauncher.yml"))
                    .unwrap_or_else(|| p.dir.join("docker-compose.override-devlauncher.yml"));
                let existed = override_path.exists();
                if write_compose_override(&compose, ws_hash, port_offset).is_some() && !existed {
                    scaffolded.push("docker-compose.override-devlauncher.yml".to_string());
                }
            }
        }

        // Deployed env file — copy from workspace env dir when the target is absent
        // but the workspace copy already exists (e.g. a second worktree on same branch).
        let ws_env = ws_env_path(ws_env_dir, p.key);
        if !p.env_dest.exists() && ws_env.exists() {
            deploy_workspace_env(&ws_env, &p.env_dest);
            let name = p
                .env_dest
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("env")
                .to_string();
            scaffolded.push(name);
        }

        if scaffolded.is_empty() {
            continue;
        }

        if !header_printed {
            println!("\n  {DIM}{sep}{R}");
            println!("  {BOLD}Scanning worktrees{R}  {DIM}— generating missing workspace files{R}");
            println!("  {DIM}{sep}{R}\n");
            header_printed = true;
        }
        for f in &scaffolded {
            println!("  {GRN}✓{R}  {label}  {DIM}{f}{R}", label = p.label);
        }
    }

    if header_printed {
        println!();
    }
}

// ── Session worker ────────────────────────────────────────────────────────────

fn run_session_loop(args: &Args, workspace_root: &Path, ws_dir: &Path) {
    print!("\x1b[H\x1b[2J");
    let _ = io::stdout().flush();

    let stopping: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    {
        let stopping = Arc::clone(&stopping);
        ctrlc::set_handler(move || {
            stopping.store(true, Ordering::Relaxed);
        })
        .expect("failed to set Ctrl+C handler");
    }
    unsafe {
        libc::signal(
            libc::SIGHUP,
            sighup_handler as *const () as libc::sighandler_t,
        );
    }

    #[allow(clippy::never_loop)]
    'session: loop {
        stopping.store(false, Ordering::Relaxed);
        SIGHUP_STOP.store(false, Ordering::Relaxed);

        let (workspace_cfg, choices, clean_start) = resolve_workspace(args, workspace_root, ws_dir);
        ensure_cooked_output();
        let slug = workspace_cfg.hash.clone();

        let logs_dir = args.logs_dir.clone().unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            let ts = std::process::Command::new("date")
                .arg("+%Y%m%d-%H%M%S")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs().to_string())
                        .unwrap_or_else(|_| "unknown".to_string())
                });
            PathBuf::from(format!("{home}/.dev-launcher/logs/{slug}/{ts}"))
        });
        fs::create_dir_all(&logs_dir).expect("cannot create logs_dir");
        launcher_log::init(&logs_dir.join("dev-launcher.log"));
        llog!("workspace={slug}  logs={}", logs_dir.display());
        let recipes_dir = tui::splash::run();
        diagnosis::recipe::init(&recipes_dir);

        let get_worktree_override = |repo: &str| -> Option<&PathBuf> {
            match repo {
                "filigran-copilot" => args.copilot_worktree.as_ref(),
                "opencti" => args.opencti_worktree.as_ref(),
                "openaev" => args.openaev_worktree.as_ref(),
                "connectors" => args.connector_worktree.as_ref(),
                _ => None,
            }
        };

        {
            let sep = "─".repeat(56);
            let need_worktrees = choices.iter().any(|c| {
                c.enabled && !c.branch.is_empty() && get_worktree_override(c.repo).is_none() && {
                    let target =
                        workspace_root.join(format!("{}-{}", c.repo, branch_to_slug(&c.branch)));
                    let main = workspace_root.join(c.repo);
                    !target.is_dir() && main.is_dir() && current_branch(&main) != c.branch.as_str()
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
                    ensure_worktree(workspace_root, c.repo, &c.branch);
                }
            }
            if need_worktrees {
                println!();
            }
        }

        let paths = {
            let resolve_path =
                |repo: &str, branch: &str, override_path: Option<&PathBuf>| -> PathBuf {
                    if let Some(p) = override_path {
                        return p.clone();
                    }
                    if branch.is_empty() {
                        return workspace_root.join(repo);
                    }
                    let slug = branch_to_slug(branch);
                    let wt = workspace_root.join(format!("{}-{}", repo, slug));
                    if wt.is_dir() {
                        return wt;
                    }
                    let main = workspace_root.join(repo);
                    if current_branch(&main) == branch {
                        return main;
                    }
                    main
                };
            Paths {
                copilot: resolve_path(
                    choices[0].repo,
                    &choices[0].branch,
                    get_worktree_override(choices[0].repo),
                ),
                opencti: resolve_path(
                    choices[1].repo,
                    &choices[1].branch,
                    get_worktree_override(choices[1].repo),
                ),
                openaev: resolve_path(
                    choices[2].repo,
                    &choices[2].branch,
                    get_worktree_override(choices[2].repo),
                ),
                connector: resolve_path(
                    choices[3].repo,
                    &choices[3].branch,
                    get_worktree_override(choices[3].repo),
                )
                .join("internal-import-file/import-document-ai"),
                // Infra products: always use the fixed workspace_root dir (no branches/worktrees).
                grafana: workspace_root.join("grafana"),
                langfuse: workspace_root.join("langfuse"),
                // Isolated venv for copilot-infinity, separate from the copilot backend venv.
                infinity: ws_dir.join(&slug).join("infinity-emb"),
                // Isolated venv for the autoresearch runner service.
                autoresearch: ws_dir.join(&slug).join("autoresearch-runner"),
            }
        };

        let no_copilot = !(choices[0].enabled && paths.copilot.is_dir());
        let no_opencti = !(choices[1].enabled && paths.opencti.is_dir());
        let no_openaev = !(choices[2].enabled && paths.openaev.is_dir());
        let no_connector = !(choices[3].enabled && paths.connector.is_dir());
        let no_grafana = !choices[4].enabled;
        let no_langfuse = !choices[5].enabled;
        let no_infinity = !choices[6].enabled;
        // choices[6].branch stores the HuggingFace model ID selected in the model picker.
        let infinity_model = {
            let m = choices[6].branch.clone();
            if m.is_empty() {
                "nomic-ai/nomic-embed-text-v1.5".to_string()
            } else {
                m
            }
        };
        let no_autoresearch = !choices[7].enabled;
        let no_opencti_front = no_opencti || args.no_opencti_front;
        let no_openaev_front = no_openaev || args.no_openaev_front;

        // ── Port offset — chosen dynamically per launch ───────────────────────────
        // Scan the host: if the default ports for every enabled service are free,
        // we use offset 0. Otherwise we step in increments of 10 until everything
        // we need is free. Nothing is persisted — next launch rescans.
        let port_offset =
            compute_dynamic_port_offset(&paths, no_copilot, no_opencti, no_openaev, no_connector);
        let es_port: u16 = 9200u16.saturating_add(port_offset);
        let opencti_gql_port: u16 = 4000u16.saturating_add(port_offset);
        let openaev_be_port: u16 = 8080u16.saturating_add(port_offset);
        if port_offset > 0 {
            println!(
                "  {DIM}Port offset +{port_offset}  \
             (opencti:{opencti_gql_port}  es:{es_port}  openaev:{openaev_be_port}){R}"
            );
        } else {
            println!("  {DIM}Port offset 0  (defaults are free){R}");
        }

        if !no_opencti && paths.opencti.is_dir() {
            let gql_dir = paths.opencti.join("opencti-platform/opencti-graphql");
            let env_file = gql_dir.join(".env.dev");
            if gql_dir.is_dir() {
                let front_dir = paths.opencti.join("opencti-platform/opencti-front/src");
                let mut flag_set: std::collections::BTreeSet<String> = Default::default();
                discover_flags_in_dir(&gql_dir.join("src"), &mut flag_set);
                if front_dir.is_dir() {
                    discover_flags_in_dir(&front_dir, &mut flag_set);
                }
                let discovered: Vec<String> = flag_set.into_iter().collect();
                if !discovered.is_empty() {
                    ensure_opencti_env(&gql_dir);
                    let active = read_active_flags(&env_file);
                    let mut flag_choices: Vec<FlagChoice> = discovered
                        .iter()
                        .map(|f| FlagChoice {
                            name: f.clone(),
                            enabled: active.contains(f),
                        })
                        .collect();
                    run_flag_selector(&slug, "OpenCTI", &mut flag_choices);
                    let selected: Vec<String> = flag_choices
                        .into_iter()
                        .filter(|f| f.enabled)
                        .map(|f| f.name)
                        .collect();
                    write_active_flags(&env_file, &selected);
                }
            }
        }

        let llm_cfg = {
            let dev_cfg = load_config();
            diagnosis::llm::resolve_llm_config(dev_cfg.as_ref())
        };
        if llm_cfg.is_some() {
            println!("  {DIM}LLM diagnosis enabled.{R}");
        }

        let state: State = Arc::new(Mutex::new(Vec::new()));
        let mut procs: Vec<Proc> = Vec::new();

        let (diag_tx, diag_rx) = mpsc::sync_channel::<DiagEvent>(32);
        let mut diagnosed: HashSet<usize> = HashSet::new();

        kill_orphaned_pids(&slug);
        clear_runtime_files(&slug);
        write_worker_pid(&slug, std::process::id());

        let ws_env_dir = ws_dir.join(&slug);
        let _ = fs::create_dir_all(&ws_env_dir);

        scan_and_scaffold_worktrees(
            &paths,
            no_copilot,
            no_opencti,
            no_openaev,
            no_connector,
            &slug,
            port_offset,
            &ws_env_dir,
        );

        // ── Step 1 / 2 — Environment ──────────────────────────────────────────────
        let sep = "─".repeat(56);
        println!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}");
        println!("\n  {DIM}{sep}{R}");
        println!("  {BOLD}Step 1 / 2  —  Environment{R}");
        println!("  {DIM}{sep}{R}\n");

        if !no_copilot && paths.copilot.is_dir() {
            let env_path = ws_env_path(&ws_env_dir, "copilot");
            let templates: Vec<PathBuf> = [".env.sample", ".env.example"]
                .iter()
                .map(|f| paths.copilot.join(f))
                .collect();
            init_workspace_env(
            &env_path,
            Some(&paths.copilot.join(".env")),
            &templates,
            "# Copilot dev environment\nADMIN_EMAIL=admin@example.com\nADMIN_PASSWORD=ChangeMe\n",
        );
            let compose_dev = paths.copilot.join("docker-compose.dev.yml");
            preflight_port_checks(
                &env_path,
                &compose_dev,
                &[
                    PortCheck {
                        label: "REDIS_URL",
                        env_key: "REDIS_URL",
                        default_value: "redis://localhost:6379",
                        container_port: 6379,
                    },
                    PortCheck {
                        label: "S3_ENDPOINT",
                        env_key: "S3_ENDPOINT",
                        default_value: "localhost:9000",
                        container_port: 9000,
                    },
                ],
            );
            patch_url_default(&env_path, "BASE_URL", 8000, 8100);
            patch_url_default(&env_path, "FRONTEND_URL", 3000, 3100);
            // Ensure INFINITY_URL is present for new workspaces created before this feature.
            {
                let mut m = parse_env_file(&env_path);
                if !m.contains_key("INFINITY_URL") {
                    m.insert(
                        "INFINITY_URL".to_string(),
                        "http://localhost:7997".to_string(),
                    );
                    write_env_file(&env_path, &m);
                }
                if !m.contains_key("INFINITY_MODEL") {
                    m.insert(
                        "INFINITY_MODEL".to_string(),
                        "nomic-ai/nomic-embed-text-v1.5".to_string(),
                    );
                    write_env_file(&env_path, &m);
                }
                if !m.contains_key("DEFAULT_EMBEDDING_PROVIDER") {
                    m.insert(
                        "DEFAULT_EMBEDDING_PROVIDER".to_string(),
                        "custom_openai".to_string(),
                    );
                    write_env_file(&env_path, &m);
                }
                if !m.contains_key("DEFAULT_EMBEDDING_PROVIDER_BASE_URL") {
                    m.insert(
                        "DEFAULT_EMBEDDING_PROVIDER_BASE_URL".to_string(),
                        "http://localhost:7997/v1".to_string(),
                    );
                    write_env_file(&env_path, &m);
                }
                // Migrate stale name written by an earlier version of dev-launcher.
                let stale_model = m
                    .get("DEFAULT_EMBEDDING_MODEL")
                    .map(|v| v == "text-embedding-nomic-embed-text-v1.5")
                    .unwrap_or(false);
                if !m.contains_key("DEFAULT_EMBEDDING_MODEL") || stale_model {
                    m.insert(
                        "DEFAULT_EMBEDDING_MODEL".to_string(),
                        "nomic-ai/nomic-embed-text-v1.5".to_string(),
                    );
                    write_env_file(&env_path, &m);
                }
                // AutoResearch runner — inject defaults on first launch.
                let ar_port = 8400u16.saturating_add(port_offset);
                if !m.contains_key("AUTORESEARCH_URL") {
                    m.insert(
                        "AUTORESEARCH_URL".to_string(),
                        format!("http://localhost:{ar_port}"),
                    );
                    write_env_file(&env_path, &m);
                }
                if !m.contains_key("AUTORESEARCH_API_KEY") {
                    m.insert("AUTORESEARCH_API_KEY".to_string(), gen_api_token());
                    write_env_file(&env_path, &m);
                }
            }
            // Apply workspace port offset. Delta-based — handles forward and
            // backward shifts and rewrites BASE_URL / FRONTEND_URL alongside
            // the other port-bearing keys.
            apply_port_offset_to_env(&env_path, "copilot", port_offset);
            run_platform_mode_selector(&env_path, &stopping);
            run_env_wizard(&env_path, COPILOT_ENV_VARS, "Copilot");
        } else if no_copilot {
            println!("  {DIM}Copilot skipped.{R}\n");
        }

        if !no_opencti && paths.opencti.is_dir() {
            let env_path = ws_env_path(&ws_env_dir, "opencti");
            let gql_dir = paths.opencti.join("opencti-platform/opencti-graphql");
            init_workspace_env(
                &env_path,
                Some(&gql_dir.join(".env.dev")),
                &[],
                "# OpenCTI graphql dev environment — generated by dev-launcher\n\
# Leave TOKEN and ENCRYPTION_KEY as ChangeMe; the wizard will auto-generate them.\n\
APP__ADMIN__EMAIL=admin@opencti.io\n\
APP__ADMIN__PASSWORD=ChangeMe\n\
APP__ADMIN__TOKEN=ChangeMe\n\
APP__ENCRYPTION_KEY=ChangeMe\n",
            );
            apply_port_offset_to_env(&env_path, "opencti", port_offset);
            run_env_wizard(&env_path, OPENCTI_ENV_VARS, "OpenCTI");
            // Ask once whether to activate the XTM One integration when Copilot is co-launched.
            if !no_copilot && paths.copilot.is_dir() {
                prompt_xtm_one_opencti_integration(&env_path);
            }
        } else if no_opencti {
            println!("  {DIM}OpenCTI skipped.{R}\n");
        }

        if !no_openaev && paths.openaev.is_dir() {
            let env_path = ws_env_path(&ws_env_dir, "openaev");
            let dev_dir = paths.openaev.join("openaev-dev");
            let templates = vec![dev_dir.join(".env.example")];
            init_workspace_env(
                &env_path,
                Some(&dev_dir.join(".env")),
                &templates,
                "# OpenAEV dev environment\n",
            );
            apply_port_offset_to_env(&env_path, "openaev", port_offset);
            if !no_copilot && paths.copilot.is_dir() {
                prompt_xtm_one_openaev_integration(&env_path);
            }
        } else if no_openaev {
            println!("  {DIM}OpenAEV skipped.{R}\n");
        }

        if !no_connector && paths.connector.is_dir() {
            let env_path = ws_env_path(&ws_env_dir, "connector");
            let opencti_url_default = format!("http://localhost:{opencti_gql_port}");
            let connector_hardcoded = format!(
                "# Connector dev environment — fill in before running\n\
OPENCTI_URL={opencti_url_default}\n\
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
CONNECTOR_LICENCE_KEY_PEM=\n"
            );
            init_workspace_env(
                &env_path,
                Some(&paths.connector.join(".env.dev")),
                &[],
                &connector_hardcoded,
            );
            apply_port_offset_to_env(&env_path, "connector", port_offset);
        }

        if !no_opencti && !no_connector {
            let opencti_env = ws_env_path(&ws_env_dir, "opencti");
            let connector_env = ws_env_path(&ws_env_dir, "connector");
            if opencti_env.exists() && connector_env.exists() {
                if let Some(token) = parse_env_file(&opencti_env)
                    .get("APP__ADMIN__TOKEN")
                    .cloned()
                {
                    if !token.is_empty() && token != "ChangeMe" {
                        let mut cenv = parse_env_file(&connector_env);
                        cenv.insert("OPENCTI_TOKEN".to_string(), token);
                        write_env_file(&connector_env, &cenv);
                        println!("  {GRN}✓{R}  OPENCTI_TOKEN synced from OpenCTI admin token");
                    }
                }
            }
        }

        if !no_connector && paths.connector.is_dir() {
            run_env_wizard(
                &ws_env_path(&ws_env_dir, "connector"),
                CONNECTOR_ENV_VARS,
                "ImportDocumentAI connector",
            );
        } else if no_connector {
            println!("  {DIM}Connector skipped.{R}\n");
        }

        // ── License PEM injection ─────────────────────────────────────────────────
        {
            let search_dirs = pem_search_dirs(workspace_root);
            let mut enabled: std::collections::HashMap<&'static str, std::path::PathBuf> =
                std::collections::HashMap::new();
            if !no_copilot {
                enabled.insert("Copilot", ws_env_path(&ws_env_dir, "copilot"));
            }
            if !no_opencti {
                enabled.insert("OpenCTI", ws_env_path(&ws_env_dir, "opencti"));
            }
            if !no_openaev {
                enabled.insert("OpenAEV", ws_env_path(&ws_env_dir, "openaev"));
            }
            let mut pem_candidates = find_pem_candidates(&search_dirs, &enabled);
            run_pem_selector(&mut pem_candidates, &stopping);
            inject_selected_pems(&pem_candidates, &ws_env_dir);
        }

        if !no_copilot {
            deploy_workspace_env(
                &ws_env_path(&ws_env_dir, "copilot"),
                &paths.copilot.join(".env"),
            );
        }
        if !no_opencti {
            deploy_workspace_env(
                &ws_env_path(&ws_env_dir, "opencti"),
                &paths
                    .opencti
                    .join("opencti-platform/opencti-graphql/.env.dev"),
            );
        }
        if !no_openaev {
            deploy_workspace_env(
                &ws_env_path(&ws_env_dir, "openaev"),
                &paths.openaev.join("openaev-dev/.env"),
            );
        }
        if !no_connector {
            deploy_workspace_env(
                &ws_env_path(&ws_env_dir, "connector"),
                &paths.connector.join(".env.dev"),
            );
        }

        // ── Step 2 / 2 — Starting services ───────────────────────────────────────
        println!("  {DIM}{sep}{R}");
        println!("  {BOLD}Step 2 / 2  —  Starting services{R}");
        println!("  {DIM}{sep}{R}\n");

        print!("  Checking Corepack… ");
        let _ = io::stdout().flush();
        ensure_corepack();

        if clean_start {
            clean_docker_for_workspace(
                &slug,
                &paths,
                no_copilot,
                no_opencti,
                no_openaev,
                no_grafana,
                no_langfuse,
            );
        }

        print!("  Checking Docker… ");
        let _ = io::stdout().flush();
        let docker_ok = docker_available();
        if docker_ok {
            println!("{GRN}running{R}");
        } else {
            println!("{RED}not reachable{R}");
            println!(
                "  {YLW}Start Docker Desktop (or the Docker daemon) before launching the stack.{R}"
            );
            println!("  {DIM}Services that need infrastructure containers will start in Degraded state.{R}\n");
        }

        let maven_ok = if !no_openaev && paths.openaev.is_dir() {
            let mvn = maven_cmd(&paths.openaev);
            print!("  Checking Maven… ");
            let _ = io::stdout().flush();
            let available = if mvn == "mvn" {
                Command::new("mvn")
                    .arg("--version")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            } else {
                PathBuf::from(&mvn).exists()
            };
            if available {
                println!("{GRN}ok{R}");
            } else {
                println!("{RED}not found{R}");
                println!("  {YLW}Install Maven before launching the stack:{R}");
                println!("    brew install maven");
                println!("  {DIM}openaev-backend will start in Degraded state.{R}\n");
            }
            available
        } else {
            true
        };

        let (openaev_java_ok, openaev_backend_env, openaev_java_reason) =
            if !no_openaev && paths.openaev.is_dir() {
                print!("  Checking Java for OpenAEV… ");
                let _ = io::stdout().flush();
                match openaev_java_env() {
                    Ok(mut env) => {
                        if env.is_empty() {
                            println!("{GRN}ok{R}");
                        } else {
                            println!("{GRN}ok{R} {DIM}(using JDK 21 override){R}");
                        }
                        env.extend(openaev_backend_env(
                            &ws_env_dir,
                            &paths.openaev,
                            port_offset,
                            openaev_be_port,
                        ));
                        (true, env, None)
                    }
                    Err(reason) => {
                        println!("{RED}incompatible{R}");
                        println!("  {YLW}{reason}{R}");
                        println!("  {DIM}openaev-backend will start in Degraded state.{R}\n");
                        (false, HashMap::new(), Some(reason))
                    }
                }
            } else {
                (true, HashMap::new(), None)
            };

        let copilot_env_path = ws_env_path(&ws_env_dir, "copilot");
        let copilot_backend_port = read_env_url_port(&copilot_env_path, "BASE_URL", 8100);
        let copilot_frontend_port = read_env_url_port(&copilot_env_path, "FRONTEND_URL", 3100);
        let copilot_infinity_port = read_env_url_port(&copilot_env_path, "INFINITY_URL", 7997);

        let copilot_manifest = if !no_copilot && paths.copilot.is_dir() {
            let mut m = load_repo_manifest(&paths.copilot, "Copilot");
            patch_manifest_ports(&mut m, copilot_backend_port, copilot_frontend_port);
            Some(m)
        } else {
            None
        };
        let opencti_manifest = if !no_opencti && paths.opencti.is_dir() {
            Some(load_repo_manifest(&paths.opencti, "OpenCTI"))
        } else {
            None
        };
        let openaev_manifest = if !no_openaev && paths.openaev.is_dir() {
            Some(load_repo_manifest(&paths.openaev, "OpenAEV"))
        } else {
            None
        };
        let _connector_manifest = if !no_connector && paths.connector.is_dir() {
            Some(load_repo_manifest(&paths.connector, "Connector"))
        } else {
            None
        };

        let mut copilot_docker_ok = true;
        let mut opencti_docker_ok = true;
        let mut openaev_docker_ok = true;
        let mut docker_projects: Vec<DockerProject> = Vec::new();

        if docker_ok {
            if !no_copilot && paths.copilot.is_dir() {
                let (dc, project) = if let Some(ref m) = copilot_manifest {
                    let f = paths.copilot.join(
                        m.docker
                            .compose_dev
                            .as_deref()
                            .unwrap_or("docker-compose.dev.yml"),
                    );
                    (f, resolve_docker_project(&paths.copilot, m, &slug))
                } else {
                    let f = paths.copilot.join("docker-compose.dev.yml");
                    (f, ws_docker_project("copilot-dev", &slug))
                };
                if dc.exists() {
                    let ov = write_compose_override(&dc, &slug, port_offset);
                    let ov_str = ov
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let extra: Vec<&str> = if ov.is_some() {
                        vec!["-f", &ov_str]
                    } else {
                        vec![]
                    };
                    copilot_docker_ok =
                        docker_compose_up("Copilot", &project, &dc, &paths.copilot, &extra);
                    docker_projects.push(DockerProject {
                        label: "Copilot".into(),
                        project,
                        compose_file: dc,
                        work_dir: paths.copilot.clone(),
                        override_file: ov,
                    });
                }
            }
            if !no_opencti && paths.opencti.is_dir() {
                let dc = paths
                    .opencti
                    .join("opencti-platform/opencti-dev/docker-compose.yml");
                if dc.exists() {
                    let base = opencti_manifest
                        .as_ref()
                        .and_then(|m| m.docker.project.clone())
                        .unwrap_or_else(|| "opencti-dev".to_string());
                    let project = ws_docker_project(&base, &slug);
                    let ov = write_compose_override(&dc, &slug, port_offset);
                    let ov_str = ov
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let extra: Vec<&str> = if ov.is_some() {
                        vec!["-f", &ov_str]
                    } else {
                        vec![]
                    };
                    opencti_docker_ok =
                        docker_compose_up("OpenCTI", &project, &dc, &paths.opencti, &extra);
                    docker_projects.push(DockerProject {
                        label: "OpenCTI".into(),
                        project,
                        compose_file: dc,
                        work_dir: paths.opencti.clone(),
                        override_file: ov,
                    });
                }
            }
            if !no_openaev && paths.openaev.is_dir() {
                let dev_dir = paths.openaev.join("openaev-dev");
                let dc = dev_dir.join("docker-compose.yml");
                if dc.exists() {
                    let env_file = dev_dir.join(".env").to_string_lossy().into_owned();
                    let base = openaev_manifest
                        .as_ref()
                        .and_then(|m| m.docker.project.clone())
                        .unwrap_or_else(|| "openaev-dev".to_string());
                    let project = ws_docker_project(&base, &slug);
                    let ov = write_compose_override(&dc, &slug, port_offset);
                    let ov_str = ov
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let mut extra: Vec<&str> = vec!["--env-file", &env_file];
                    if ov.is_some() {
                        extra.extend_from_slice(&["-f", &ov_str]);
                    }
                    openaev_docker_ok =
                        docker_compose_up("OpenAEV", &project, &dc, &dev_dir, &extra);
                    docker_projects.push(DockerProject {
                        label: "OpenAEV".into(),
                        project,
                        compose_file: dc,
                        work_dir: dev_dir,
                        override_file: ov,
                    });
                }
            }
            if !no_grafana {
                bootstrap_infra_dir(&paths.grafana, "grafana");
                let dc = paths.grafana.join("docker-compose.dev.yml");
                if dc.exists() {
                    let project = ws_docker_project("grafana-dev", &slug);
                    let ov = write_compose_override(&dc, &slug, port_offset);
                    let ov_str = ov
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let env_file = paths.grafana.join(".env");
                    let env_file_str = env_file.to_string_lossy().into_owned();
                    let mut extra: Vec<&str> = if env_file.exists() {
                        vec!["--env-file", &env_file_str]
                    } else {
                        vec![]
                    };
                    if ov.is_some() {
                        extra.extend_from_slice(&["-f", &ov_str]);
                    }
                    docker_compose_up("Grafana", &project, &dc, &paths.grafana, &extra);
                    docker_projects.push(DockerProject {
                        label: "Grafana".into(),
                        project,
                        compose_file: dc,
                        work_dir: paths.grafana.clone(),
                        override_file: ov,
                    });
                }
            }
            if !no_langfuse {
                bootstrap_infra_dir(&paths.langfuse, "langfuse");
                let dc = paths.langfuse.join("docker-compose.dev.yml");
                if dc.exists() {
                    let project = ws_docker_project("langfuse-dev", &slug);
                    let ov = write_compose_override(&dc, &slug, port_offset);
                    let ov_str = ov
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let env_file = paths.langfuse.join(".env");
                    let env_file_str = env_file.to_string_lossy().into_owned();
                    let mut extra: Vec<&str> = if env_file.exists() {
                        vec!["--env-file", &env_file_str]
                    } else {
                        vec![]
                    };
                    if ov.is_some() {
                        extra.extend_from_slice(&["-f", &ov_str]);
                    }
                    docker_compose_up("Langfuse", &project, &dc, &paths.langfuse, &extra);
                    docker_projects.push(DockerProject {
                        label: "Langfuse".into(),
                        project,
                        compose_file: dc,
                        work_dir: paths.langfuse.clone(),
                        override_file: ov,
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
                    $svc.spawn_cmd = Some(SpawnCmd {
                        prog: $prog.to_string(),
                        args: $argv.iter().map(|s| s.to_string()).collect(),
                        dir: $dir.to_path_buf(),
                        env: $env.clone(),
                        requires_docker: false,
                    });
                    let port_conflict: Option<String> = $svc
                        .url
                        .as_deref()
                        .and_then(extract_url_port)
                        .and_then(port_in_use);
                    if let Some(conflict) = port_conflict {
                        $svc.health = Health::Degraded(conflict);
                        svcs.push($svc);
                    } else {
                        let idx = svcs.len();
                        match spawn_svc($prog, $argv, $dir, $env, &$svc.log_path) {
                            Ok((child, pgid)) => {
                                record_pid(&slug, child.id());
                                $svc.pid = Some(child.id());
                                $svc.started_at = Some(Instant::now());
                                $svc.health = if $svc.url.is_some() {
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
                    }
                }};
            }

            // Copilot
            if !no_copilot && paths.copilot.is_dir() {
                let uses_manifest = copilot_manifest
                    .as_ref()
                    .is_some_and(|m| !m.services.is_empty());
                if uses_manifest {
                    let m = copilot_manifest.as_ref().unwrap();
                    let _bootstrap_ok = run_manifest_bootstrap(&paths.copilot, m);
                    let backend_env = copilot_backend_env(&paths.copilot);
                    for def in &m.services {
                        let log_path = logs_dir.join(
                            def.log_name
                                .clone()
                                .unwrap_or_else(|| format!("copilot-{}.log", def.name)),
                        );
                        let (url, health_path) = split_health_url_parts(def.health.as_deref());
                        let mut svc = services::Svc::new(
                            format!("copilot-{}", def.name),
                            url,
                            health_path,
                            def.timeout_secs,
                            log_path,
                        );
                        svc.requires = def.requires.clone();
                        if def.requires_docker && !copilot_docker_ok {
                            svc.health = Health::Degraded(
                                "Docker deps not running — start Docker first".into(),
                            );
                            svcs.push(svc);
                            continue;
                        }
                        let work_dir = if def.cwd.is_empty() {
                            paths.copilot.clone()
                        } else {
                            paths.copilot.join(&def.cwd)
                        };
                        if def.args.is_empty() || !work_dir.is_dir() {
                            svcs.push(svc);
                            continue;
                        }
                        let prog = if def.args[0].starts_with('.') {
                            work_dir.join(&def.args[0]).to_string_lossy().into_owned()
                        } else {
                            def.args[0].clone()
                        };
                        let rest: Vec<&str> = def.args[1..].iter().map(|s| s.as_str()).collect();
                        let empty_env: HashMap<String, String> = HashMap::new();
                        let mut frontend_env: HashMap<String, String> = HashMap::new();
                        frontend_env.insert("PORT".to_string(), copilot_frontend_port.to_string());
                        frontend_env.insert(
                            "VITE_API_URL".to_string(),
                            format!("http://localhost:{copilot_backend_port}"),
                        );
                        let env = if def.cwd == "backend" {
                            &backend_env
                        } else if def.cwd == "frontend" {
                            &frontend_env
                        } else {
                            &empty_env
                        };
                        if (prog.starts_with('/') || prog.starts_with("./") || prog.contains("/."))
                            && !PathBuf::from(&prog).exists()
                        {
                            svc.spawn_cmd = Some(SpawnCmd {
                                prog: prog.clone(),
                                args: rest.iter().map(|s| s.to_string()).collect(),
                                dir: work_dir.clone(),
                                env: env.clone(),
                                requires_docker: def.requires_docker,
                            });
                            svc.health = Health::Degraded(format!(
                                "{} not found — run ./dev.sh once",
                                &def.args[0]
                            ));
                            svcs.push(svc);
                            continue;
                        }
                        if !def.requires.is_empty() {
                            let unmet: Vec<&str> = def
                                .requires
                                .iter()
                                .filter(|r| !svcs.iter().any(|s| &s.name == *r && s.is_healthy()))
                                .map(|s| s.as_str())
                                .collect();
                            if !unmet.is_empty() {
                                svc.spawn_cmd = Some(SpawnCmd {
                                    prog: prog.clone(),
                                    args: rest.iter().map(|s| s.to_string()).collect(),
                                    dir: work_dir.clone(),
                                    env: env.clone(),
                                    requires_docker: def.requires_docker,
                                });
                                svc.health =
                                    Health::Degraded(format!("Waiting for {}…", unmet.join(", ")));
                                svcs.push(svc);
                                continue;
                            }
                        }
                        try_spawn!(svc, &prog, &rest, &work_dir, env);
                    }
                } else {
                    let backend_port_str = copilot_backend_port.to_string();
                    let backend_url = format!("http://localhost:{copilot_backend_port}");
                    let frontend_url = format!("http://localhost:{copilot_frontend_port}");
                    let backend_dir = paths.copilot.join("backend");
                    ensure_copilot_backend_venv(&backend_dir);
                    ensure_copilot_backend_dev_deps(&backend_dir);
                    let python = backend_dir.join(".venv/bin/python");
                    let backend_env = copilot_backend_env(&paths.copilot);
                    let mut svc = services::Svc::new(
                        "copilot-backend",
                        Some(&backend_url),
                        "/api/health",
                        120,
                        logs_dir.join("copilot-backend.log"),
                    );
                    if !copilot_docker_ok {
                        svc.health =
                            Health::Degraded("Docker deps not running — start Docker first".into());
                        svcs.push(svc);
                    } else if python.exists() {
                        try_spawn!(
                            svc,
                            python.to_str().unwrap(),
                            &[
                                "-m",
                                "uvicorn",
                                "app.main:application",
                                "--reload",
                                "--host",
                                "0.0.0.0",
                                "--port",
                                &backend_port_str,
                                "--timeout-graceful-shutdown",
                                "3"
                            ],
                            &backend_dir,
                            &backend_env
                        );
                    } else {
                        svc.spawn_cmd = Some(SpawnCmd {
                            prog: python.to_str().unwrap().to_string(),
                            args: [
                                "-m",
                                "uvicorn",
                                "app.main:application",
                                "--reload",
                                "--host",
                                "0.0.0.0",
                                "--port",
                                backend_port_str.as_str(),
                                "--timeout-graceful-shutdown",
                                "3",
                            ]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                            dir: backend_dir.clone(),
                            env: backend_env.clone(),
                            requires_docker: false,
                        });
                        svc.health =
                            Health::Degraded("venv missing — run ./dev.sh once to set up".into());
                        svcs.push(svc);
                    }
                    let mut svc = services::Svc::new(
                        "copilot-worker",
                        None::<String>,
                        "",
                        10,
                        logs_dir.join("copilot-worker.log"),
                    );
                    if !copilot_docker_ok {
                        svc.health = Health::Degraded("Docker deps not running".into());
                        svcs.push(svc);
                    } else if python.exists() {
                        try_spawn!(
                            svc,
                            python.to_str().unwrap(),
                            &["-m", "saq", "app.worker.settings"],
                            &backend_dir,
                            &backend_env
                        );
                    } else {
                        svc.spawn_cmd = Some(SpawnCmd {
                            prog: python.to_str().unwrap().to_string(),
                            args: ["-m", "saq", "app.worker.settings"]
                                .iter()
                                .map(|s| s.to_string())
                                .collect(),
                            dir: backend_dir.clone(),
                            env: backend_env.clone(),
                            requires_docker: false,
                        });
                        svc.health = Health::Degraded("venv missing".into());
                        svcs.push(svc);
                    }
                    let fe_dir = paths.copilot.join("frontend");
                    let fe_deps_ok = ensure_copilot_fe_deps(&fe_dir);
                    let mut svc = services::Svc::new(
                        "copilot-frontend",
                        Some(&frontend_url),
                        "",
                        90,
                        logs_dir.join("copilot-frontend.log"),
                    );
                    if fe_dir.is_dir() {
                        if fe_deps_ok {
                            let fe_port_str = copilot_frontend_port.to_string();
                            let mut fe_env = HashMap::new();
                            fe_env.insert("PORT".to_string(), fe_port_str);
                            fe_env.insert(
                                "VITE_API_URL".to_string(),
                                format!("http://localhost:{copilot_backend_port}"),
                            );
                            try_spawn!(svc, "yarn", &["dev"], &fe_dir, &fe_env);
                        } else {
                            svc.health = Health::Degraded(
                                "yarn install failed — run: cd frontend && yarn install".into(),
                            );
                            svcs.push(svc);
                        }
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
                    let env_file = gql_dir.join(".env.dev");
                    if env_file.exists() {
                        for (k, v) in parse_env_file(&env_file) {
                            gql_env.insert(k, v);
                        }
                    }
                }
                let opencti_password_ok = gql_env
                    .get("APP__ADMIN__PASSWORD")
                    .map(|p| !p.is_empty() && p != "ChangeMe")
                    .unwrap_or(false);
                let mut svc = services::Svc::new(
                    "opencti-graphql",
                    Some(format!("http://localhost:{opencti_gql_port}")),
                    "",
                    300,
                    logs_dir.join("opencti-graphql.log"),
                );
                if !opencti_docker_ok {
                    svc.health =
                        Health::Degraded("Docker deps not running — start Docker first".into());
                    svcs.push(svc);
                } else if !opencti_password_ok {
                    svc.health = Health::Degraded(
                    "APP__ADMIN__PASSWORD not set — run dev-launcher again to fill in credentials"
                        .into(),
                );
                    svcs.push(svc);
                } else if gql_dir.is_dir() {
                    if !no_copilot && paths.copilot.is_dir() {
                        svc.requires = vec!["copilot-backend".to_string()];
                        svc.spawn_cmd = Some(SpawnCmd {
                            prog: "yarn".to_string(),
                            args: vec!["start".to_string()],
                            dir: gql_dir.clone(),
                            env: gql_env.clone(),
                            requires_docker: true,
                        });
                        svc.health = Health::Degraded("Waiting for copilot-backend…".into());
                        svcs.push(svc);
                    } else {
                        wait_for_opensearch(es_port, 120);
                        wipe_opencti_es_indices_if_stale(es_port);
                        try_spawn!(svc, "yarn", &["start"], &gql_dir, &gql_env);
                    }
                }

                if !no_opencti_front {
                    let front_dir = paths.opencti.join("opencti-platform/opencti-front");
                    let fe_deps_ok = ensure_opencti_fe_deps(&front_dir);
                    let mut front_env = HashMap::new();
                    front_env.insert(
                        "BACK_END_URL".to_string(),
                        format!("http://localhost:{opencti_gql_port}"),
                    );
                    let mut svc = services::Svc::new(
                        "opencti-frontend",
                        Some(format!(
                            "http://localhost:{}",
                            3000u16.saturating_add(port_offset)
                        )),
                        "",
                        120,
                        logs_dir.join("opencti-frontend.log"),
                    );
                    if front_dir.is_dir() {
                        if !fe_deps_ok {
                            svc.health = Health::Degraded(
                            "yarn install failed — run: cd opencti-platform/opencti-front && yarn install".into(),
                        );
                            svcs.push(svc);
                        } else if !no_copilot && paths.copilot.is_dir() {
                            svc.requires = vec!["copilot-backend".to_string()];
                            svc.spawn_cmd = Some(SpawnCmd {
                                prog: "yarn".to_string(),
                                args: vec!["dev".to_string()],
                                dir: front_dir.clone(),
                                env: front_env.clone(),
                                requires_docker: false,
                            });
                            svc.health = Health::Degraded("Waiting for copilot-backend…".into());
                            svcs.push(svc);
                        } else {
                            try_spawn!(svc, "yarn", &["dev"], &front_dir, &front_env);
                        }
                    }
                }
            }

            // OpenAEV
            if !no_openaev && paths.openaev.is_dir() {
                let mvn = maven_cmd(&paths.openaev);
                let api_dir = paths.openaev.join("openaev-api");

                let mut svc = services::Svc::new(
                    "openaev-backend",
                    Some(format!("http://localhost:{openaev_be_port}")),
                    "/api/health?health_access_key=ChangeMe",
                    180,
                    logs_dir.join("openaev-backend.log"),
                );
                if !openaev_docker_ok {
                    svc.health =
                        Health::Degraded("Docker deps not running — start Docker first".into());
                    svcs.push(svc);
                } else if !openaev_java_ok {
                    svc.health = Health::Degraded(
                        openaev_java_reason
                            .clone()
                            .unwrap_or_else(|| "OpenAEV requires JDK 21".to_string()),
                    );
                    svcs.push(svc);
                } else if !maven_ok {
                    svc.health = Health::Degraded(
                        "Maven not found — install with 'brew install maven'".into(),
                    );
                    svcs.push(svc);
                } else if api_dir.is_dir() {
                    if !no_copilot && paths.copilot.is_dir() {
                        svc.requires = vec!["copilot-backend".to_string()];
                        svc.spawn_cmd = Some(SpawnCmd {
                            prog: mvn.clone(),
                            args: [
                                "spring-boot:run",
                                "-Pdev",
                                "-pl",
                                "openaev-api",
                                "-am",
                                "-Dspring-boot.run.profiles=dev",
                            ]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                            dir: paths.openaev.clone(),
                            env: openaev_backend_env.clone(),
                            requires_docker: true,
                        });
                        svc.health = Health::Degraded("Waiting for copilot-backend…".into());
                        svcs.push(svc);
                    } else {
                        try_spawn!(
                            svc,
                            &mvn,
                            &[
                                "spring-boot:run",
                                "-Pdev",
                                "-pl",
                                "openaev-api",
                                "-am",
                                "-Dspring-boot.run.profiles=dev"
                            ],
                            &paths.openaev,
                            &openaev_backend_env
                        );
                    }
                } else {
                    svc.health = Health::Degraded("openaev-api/ not found".into());
                    svcs.push(svc);
                }

                if !no_openaev_front {
                    let fe_dir = paths.openaev.join("openaev-front");
                    let fe_deps_ok = ensure_openaev_fe_deps(&fe_dir);
                    let mut svc = services::Svc::new(
                        "openaev-frontend",
                        Some("http://localhost:3001"),
                        "",
                        90,
                        logs_dir.join("openaev-frontend.log"),
                    );
                    if fe_dir.is_dir() {
                        if !fe_deps_ok {
                            svc.health = Health::Degraded(
                                "yarn install failed — run: cd openaev-front && yarn install"
                                    .into(),
                            );
                            svcs.push(svc);
                        } else if !no_copilot && paths.copilot.is_dir() {
                            svc.requires = vec!["copilot-backend".to_string()];
                            svc.spawn_cmd = Some(SpawnCmd {
                                prog: "yarn".to_string(),
                                args: vec!["start".to_string()],
                                dir: fe_dir.clone(),
                                env: HashMap::new(),
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

            // Grafana (Docker-only — no host process to spawn)
            if !no_grafana {
                let mut svc = services::Svc::new(
                    "grafana",
                    Some("http://localhost:3200"),
                    "/api/health",
                    60,
                    logs_dir.join("grafana.log"),
                );
                svc.health = Health::Launching;
                svcs.push(svc);
            }

            // Langfuse (Docker-only — no host process to spawn)
            if !no_langfuse {
                let mut svc = services::Svc::new(
                    "langfuse",
                    Some("http://localhost:3201"),
                    "/api/public/health",
                    120,
                    logs_dir.join("langfuse.log"),
                );
                svc.health = Health::Launching;
                svcs.push(svc);
            }

            // Infinity embedding server — optional, user-selected model via workspace config.
            // Runs in its own isolated venv (paths.infinity) to avoid conflicts with
            // the Copilot backend's dependencies (click versions, optimum, etc.).
            // --url-prefix /v1 mounts all routes under /v1/ for OpenAI-compatible access.
            // --no-bettertransformer avoids a NameError in infinity-emb 0.0.76 when
            // optimum is not installed (BetterTransformerManager missing availability guard).
            // A transparent logging proxy sits at copilot_infinity_port (the public port Copilot
            // uses) and forwards to the actual Infinity server on copilot_infinity_port+10000.
            // The proxy logs the 'model' field of every POST /v1/embeddings request.
            if !no_infinity {
                let infinity_dir = &paths.infinity;
                let internal_port = copilot_infinity_port.saturating_add(10000);
                let internal_url = format!("http://localhost:{internal_port}");
                let proxy_url = format!("http://localhost:{copilot_infinity_port}");
                let infinity_bin = infinity_dir.join(".venv/bin/infinity_emb");
                let python3_bin = infinity_dir.join(".venv/bin/python3");
                let proxy_script = infinity_dir.join("proxy.py");

                if !ensure_infinity_emb_isolated(infinity_dir) {
                    let mut svc = services::Svc::new(
                        "infinity-emb",
                        Some(&proxy_url),
                        "/health",
                        120,
                        logs_dir.join("infinity-emb.log"),
                    );
                    svc.health = Health::Degraded(format!(
                        "python3 not found or venv creation failed ({})",
                        infinity_dir.display()
                    ));
                    svcs.push(svc);
                } else {
                    let _ = fs::write(&proxy_script, include_str!("infra/infinity/proxy.py"));

                    // Actual embedding server — health-probed on its internal port so the
                    // requires chain below fires only once the model is truly loaded.
                    let mut emb_svc = services::Svc::new(
                        "infinity-emb",
                        Some(&internal_url),
                        "/health",
                        180,
                        logs_dir.join("infinity-emb.log"),
                    );
                    let internal_port_str = internal_port.to_string();
                    let bin_str = infinity_bin.to_string_lossy().into_owned();
                    try_spawn!(
                        emb_svc,
                        &bin_str,
                        &[
                            "v2",
                            "--model-id",
                            &infinity_model,
                            "--port",
                            &internal_port_str,
                            "--no-bettertransformer",
                            "--url-prefix",
                            "/v1"
                        ],
                        infinity_dir,
                        &HashMap::new()
                    );

                    // Logging proxy — deferred until infinity-emb passes its health probe.
                    let mut proxy_svc = services::Svc::new(
                        "infinity-proxy",
                        Some(&proxy_url),
                        "/health",
                        30,
                        logs_dir.join("infinity-proxy.log"),
                    );
                    proxy_svc.requires = vec!["infinity-emb".to_string()];
                    let python3_str = python3_bin.to_string_lossy().into_owned();
                    let proxy_script_str = proxy_script.to_string_lossy().into_owned();
                    let proxy_port_str = copilot_infinity_port.to_string();
                    proxy_svc.spawn_cmd = Some(SpawnCmd {
                        prog: python3_str,
                        args: vec![proxy_script_str, internal_url.clone(), proxy_port_str],
                        dir: infinity_dir.to_path_buf(),
                        env: HashMap::new(),
                        requires_docker: false,
                    });
                    proxy_svc.health = Health::Degraded("Waiting for infinity-emb…".into());
                    svcs.push(proxy_svc);
                }
            }

            // AutoResearch runner — manages a local autoresearch repo clone.
            // Uses uv (not pip) for ML deps; runner.py itself uses a minimal service venv.
            // On macOS the MPS-compatible fork is used; on Linux the main NVIDIA repo.
            // The API key is stored in the copilot env so XTM One can read it directly.
            if !no_autoresearch {
                let ar_dir = &paths.autoresearch;
                let ar_port = 8400u16.saturating_add(port_offset);
                let ar_url = format!("http://localhost:{ar_port}");

                // Choose the right autoresearch fork based on the current platform.
                let repo_url = if cfg!(target_os = "macos") {
                    "https://github.com/miolini/autoresearch-macos"
                } else {
                    "https://github.com/karpathy/autoresearch"
                };

                if !ensure_autoresearch_isolated(ar_dir, repo_url) {
                    let mut svc = services::Svc::new(
                        "autoresearch",
                        Some(&ar_url),
                        "/health",
                        120,
                        logs_dir.join("autoresearch.log"),
                    );
                    svc.health = Health::Degraded(
                        "setup failed — check that git, uv, and python3 are installed".into(),
                    );
                    svcs.push(svc);
                } else {
                    let runner_script = ar_dir.join("runner.py");
                    let _ = fs::write(&runner_script, include_str!("infra/autoresearch/runner.py"));

                    // Seed train.py once (only if not yet patched by the agent).
                    let train_seed = ar_dir.join("repo/train.py");
                    let patched_marker = ar_dir.join("repo/.cti_patched");
                    if train_seed.exists() && !patched_marker.exists() {
                        let _ = fs::write(&train_seed, include_str!("infra/autoresearch/train.py"));
                        let _ = fs::write(&patched_marker, "");
                    }

                    let api_key = {
                        let m = parse_env_file(&ws_env_path(&ws_env_dir, "copilot"));
                        m.get("AUTORESEARCH_API_KEY")
                            .cloned()
                            .unwrap_or_else(gen_api_token)
                    };

                    let python3_bin = ar_dir.join(".venv/bin/python3");
                    let python3_str = python3_bin.to_string_lossy().into_owned();
                    let ar_port_str = ar_port.to_string();

                    let mut env = HashMap::new();
                    env.insert("AUTORESEARCH_API_KEY".to_string(), api_key);
                    env.insert("AUTORESEARCH_PORT".to_string(), ar_port.to_string());
                    env.insert(
                        "AUTORESEARCH_DIR".to_string(),
                        ar_dir.to_string_lossy().into_owned(),
                    );
                    env.insert("AUTORESEARCH_REPO_URL".to_string(), repo_url.to_string());

                    let mut svc = services::Svc::new(
                        "autoresearch",
                        Some(&ar_url),
                        "/health",
                        120,
                        logs_dir.join("autoresearch.log"),
                    );
                    try_spawn!(
                        svc,
                        &python3_str,
                        &[
                            "-m",
                            "uvicorn",
                            "runner:app",
                            "--host",
                            "0.0.0.0",
                            "--port",
                            &ar_port_str
                        ],
                        ar_dir,
                        &env
                    );
                }
            }

            // Connector
            if !no_connector && paths.connector.is_dir() {
                let env_path = ensure_connector_env(&paths.connector);
                let venv = ensure_connector_venv(&paths.connector);
                let python = venv.join("bin/python");
                let src_dir = paths.connector.join("src");
                let env = parse_env_file(&env_path);

                let mut svc = services::Svc::new(
                    "connector",
                    None::<String>,
                    "",
                    30,
                    logs_dir.join("connector.log"),
                );
                svc.requires = vec!["opencti-graphql".to_string()];
                if let Some(reason) = validate_connector_env(&env) {
                    svc.health = Health::Degraded(reason);
                    svcs.push(svc);
                } else if src_dir.is_dir() && python.exists() {
                    let opencti_ready = svcs
                        .iter()
                        .any(|s| s.name == "opencti-graphql" && s.is_healthy());
                    if !opencti_ready {
                        let python_str = python.to_str().unwrap().to_string();
                        svc.spawn_cmd = Some(SpawnCmd {
                            prog: python_str.clone(),
                            args: vec!["main.py".to_string()],
                            dir: src_dir.clone(),
                            env: env.clone(),
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
            let state = Arc::clone(&state);
            let stopping = Arc::clone(&stopping);
            thread::spawn(move || loop {
                if stopping.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_secs(1));

                let to_probe: Vec<(usize, String, bool, u64)> = {
                    let svcs = state.lock().unwrap();
                    svcs.iter()
                        .enumerate()
                        .filter(|(_, s)| !s.health.is_done())
                        .filter_map(|(i, s)| {
                            s.health_url().map(|url| {
                                let timed_out = s
                                    .started_at
                                    .map(|t| t.elapsed() > s.startup_timeout)
                                    .unwrap_or(false);
                                (i, url, timed_out, s.startup_timeout.as_secs())
                            })
                        })
                        .collect()
                };

                for (i, url, timed_out, timeout_secs) in to_probe {
                    let ok = !timed_out && probe(&url);

                    let mut svcs = state.lock().unwrap();
                    if svcs[i].health.is_done() {
                        continue;
                    }
                    let prev = svcs[i].health.label_plain();
                    let svc_name = svcs[i].name.clone();
                    let new_health = if ok {
                        Health::Up
                    } else if timed_out {
                        Health::Degraded(format!("no response after {timeout_secs}s"))
                    } else {
                        match &svcs[i].health {
                            Health::Probing(n) => Health::Probing(n + 1),
                            _ => Health::Probing(1),
                        }
                    };
                    if new_health.is_done() {
                        llog!("[HEALTH] {svc_name}: {prev} → {}", new_health.label_plain());
                    }
                    svcs[i].health = new_health;
                }
            });
        }

        // ── TUI setup ─────────────────────────────────────────────────────────────
        let mut raw_mode: Option<TuiGuard> = TuiGuard::enter();
        let has_tui = raw_mode.is_some();
        let mut mode = Mode::Overview { cursor: 0 };
        let mut creds: Vec<CredEntry> = Vec::new();
        let mut show_paths = false;
        // Set to true when the user presses M from Overview to leave the TUI without
        // stopping the stack (detach).  We pause the process via SIGSTOP.
        let mut want_detach = false;
        // Set to true when the user explicitly presses q/Esc from Overview so that
        // we return to the workspace selector instead of exiting the process.
        let mut want_restart = false;

        let (tx, rx) = mpsc::sync_channel::<InputEvent>(32);
        let input_paused = Arc::new(AtomicBool::new(false));
        if has_tui {
            spawn_input_thread(tx, Arc::clone(&stopping), Arc::clone(&input_paused));
        }

        // ── Main loop ─────────────────────────────────────────────────────────────
        print!("\x1b[2J");
        let render_interval = Duration::from_millis(500);
        let mut last_render = Instant::now();
        let mut force_render = true;
        let mut last_rotation_check = Instant::now();
        let mut last_snapshot = Instant::now() - Duration::from_secs(2);
        const LOG_ROTATION_INTERVAL_SECS: u64 = 30;
        const LOG_MAX_BYTES: u64 = 3_000_000;

        loop {
            // ── Detach (M) — leave TUI without stopping the stack ─────────────────
            if want_detach {
                want_detach = false;
                drop(raw_mode.take());
                mark_detached(&slug);
                write_worker_pid(&slug, std::process::id());
                publish_runtime_snapshot_for_state(&slug, &state, port_offset);
                print!("\x1b[H\x1b[2J");
                let _ = io::stdout().flush();
                compress_rotated_logs(&logs_dir);
                // Pause this process — the selector resumes it via SIGCONT when the
                // user reattaches; the workspace CLI also resumes us briefly to push
                // restart commands while we stay detached.
                unsafe {
                    libc::kill(libc::getpid(), libc::SIGSTOP);
                }
                // ── Resumed by SIGCONT ────────────────────────────────────────────
                // If we were woken solely to service a control request, handle it,
                // refresh the snapshot, and SIGSTOP again to stay detached. Loop
                // until we're woken with no pending request — that signals a real
                // reattach by the selector.
                while control::request_path(&slug).exists() {
                    publish_runtime_snapshot_for_state(&slug, &state, port_offset);
                    handle_control_request(
                        &slug,
                        &state,
                        &mut procs,
                        &mut diagnosed,
                        &docker_projects,
                        es_port,
                        &stopping,
                    );
                    publish_runtime_snapshot_for_state(&slug, &state, port_offset);
                    if stopping.load(Ordering::Relaxed) {
                        break;
                    }
                    unsafe {
                        libc::kill(libc::getpid(), libc::SIGSTOP);
                    }
                }
                if stopping.load(Ordering::Relaxed) {
                    // StopWorkspace was queued — fall through to the shutdown path
                    // below without re-entering the TUI.
                    continue;
                }
                let _ = fs::remove_file(detached_marker_path(&slug));
                write_worker_pid(&slug, std::process::id());
                raw_mode = TuiGuard::enter();
                force_render = true;
            }

            if handle_control_request(
                &slug,
                &state,
                &mut procs,
                &mut diagnosed,
                &docker_projects,
                es_port,
                &stopping,
            ) {
                force_render = true;
            }

            // ── Shutdown ─────────────────────────────────────────────────────────
            if stopping.load(Ordering::Relaxed) || SIGHUP_STOP.load(Ordering::Relaxed) {
                drop(raw_mode.take());

                let pairs: Vec<(String, Option<usize>)> = {
                    let svcs = state.lock().unwrap();
                    svcs.iter()
                        .enumerate()
                        .filter(|(_, s)| !matches!(s.health, Health::Pending))
                        .map(|(svc_i, s)| {
                            let proc_j = procs.iter().position(|p| p.idx == svc_i);
                            (s.name.clone(), proc_j)
                        })
                        .collect()
                };

                eprintln!("[dev-launcher] Stopping {} process(es)…", procs.len());
                let kill_deadlines: Vec<Instant> = {
                    let svcs = state.lock().unwrap();
                    procs
                        .iter()
                        .map(|p| {
                            let grace_secs = if svcs.get(p.idx).map(|s| s.name.as_str())
                                == Some("opencti-graphql")
                            {
                                180
                            } else {
                                5
                            };
                            Instant::now() + Duration::from_secs(grace_secs)
                        })
                        .collect()
                };
                for p in &mut procs {
                    eprintln!(
                        "[dev-launcher]   SIGTERM → pgid -{} (svc #{})",
                        p.pgid, p.idx
                    );
                    p.kill();
                }

                let mut term_status: Vec<TermStatus> =
                    procs.iter().map(|_| TermStatus::Terminating).collect();

                let started = Instant::now();
                let mut timed_out = false;

                loop {
                    for (j, p) in procs.iter_mut().enumerate() {
                        if term_status[j] == TermStatus::Terminating {
                            if let Some(code) = p.try_reap() {
                                term_status[j] = TermStatus::Stopped(code);
                            }
                        }
                    }

                    let now = Instant::now();
                    for (j, p) in procs.iter_mut().enumerate() {
                        if term_status[j] == TermStatus::Terminating && now >= kill_deadlines[j] {
                            eprintln!(
                                "[dev-launcher]   SIGKILL → pgid -{} (grace period exceeded)",
                                p.pgid
                            );
                            unsafe {
                                libc::kill(-p.pgid, libc::SIGKILL);
                            }
                            term_status[j] = TermStatus::Killed;
                            timed_out = true;
                        }
                    }

                    render_shutdown(&slug, &pairs, &term_status, started.elapsed(), timed_out);

                    let all_done = term_status.iter().all(|s| *s != TermStatus::Terminating);
                    if all_done {
                        render_shutdown(&slug, &pairs, &term_status, started.elapsed(), timed_out);
                        thread::sleep(Duration::from_millis(600));
                        let _ = fs::remove_file(pid_file_path(&slug));
                        let _ = fs::remove_file(detached_marker_path(&slug));
                        remove_worker_pid(&slug);
                        clear_runtime_files(&slug);
                        eprintln!("[dev-launcher] All processes stopped. PID file removed.");
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

                // Compress rotated log files before leaving this session.
                compress_rotated_logs(&logs_dir);

                // Return to the workspace selector when the user pressed q/Esc,
                // or exit the process for Ctrl+C / SIGHUP.
                if want_restart {
                    print!("\x1b[H\x1b[2J");
                    let _ = io::stdout().flush();
                    // Exit with 0 so the parent selector process sees a clean exit
                    // and can show the workspace picker.  `continue 'session` would
                    // relaunch the entire stack, which is not what the user wants.
                    std::process::exit(0);
                }
                break 'session;
            }

            // ── Crash detection ───────────────────────────────────────────────────
            let mut auto_diagnose: Option<(usize, crate::services::Svc)> = None;
            let mut reaped_indices: Vec<usize> = Vec::new();
            {
                let mut svcs = state.lock().unwrap();
                for (pi, p) in procs.iter_mut().enumerate() {
                    if let Some(code) = p.try_reap() {
                        let already_crashed = matches!(svcs[p.idx].health, Health::Crashed(_));
                        if already_crashed {
                            // Process already recorded as crashed — just remove from procs.
                            reaped_indices.push(pi);
                            continue;
                        }
                        // If opencti-graphql crashes while ES is down, auto-defer so the
                        // auto-spawn loop re-launches it once ES recovers.
                        if svcs[p.idx].name == "opencti-graphql"
                            && svcs[p.idx].spawn_cmd.is_some()
                            && !opensearch_ready(es_port)
                        {
                            svcs[p.idx].health =
                                Health::Degraded("Waiting for OpenSearch/ES…".into());
                        } else {
                            svcs[p.idx].health = Health::Crashed(code);
                            llog!("[CRASH] {}: exit {code}", svcs[p.idx].name);
                        }
                        force_render = true;
                        reaped_indices.push(pi);

                        let is_real_crash = matches!(svcs[p.idx].health, Health::Crashed(_));
                        if !diagnosed.contains(&p.idx) && is_real_crash {
                            diagnosed.insert(p.idx);
                            let log_path = svcs[p.idx].log_path.clone();
                            let svc_idx = p.idx;
                            let tx = diag_tx.clone();
                            let llm = llm_cfg.clone();
                            thread::spawn(move || {
                                thread::sleep(Duration::from_millis(300));
                                if let Some(msg) =
                                    diagnosis::diagnose_crash(&log_path, llm.as_ref())
                                {
                                    let _ = tx.send(DiagEvent::Result { svc_idx, msg });
                                }
                            });

                            if has_tui && matches!(mode, Mode::Overview { .. }) {
                                auto_diagnose = Some((p.idx, svcs[p.idx].clone()));
                            }
                        }
                    }
                }
            }
            // Remove reaped procs in reverse order to preserve indices.
            for pi in reaped_indices.into_iter().rev() {
                procs.remove(pi);
            }
            if let Some((svc_idx, svc)) = auto_diagnose {
                let findings = diagnose_service(&svc, &paths, &ws_env_dir);
                let cursor = findings
                    .iter()
                    .position(|f| f.fix.is_some() && !f.resolved)
                    .unwrap_or(0);
                mode = Mode::Diagnose {
                    svc_idx,
                    findings,
                    cursor,
                };
            }

            // ── Receive diagnosis results ─────────────────────────────────────────
            while let Ok(DiagEvent::Result { svc_idx, msg }) = diag_rx.try_recv() {
                let mut svcs = state.lock().unwrap();
                if let Some(svc) = svcs.get_mut(svc_idx) {
                    svc.diagnosis = Some(msg);
                }
                force_render = true;
            }

            // ── Auto log rotation ─────────────────────────────────────────────────
            if last_rotation_check.elapsed().as_secs() >= LOG_ROTATION_INTERVAL_SECS {
                let log_paths: Vec<PathBuf> = {
                    let svcs = state.lock().unwrap();
                    svcs.iter()
                        .filter(|s| !matches!(s.health, Health::Pending))
                        .map(|s| s.log_path.clone())
                        .collect()
                };
                for path in &log_paths {
                    if let Ok(meta) = fs::metadata(path) {
                        if meta.len() > LOG_MAX_BYTES {
                            let _ = rotate_log(path);
                            force_render = true;
                        }
                    }
                }
                last_rotation_check = Instant::now();
            }

            // ── Auto-spawn services waiting on requires ───────────────────────────
            #[allow(clippy::type_complexity)]
            let (spawn_candidates, copilot_backend_url): (
                Vec<(usize, String, SpawnCmd, PathBuf)>,
                Option<String>,
            ) = {
                let svcs = state.lock().unwrap();
                let url = svcs
                    .iter()
                    .find(|s| s.name == "copilot-backend")
                    .and_then(|s| s.url.clone());
                let candidates = svcs
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.is_waiting_for_requires())
                    .filter(|(_, s)| {
                        s.requires
                            .iter()
                            .all(|req| svcs.iter().any(|o| &o.name == req && o.is_healthy()))
                    })
                    .filter_map(|(i, s)| {
                        s.spawn_cmd
                            .clone()
                            .map(|cmd| (i, s.name.clone(), cmd, s.log_path.clone()))
                    })
                    .collect();
                (candidates, url)
            };
            for (idx, svc_name, mut cmd, log_path) in spawn_candidates {
                // Before spawning opencti-graphql, ensure ES is accepting connections.
                // If not ready yet, defer to the next loop tick rather than crashing.
                if svc_name == "opencti-graphql" && !opensearch_ready(es_port) {
                    let mut svcs = state.lock().unwrap();
                    svcs[idx].health = Health::Degraded("Waiting for OpenSearch/ES…".into());
                    continue;
                }
                if let Some(ref url) = copilot_backend_url {
                    match svc_name.as_str() {
                        "opencti-graphql" => {
                            wipe_opencti_es_indices_if_stale(es_port);
                            let ws_file = ws_env_path(&ws_env_dir, "opencti");
                            let repo_file = paths
                                .opencti
                                .join("opencti-platform/opencti-graphql/.env.dev");

                            let xtm_one_enabled = parse_env_file(&ws_file)
                                .get("XTM_ONE_ENABLED")
                                .is_some_and(|v| v == "true");

                            if xtm_one_enabled {
                                // Read the actual registration token from the Copilot workspace env
                                // (falls back to the well-known dev default if not overridden).
                                let copilot_ws = ws_env_path(&ws_env_dir, "copilot");
                                let xtm_token = parse_env_file(&copilot_ws)
                                    .get("PLATFORM_REGISTRATION_TOKEN")
                                    .cloned()
                                    .unwrap_or_else(|| {
                                        "xtm-default-registration-token".to_string()
                                    });
                                if ws_file.exists() {
                                    let mut fenv = parse_env_file(&ws_file);
                                    fenv.insert("XTM__XTM_ONE_URL".to_string(), url.clone());
                                    fenv.insert(
                                        "XTM__XTM_ONE_TOKEN".to_string(),
                                        xtm_token.clone(),
                                    );
                                    write_env_file(&ws_file, &fenv);
                                    deploy_workspace_env(&ws_file, &repo_file);
                                }
                                cmd.env.insert("XTM__XTM_ONE_URL".to_string(), url.clone());
                                cmd.env.insert("XTM__XTM_ONE_TOKEN".to_string(), xtm_token);
                            }
                        }
                        "openaev-backend" => {
                            let ws_file = ws_env_path(&ws_env_dir, "openaev");
                            let repo_file = paths.openaev.join("openaev-dev/.env");
                            let xtm_one_enabled = parse_env_file(&ws_file)
                                .get("OPENAEV_XTM_ONE_ENABLE")
                                .is_some_and(|v| v == "true");

                            if xtm_one_enabled && ws_file.exists() {
                                let copilot_ws = ws_env_path(&ws_env_dir, "copilot");
                                let xtm_token = parse_env_file(&copilot_ws)
                                    .get("PLATFORM_REGISTRATION_TOKEN")
                                    .cloned()
                                    .unwrap_or_else(|| {
                                        "xtm-default-registration-token".to_string()
                                    });
                                let mut fenv = parse_env_file(&ws_file);
                                fenv.insert(
                                    "OPENAEV_XTM_ONE_ENABLE".to_string(),
                                    "true".to_string(),
                                );
                                fenv.insert("OPENAEV_XTM_ONE_URL".to_string(), url.clone());
                                fenv.insert("OPENAEV_XTM_ONE_TOKEN".to_string(), xtm_token.clone());
                                write_env_file(&ws_file, &fenv);
                                deploy_workspace_env(&ws_file, &repo_file);
                                cmd.env.insert(
                                    "OPENAEV_XTM_ONE_ENABLE".to_string(),
                                    "true".to_string(),
                                );
                                cmd.env
                                    .insert("OPENAEV_XTM_ONE_URL".to_string(), url.clone());
                                cmd.env
                                    .insert("OPENAEV_XTM_ONE_TOKEN".to_string(), xtm_token);
                            }
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
                        svcs[idx].health = if has_url {
                            Health::Launching
                        } else {
                            Health::Running
                        };
                        svcs[idx].pid = Some(child.id());
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

            if force_render || last_snapshot.elapsed() >= Duration::from_secs(1) {
                publish_runtime_snapshot_for_state(&slug, &state, port_offset);
                last_snapshot = Instant::now();
            }

            // ── Input handling ────────────────────────────────────────────────────
            let mut got_input = false;
            if has_tui {
                let visible_count = state
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|s| !matches!(s.health, Health::Pending))
                    .count();

                while let Ok(event) = rx.try_recv() {
                    got_input = true;
                    match &mut mode {
                        Mode::Overview { cursor } => match event {
                            InputEvent::Up => {
                                *cursor = cursor.saturating_sub(1);
                            }
                            InputEvent::Down if visible_count > 0 => {
                                *cursor = (*cursor + 1).min(visible_count - 1);
                            }
                            InputEvent::Enter => {
                                let svcs = state.lock().unwrap();
                                let visible: Vec<usize> = svcs
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, s)| !matches!(s.health, Health::Pending))
                                    .map(|(i, _)| i)
                                    .collect();
                                if let Some(&svc_idx) = visible.get(*cursor) {
                                    drop(svcs);
                                    mode = Mode::LogView {
                                        svc_idx,
                                        scroll: 0,
                                        follow: true,
                                    };
                                }
                            }
                            InputEvent::Diagnose => {
                                let svc_to_diag: Option<(usize, crate::services::Svc)> = {
                                    let svcs = state.lock().unwrap();
                                    let visible: Vec<usize> = svcs
                                        .iter()
                                        .enumerate()
                                        .filter(|(_, s)| !matches!(s.health, Health::Pending))
                                        .map(|(i, _)| i)
                                        .collect();
                                    visible
                                        .get(*cursor)
                                        .and_then(|&idx| svcs.get(idx).map(|s| (idx, s.clone())))
                                };
                                if let Some((idx, svc)) = svc_to_diag {
                                    let findings = diagnose_service(&svc, &paths, &ws_env_dir);
                                    let diag_cursor = findings
                                        .iter()
                                        .position(|f| f.fix.is_some() && !f.resolved)
                                        .unwrap_or(0);
                                    mode = Mode::Diagnose {
                                        svc_idx: idx,
                                        findings,
                                        cursor: diag_cursor,
                                    };
                                }
                            }
                            InputEvent::Restart => {
                                let visible: Vec<usize> = {
                                    let svcs = state.lock().unwrap();
                                    svcs.iter()
                                        .enumerate()
                                        .filter(|(_, s)| !matches!(s.health, Health::Pending))
                                        .map(|(i, _)| i)
                                        .collect()
                                };
                                if let Some(&idx) = visible.get(*cursor) {
                                    let _ = restart_service_at_idx(
                                        &state,
                                        &mut procs,
                                        &mut diagnosed,
                                        &slug,
                                        idx,
                                        es_port,
                                    );
                                    force_render = true;
                                }
                            }
                            InputEvent::Stop => {
                                let visible: Vec<usize> = {
                                    let svcs = state.lock().unwrap();
                                    svcs.iter()
                                        .enumerate()
                                        .filter(|(_, s)| !matches!(s.health, Health::Pending))
                                        .map(|(i, _)| i)
                                        .collect()
                                };
                                if let Some(&idx) = visible.get(*cursor) {
                                    let is_stopped = {
                                        let svcs = state.lock().unwrap();
                                        matches!(svcs[idx].health, Health::Stopped)
                                    };
                                    if !is_stopped {
                                        let _ = stop_service_at_idx(&state, &mut procs, idx);
                                        force_render = true;
                                    }
                                }
                            }
                            InputEvent::FullRestart => {
                                drop(raw_mode.take());
                                ensure_cooked_output();
                                print!("\x1b[H\x1b[2J");
                                let _ = io::stdout().flush();

                                let svc_names: Vec<String> = {
                                    let svcs = state.lock().unwrap();
                                    svcs.iter()
                                        .filter(|s| {
                                            !matches!(s.health, Health::Pending)
                                                && s.spawn_cmd.is_some()
                                        })
                                        .map(|s| s.name.clone())
                                        .collect()
                                };

                                let p = |s: &str| {
                                    print!("{s}\r\n");
                                };
                                p(&format!("\n  {BOLD}{YLW}⚠  Full stack restart{R}\n"));
                                p("  The following will be restarted:");
                                for name in &svc_names {
                                    p(&format!("    {DIM}•{R}  {name}"));
                                }
                                for dp in &docker_projects {
                                    p(&format!("    {DIM}•{R}  Docker — {} containers", dp.label));
                                }
                                p("");
                                p(&format!(
                                    "  {DIM}Database data and volumes are NOT wiped.{R}"
                                ));
                                p("");
                                p(&format!("  {CYN}Enter{R} confirm   {DIM}q / Esc{R} cancel"));
                                let _ = io::stdout().flush();

                                let _input_pause = InputPauseGuard::new(&input_paused);
                                while rx.try_recv().is_ok() {}
                                let _ = crossterm::terminal::enable_raw_mode();
                                let confirmed = loop {
                                    if stopping.load(Ordering::Relaxed) {
                                        break false;
                                    }
                                    if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                                        if let Ok(Event::Key(k)) = event::read() {
                                            match k.code {
                                                KeyCode::Enter => break true,
                                                KeyCode::Char('q') | KeyCode::Esc => break false,
                                                KeyCode::Char('c')
                                                    if k.modifiers
                                                        .contains(KeyModifiers::CONTROL) =>
                                                {
                                                    break false
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                };
                                let _ = crossterm::terminal::disable_raw_mode();
                                drop(_input_pause);
                                while rx.try_recv().is_ok() {}

                                if confirmed {
                                    ensure_cooked_output();
                                    print!("\x1b[H\x1b[2J");
                                    let p = |s: &str| {
                                        print!("{s}\r\n");
                                    };
                                    p(&format!("\n  {BOLD}Restarting full stack…{R}\n"));
                                    let _ = io::stdout().flush();
                                    let _ = restart_workspace_runtime(
                                        &state,
                                        &mut procs,
                                        &mut diagnosed,
                                        &slug,
                                        &docker_projects,
                                    );
                                    thread::sleep(Duration::from_millis(400));
                                }

                                drain_input_events();
                                raw_mode = TuiGuard::enter();
                                mode = Mode::Overview { cursor: 0 };
                                force_render = true;
                            }
                            InputEvent::Back => {
                                drop(raw_mode.take());
                                while rx.try_recv().is_ok() {}
                                let choice = prompt_back_action(&stopping, &input_paused);
                                drain_input_events();
                                while rx.try_recv().is_ok() {}
                                raw_mode = TuiGuard::enter();
                                force_render = true;
                                match choice {
                                    BackChoice::Stop => {
                                        want_restart = true;
                                        stopping.store(true, Ordering::Relaxed);
                                    }
                                    BackChoice::Detach => {
                                        want_detach = true;
                                    }
                                    BackChoice::Cancel => {}
                                }
                            }
                            InputEvent::Detach => {
                                want_detach = true;
                            }
                            InputEvent::Credentials => {
                                creds = gather_credentials(&ws_env_dir, &paths);
                                mode = Mode::Credentials;
                            }
                            InputEvent::TogglePaths => {
                                show_paths = !show_paths;
                            }
                            InputEvent::OpenInCode => {
                                let dir: Option<std::path::PathBuf> = {
                                    let svcs = state.lock().unwrap();
                                    let visible: Vec<usize> = svcs
                                        .iter()
                                        .enumerate()
                                        .filter(|(_, s)| !matches!(s.health, Health::Pending))
                                        .map(|(i, _)| i)
                                        .collect();
                                    visible.get(*cursor).and_then(|&idx| {
                                        let name = &svcs[idx].name;
                                        // Open the repo root, not the service subdirectory.
                                        let repo_root = if name.starts_with("copilot") {
                                            Some(paths.copilot.clone())
                                        } else if name.starts_with("opencti") {
                                            Some(paths.opencti.clone())
                                        } else if name.starts_with("openaev") {
                                            Some(paths.openaev.clone())
                                        } else if name.starts_with("connector") {
                                            Some(paths.connector.clone())
                                        } else if name.starts_with("grafana") {
                                            Some(paths.grafana.clone())
                                        } else if name.starts_with("langfuse") {
                                            Some(paths.langfuse.clone())
                                        } else if name.starts_with("infinity") {
                                            Some(paths.infinity.clone())
                                        } else {
                                            svcs[idx].spawn_cmd.as_ref().map(|c| c.dir.clone())
                                        };
                                        repo_root
                                    })
                                };
                                if let Some(dir) = dir {
                                    open_dir_in_vscode(&dir);
                                }
                            }
                            _ => {}
                        },
                        Mode::LogView {
                            svc_idx,
                            scroll,
                            follow,
                        } => match event {
                            InputEvent::Back => {
                                mode = Mode::Overview { cursor: 0 };
                            }
                            InputEvent::Up => {
                                *scroll += 5;
                                *follow = false;
                            }
                            InputEvent::Down => {
                                *scroll = scroll.saturating_sub(5);
                                if *scroll == 0 {
                                    *follow = true;
                                }
                            }
                            InputEvent::PageUp => {
                                *scroll += 20;
                                *follow = false;
                            }
                            InputEvent::PageDown => {
                                *scroll = scroll.saturating_sub(20);
                                if *scroll == 0 {
                                    *follow = true;
                                }
                            }
                            InputEvent::Follow => {
                                *scroll = 0;
                                *follow = true;
                            }
                            InputEvent::Diagnose => {
                                let idx = *svc_idx;
                                let svc_clone = {
                                    let svcs = state.lock().unwrap();
                                    svcs.get(idx).cloned()
                                };
                                if let Some(svc) = svc_clone {
                                    let findings = diagnose_service(&svc, &paths, &ws_env_dir);
                                    let diag_cursor = findings
                                        .iter()
                                        .position(|f| f.fix.is_some() && !f.resolved)
                                        .unwrap_or(0);
                                    mode = Mode::Diagnose {
                                        svc_idx: idx,
                                        findings,
                                        cursor: diag_cursor,
                                    };
                                }
                            }
                            InputEvent::RotateLog => {
                                let log_path = {
                                    let svcs = state.lock().unwrap();
                                    svcs.get(*svc_idx).map(|s| s.log_path.clone())
                                };
                                if let Some(path) = log_path {
                                    let _ = rotate_log(&path);
                                    *scroll = 0;
                                    *follow = true;
                                }
                                force_render = true;
                            }
                            _ => {}
                        },
                        Mode::Diagnose {
                            cursor,
                            findings,
                            svc_idx,
                        } => match event {
                            InputEvent::Back => {
                                mode = Mode::Overview { cursor: 0 };
                            }
                            InputEvent::Up => {
                                *cursor = cursor.saturating_sub(1);
                            }
                            InputEvent::Down if *cursor + 1 < findings.len() => {
                                *cursor += 1;
                            }
                            InputEvent::Enter => {
                                let idx = *svc_idx;
                                let cur = *cursor;
                                let fix_action = findings.get(cur).and_then(|f| f.fix.clone());
                                if let Some(action) = fix_action {
                                    let wants_restart = action.restart_after();
                                    drop(raw_mode.take());
                                    ensure_cooked_output();
                                    print!("\x1b[H\x1b[2J");
                                    let _ = io::stdout().flush();
                                    let fix_ok = diagnosis::run_fix_action(&action);

                                    if fix_ok && wants_restart {
                                        let (cmd, log_path) = {
                                            let svcs = state.lock().unwrap();
                                            svcs.get(idx)
                                                .map(|s| {
                                                    llog!(
                                                        "[RESTART] {} — restarting after fix",
                                                        s.name
                                                    );
                                                    (s.spawn_cmd.clone(), s.log_path.clone())
                                                })
                                                .unwrap_or_default()
                                        };
                                        if let Some(cmd) = cmd {
                                            if let Some(pos) =
                                                procs.iter().position(|p| p.idx == idx)
                                            {
                                                unsafe {
                                                    libc::kill(-procs[pos].pgid, libc::SIGKILL);
                                                }
                                                procs.remove(pos);
                                            }
                                            let args: Vec<&str> =
                                                cmd.args.iter().map(|s| s.as_str()).collect();
                                            match spawn_svc(
                                                &cmd.prog, &args, &cmd.dir, &cmd.env, &log_path,
                                            ) {
                                                Ok((child, pgid)) => {
                                                    let mut svcs = state.lock().unwrap();
                                                    let has_url = svcs[idx].url.is_some();
                                                    record_pid(&slug, child.id());
                                                    svcs[idx].health = if has_url {
                                                        Health::Launching
                                                    } else {
                                                        Health::Running
                                                    };
                                                    svcs[idx].pid = Some(child.id());
                                                    svcs[idx].started_at = Some(Instant::now());
                                                    svcs[idx].restarted_at = Some(Instant::now());
                                                    svcs[idx].diagnosis = None;
                                                    procs.push(Proc { idx, pgid, child });
                                                    println!(
                                                    "\n  {GRN}✓{R}  Service restarted — returning to overview."
                                                );
                                                }
                                                Err(e) => {
                                                    let mut svcs = state.lock().unwrap();
                                                    svcs[idx].health =
                                                        Health::Degraded(e.to_string());
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
                                        let svc_snap = state.lock().unwrap().get(idx).cloned();
                                        let new_findings = svc_snap
                                            .map(|svc| diagnose_service(&svc, &paths, &ws_env_dir))
                                            .unwrap_or_default();
                                        let new_cursor = new_findings
                                            .iter()
                                            .position(|f| f.fix.is_some() && !f.resolved)
                                            .unwrap_or(0);
                                        mode = Mode::Diagnose {
                                            svc_idx: idx,
                                            findings: new_findings,
                                            cursor: new_cursor,
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
                                                health: s.health.label_plain(),
                                                uptime_secs: s.secs(),
                                                log_path: s.log_path.clone(),
                                                spawn_cmd: s.spawn_cmd.as_ref().map(|c| {
                                                    let mut parts = vec![c.prog.clone()];
                                                    parts.extend(c.args.iter().cloned());
                                                    parts.join(" ")
                                                }),
                                            })
                                        };
                                        let Some(ctx) = ctx else { continue };
                                        let svc_name = {
                                            let svcs = state.lock().unwrap();
                                            svcs.get(idx)
                                                .map(|s| s.name.clone())
                                                .unwrap_or_default()
                                        };
                                        let log_tail = tail_file(&ctx.log_path, 15);
                                        drop(raw_mode.take());
                                        let _ = disable_raw_mode();
                                        ensure_cooked_output();

                                        let p = |s: &str| {
                                            print!("{s}\r\n");
                                        };
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
                                            p(&format!(
                                                "  {DIM}Logs ({} lines):{R}",
                                                log_tail.len()
                                            ));
                                            let preview: Vec<_> =
                                                log_tail.iter().rev().take(8).rev().collect();
                                            for line in &preview {
                                                p(&format!("  {DIM}│{R} {line}"));
                                            }
                                            if log_tail.len() > 8 {
                                                p(&format!(
                                                    "  {DIM}  … ({} more lines in issue){R}",
                                                    log_tail.len() - 8
                                                ));
                                            }
                                        }
                                        p("");
                                        p("  This will open an issue at AreDee-Bangs/dev-launcher");
                                        p("  so the recipe can be implemented.");
                                        p("");
                                        p(&format!(
                                            "  {CYN}Enter{R} create issue   {DIM}q / Esc{R} cancel"
                                        ));
                                        let _ = io::stdout().flush();

                                        let _input_pause = InputPauseGuard::new(&input_paused);
                                        while rx.try_recv().is_ok() {}
                                        let _ = crossterm::terminal::enable_raw_mode();
                                        let confirmed = loop {
                                            if stopping.load(Ordering::Relaxed) {
                                                break false;
                                            }
                                            if event::poll(Duration::from_millis(100))
                                                .unwrap_or(false)
                                            {
                                                if let Ok(Event::Key(k)) = event::read() {
                                                    match k.code {
                                                        KeyCode::Enter => break true,
                                                        KeyCode::Char('q') | KeyCode::Esc => {
                                                            break false
                                                        }
                                                        KeyCode::Char('c')
                                                            if k.modifiers.contains(
                                                                KeyModifiers::CONTROL,
                                                            ) =>
                                                        {
                                                            stopping.store(true, Ordering::Relaxed);
                                                            break false;
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                            }
                                        };
                                        let _ = disable_raw_mode();
                                        drop(_input_pause);
                                        while rx.try_recv().is_ok() {}

                                        if confirmed {
                                            print!("\r\n  Creating issue…\r\n");
                                            let _ = io::stdout().flush();
                                            match create_github_issue(
                                                &f.kind, &svc_name, &f.title, &f.body, &log_tail,
                                                &ctx,
                                            ) {
                                                Ok(url) => {
                                                    print!(
                                                        "\r  {GRN}✓{R}  Issue created: {url}\r\n"
                                                    )
                                                }
                                                Err(err) => {
                                                    print!("\r  {RED}✗{R}  Failed: {err}\r\n")
                                                }
                                            }
                                        } else {
                                            print!("\r\n  Cancelled.\r\n");
                                        }
                                        print!("\r\n  Returning to diagnosis…\r\n");
                                        let _ = io::stdout().flush();
                                        thread::sleep(Duration::from_millis(1500));
                                        raw_mode = TuiGuard::enter();
                                        let svc_snap = state.lock().unwrap().get(idx).cloned();
                                        let new_findings = svc_snap
                                            .map(|svc| diagnose_service(&svc, &paths, &ws_env_dir))
                                            .unwrap_or_default();
                                        mode = Mode::Diagnose {
                                            svc_idx: idx,
                                            findings: new_findings,
                                            cursor: cur.min(findings.len().saturating_sub(1)),
                                        };
                                        force_render = true;
                                    }
                                }
                            }
                            _ => {}
                        },
                        Mode::Credentials => {
                            if let InputEvent::Back = event {
                                mode = Mode::Overview { cursor: 0 };
                            }
                        }
                    }
                }

                if let Mode::Overview { cursor } = &mut mode {
                    if visible_count > 0 {
                        *cursor = (*cursor).min(visible_count - 1);
                    }
                }
            }

            // ── Render ────────────────────────────────────────────────────────────
            if got_input || force_render || last_render.elapsed() >= render_interval {
                if let Some(tui) = raw_mode.as_mut() {
                    let svcs = state.lock().unwrap();
                    let lines = match &mode {
                        Mode::Overview { cursor } => build_overview_lines(
                            &svcs, &slug, &logs_dir, *cursor, has_tui, show_paths,
                        ),
                        Mode::LogView {
                            svc_idx,
                            scroll,
                            follow,
                        } => svcs
                            .get(*svc_idx)
                            .map(|svc| build_log_view_lines(svc, *scroll, *follow))
                            .unwrap_or_default(),
                        Mode::Diagnose {
                            svc_idx,
                            findings,
                            cursor,
                        } => svcs
                            .get(*svc_idx)
                            .map(|svc| build_diagnose_lines(svc, findings, *cursor))
                            .unwrap_or_default(),
                        Mode::Credentials => build_credentials_lines(&creds, &slug),
                    };
                    drop(svcs);
                    draw_ansi_lines(tui, &lines);
                }
                last_render = Instant::now();
                force_render = false;
            }

            thread::sleep(Duration::from_millis(20));
        }
    } // 'session loop
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();
    let workspace_root = resolve_workspace_root(&args);
    let ws_dir = workspaces_dir(&workspace_root);
    if let Some(CliCommand::Workspace(cmd)) = &args.command {
        if let Err(err) = run_workspace_command(cmd, &ws_dir) {
            eprintln!("{err}");
            std::process::exit(1);
        }
        return;
    }
    if args.session_worker {
        run_session_loop(&args, &workspace_root, &ws_dir);
    } else {
        run_as_selector(&args, &workspace_root, &ws_dir);
    }
}
