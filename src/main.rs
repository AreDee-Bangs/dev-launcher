//! dev-launcher — multi-product stack launcher with process-tree management, health monitoring,
//! and an interactive TUI for diving into per-service logs.

pub mod args;
pub mod config;
pub mod diagnosis;
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

// ── Public re-exports from submodules ─────────────────────────────────────────

use args::Args;
use config::{load_config, read_line_or_interrupt, resolve_workspace_root};
use diagnosis::{create_github_issue, diagnose_service, needs_recipe, DiagEvent, IssueContext};
use services::{
    alive_pid_count, docker_available, docker_compose_down, docker_compose_up,
    docker_down_workspace, docker_running_for_workspace, ensure_opencti_env,
    ensure_opencti_graphql_python_deps, kill_orphaned_pids, load_repo_manifest,
    mark_detached, detached_marker_path, opensearch_ready, patch_manifest_ports, pid_file_path,
    probe, read_compose_postgres_password, read_worker_pid, record_pid, compress_rotated_logs,
    remove_worker_pid, resolve_docker_project, rotate_log, run_blocking, run_manifest_bootstrap,
    sighup_handler, shutdown_detached_session, spawn_svc, split_health_url_parts,
    wait_for_opensearch, wipe_opencti_es_indices_if_stale, workspace_run_status,
    write_compose_override, write_worker_pid, ws_docker_project,
    DockerProject, Health, Paths, Proc, SpawnCmd, State, WorkspaceRunStatus, SIGHUP_STOP,
};
use tui::{
    build_credentials_lines, build_diagnose_lines, build_log_view_lines, build_overview_lines,
    drain_input_events, draw_ansi_lines, ensure_cooked_output, gather_credentials, render_shutdown,
    spawn_input_thread, tail_file, CredEntry, InputEvent, Mode, TermStatus, TuiGuard, BOLD,
    BUILD_VERSION, CYN, DIM, GRN, R, RED, YLW,
};
use workspace::{
    apply_port_offset_to_env, branch_to_slug, choices_to_workspace, compute_workspace_hash,
    current_branch, current_commit_short, default_product_choices, deploy_workspace_env,
    discover_flags_in_dir, ensure_worktree, extract_url_port, find_free_offset, init_workspace_env,
    list_workspaces, load_workspace, parse_env_file, patch_url_default, port_in_use,
    preflight_port_checks, read_active_flags, read_env_url_port, run_env_wizard, run_flag_selector,
    run_platform_mode_selector, run_product_selector, run_workspace_delete, run_workspace_selector,
    save_workspace, today, workspace_to_choices, workspaces_dir, write_active_flags, write_env_file,
    ws_env_path, FlagChoice, LaunchMode, PortCheck, ProductChoice, WorkspaceAction, WorkspaceConfig,
    WorkspaceEntry, COMMIT_PREFIX, CONNECTOR_ENV_VARS, COPILOT_ENV_VARS, OPENCTI_ENV_VARS,
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
    let compose = copilot_dir.join("docker-compose.dev.yml");
    if let Some(password) = read_compose_postgres_password(&compose) {
        env.insert(
            "DATABASE_URL".into(),
            format!("postgresql+asyncpg://copilot:{password}@localhost:5432/copilot"),
        );
    }
    env
}

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

fn maven_cmd(openaev_root: &Path) -> String {
    let wrapper = openaev_root.join("mvnw");
    if wrapper.exists() {
        wrapper.to_string_lossy().into_owned()
    } else {
        "mvn".to_string()
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
            if is_new {
                let _ = fs::write(
                    dir.join("docker-compose.dev.yml"),
                    include_str!("infra/langfuse/docker-compose.dev.yml"),
                );
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
            stopped.push(StoppedSession { hash: hash.to_string(), pid });
            return true;
        }
    }
}

fn startup_orphan_check(ws_dir: &Path, _workspace_root: &Path) {
    for ws in list_workspaces(ws_dir) {
        if !detached_marker_path(&ws.hash).exists() {
            continue;
        }

        if let Some(worker_pid) = read_worker_pid(&ws.hash) {
            let alive = unsafe { libc::kill(worker_pid as libc::pid_t, 0) } == 0;
            if alive {
                eprintln!(
                    "  [dev-launcher] Stopped session {} found (previous selector exited). Terminating.",
                    ws.hash
                );
                unsafe { libc::kill(worker_pid as libc::pid_t, libc::SIGTERM); }
                thread::sleep(Duration::from_millis(300));
                unsafe { libc::kill(worker_pid as libc::pid_t, libc::SIGKILL); }
                remove_worker_pid(&ws.hash);
                let _ = fs::remove_file(detached_marker_path(&ws.hash));
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
        || args.copilot_branch.is_some() || args.opencti_branch.is_some()
        || args.openaev_branch.is_some() || args.connector_branch.is_some()
        || args.copilot_commit.is_some() || args.opencti_commit.is_some()
        || args.openaev_commit.is_some() || args.connector_commit.is_some()
        || args.copilot_worktree.is_some() || args.opencti_worktree.is_some()
        || args.openaev_worktree.is_some() || args.connector_worktree.is_some();

    if has_direct {
        let (cfg, _, _) = resolve_workspace(args, workspace_root, ws_dir);
        loop {
            if selector_stopping.load(Ordering::Relaxed) { break; }
            let pid = spawn_session_worker(&exe, &cfg.hash, args.clean_start);
            wait_for_session(pid, &cfg.hash, &mut stopped);
            if selector_stopping.load(Ordering::Relaxed) { break; }
        }
        for s in &stopped {
            unsafe { libc::kill(s.pid as libc::pid_t, libc::SIGTERM); }
        }
        return;
    }

    startup_orphan_check(ws_dir, workspace_root);

    'selector: loop {
        if selector_stopping.load(Ordering::Relaxed) {
            for s in &stopped {
                unsafe { libc::kill(s.pid as libc::pid_t, libc::SIGTERM); }
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
                WorkspaceAction::Delete(cfg) => {
                    if let Some(pos) = stopped.iter().position(|s| s.hash == cfg.hash) {
                        let s = stopped.remove(pos);
                        unsafe { libc::kill(s.pid as libc::pid_t, libc::SIGTERM); }
                        thread::sleep(Duration::from_millis(500));
                        unsafe { libc::kill(s.pid as libc::pid_t, libc::SIGKILL); }
                        let _ = fs::remove_file(detached_marker_path(&s.hash));
                        remove_worker_pid(&s.hash);
                    }
                    run_workspace_delete(&cfg, workspace_root, ws_dir);
                }
                other => break other,
            }
        };

        match action {
            WorkspaceAction::Reattach(cfg) => {
                if let Some(pos) = stopped.iter().position(|s| s.hash == cfg.hash) {
                    let s = stopped.remove(pos);
                    unsafe { libc::kill(s.pid as libc::pid_t, libc::SIGCONT); }
                    wait_for_session(s.pid, &s.hash, &mut stopped);
                }
            }
            WorkspaceAction::StopSession(cfg) => {
                if let Some(pos) = stopped.iter().position(|s| s.hash == cfg.hash) {
                    let s = stopped.remove(pos);
                    unsafe { libc::kill(s.pid as libc::pid_t, libc::SIGTERM); }
                    thread::sleep(Duration::from_millis(500));
                    unsafe { libc::kill(s.pid as libc::pid_t, libc::SIGKILL); }
                    let _ = fs::remove_file(detached_marker_path(&s.hash));
                    remove_worker_pid(&s.hash);
                }
            }
            WorkspaceAction::Open(cfg) => {
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
            WorkspaceAction::CreateNew => {
                let mut choices = default_product_choices(workspace_root);
                drain_input_events();
                let clean = match run_product_selector("new", &mut choices) {
                    LaunchMode::Quit => continue 'selector,
                    LaunchMode::Clean => true,
                    LaunchMode::Normal => false,
                };
                let mut cfg = choices_to_workspace(&choices);
                cfg.port_offset = find_free_offset(ws_dir);
                save_workspace(ws_dir, &cfg);
                let pid = spawn_session_worker(&exe, &cfg.hash, clean);
                wait_for_session(pid, &cfg.hash, &mut stopped);
            }
            WorkspaceAction::Delete(_) => {
                // Handled inline in the inner loop above.
            }
            WorkspaceAction::Quit => {
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
                            unsafe { libc::kill(s.pid as libc::pid_t, libc::SIGTERM); }
                        }
                        thread::sleep(Duration::from_secs(1));
                        for s in &stopped {
                            unsafe { libc::kill(s.pid as libc::pid_t, libc::SIGKILL); }
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
    println!(
        "  {BOLD}OpenCTI + XTM One (Copilot) integration{R}"
    );
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
    let mut cfg = choices_to_workspace(&choices);
    cfg.port_offset = find_free_offset(ws_dir);
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
                port_offset: find_free_offset(ws_dir),
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
                WorkspaceAction::Delete(cfg) => {
                    run_workspace_delete(&cfg, workspace_root, ws_dir);
                    continue 'selector;
                }
                WorkspaceAction::Open(cfg) => {
                    break workspace_to_choices(&cfg, workspace_root);
                }
                WorkspaceAction::CreateNew => {
                    break default_product_choices(workspace_root);
                }
                WorkspaceAction::Reattach(cfg) => {
                    break workspace_to_choices(&cfg, workspace_root);
                }
                WorkspaceAction::StopSession(_) => {
                    continue 'selector;
                }
                WorkspaceAction::Quit => {
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
                ensure_worktree(&workspace_root, c.repo, &c.branch);
            }
        }
        if need_worktrees {
            println!();
        }
    }

    let paths = {
        let resolve_path = |repo: &str, branch: &str, override_path: Option<&PathBuf>| -> PathBuf {
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
        }
    };

    let no_copilot = !(choices[0].enabled && paths.copilot.is_dir());
    let no_opencti = !(choices[1].enabled && paths.opencti.is_dir());
    let no_openaev = !(choices[2].enabled && paths.openaev.is_dir());
    let no_connector = !(choices[3].enabled && paths.connector.is_dir());
    let no_grafana = !choices[4].enabled;
    let no_langfuse = !choices[5].enabled;
    let no_opencti_front = no_opencti || args.no_opencti_front;
    let no_openaev_front = no_openaev || args.no_openaev_front;

    // ── Port offset — derives workspace-specific ports from the stored offset ──
    let port_offset = workspace_cfg.port_offset;
    // Elasticsearch/OpenSearch host port (workspace-specific)
    let es_port: u16 = 9200u16.saturating_add(port_offset);
    // opencti-graphql GraphQL server port
    let opencti_gql_port: u16 = 4000u16.saturating_add(port_offset);
    // openaev Spring Boot server port
    let openaev_be_port: u16 = 8080u16.saturating_add(port_offset);
    if port_offset > 0 {
        println!(
            "  {DIM}Port offset +{port_offset}  \
             (opencti:{opencti_gql_port}  es:{es_port}  openaev:{openaev_be_port}){R}"
        );
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

    // ── Step 1 / 2 — Environment ──────────────────────────────────────────────
    let sep = "─".repeat(56);
    println!("\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}");
    println!("\n  {DIM}{sep}{R}");
    println!("  {BOLD}Step 1 / 2  —  Environment{R}");
    println!("  {DIM}{sep}{R}\n");

    let ws_env_dir = ws_dir.join(&slug);
    let _ = fs::create_dir_all(&ws_env_dir);

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
        // Apply workspace port offset (idempotent — no-op when offset is 0)
        if port_offset > 0 {
            patch_url_default(&env_path, "BASE_URL", 8100, 8100u16.saturating_add(port_offset));
            patch_url_default(&env_path, "FRONTEND_URL", 3100, 3100u16.saturating_add(port_offset));
            apply_port_offset_to_env(&env_path, "copilot", port_offset);
        }
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

    let copilot_env_path = ws_env_path(&ws_env_dir, "copilot");
    let copilot_backend_port = read_env_url_port(&copilot_env_path, "BASE_URL", 8100);
    let copilot_frontend_port = read_env_url_port(&copilot_env_path, "FRONTEND_URL", 3100);

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
                openaev_docker_ok = docker_compose_up("OpenAEV", &project, &dc, &dev_dir, &extra);
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
                        svc.health =
                            Health::Degraded("Docker deps not running — start Docker first".into());
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
                    let env = if def.cwd == "backend" {
                        &backend_env
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
                ensure_copilot_fe_deps(&fe_dir);
                let mut svc = services::Svc::new(
                    "copilot-frontend",
                    Some(&frontend_url),
                    "",
                    90,
                    logs_dir.join("copilot-frontend.log"),
                );
                if fe_dir.is_dir() {
                    let fe_port_str = copilot_frontend_port.to_string();
                    try_spawn!(
                        svc,
                        "yarn",
                        &["dev", "--port", &fe_port_str],
                        &fe_dir,
                        &HashMap::new()
                    );
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
                "/health",
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
                ensure_opencti_fe_deps(&front_dir);
                let mut svc = services::Svc::new(
                    "opencti-frontend",
                    Some(format!("http://localhost:{}", 3000u16.saturating_add(port_offset))),
                    "",
                    120,
                    logs_dir.join("opencti-frontend.log"),
                );
                if front_dir.is_dir() {
                    if !no_copilot && paths.copilot.is_dir() {
                        svc.requires = vec!["copilot-backend".to_string()];
                        svc.spawn_cmd = Some(SpawnCmd {
                            prog: "yarn".to_string(),
                            args: vec!["dev".to_string()],
                            dir: front_dir.clone(),
                            env: HashMap::new(),
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
                            "-Dspring-boot.run.profiles=dev",
                        ]
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                        dir: paths.openaev.clone(),
                        env: HashMap::new(),
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
                            "-Dspring-boot.run.profiles=dev"
                        ],
                        &paths.openaev,
                        &HashMap::new()
                    );
                }
            } else {
                svc.health = Health::Degraded("openaev-api/ not found".into());
                svcs.push(svc);
            }

            if !no_openaev_front {
                let fe_dir = paths.openaev.join("openaev-front");
                ensure_openaev_fe_deps(&fe_dir);
                let mut svc = services::Svc::new(
                    "openaev-frontend",
                    Some("http://localhost:3001"),
                    "",
                    90,
                    logs_dir.join("openaev-frontend.log"),
                );
                if fe_dir.is_dir() {
                    if !no_copilot && paths.copilot.is_dir() {
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
                svcs[i].health = if ok {
                    Health::Up
                } else if timed_out {
                    Health::Degraded(format!("no response after {timeout_secs}s"))
                } else {
                    match &svcs[i].health {
                        Health::Probing(n) => Health::Probing(n + 1),
                        _ => Health::Probing(1),
                    }
                };
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

    let (tx, rx) = mpsc::sync_channel::<InputEvent>(32);
    if has_tui {
        spawn_input_thread(tx, Arc::clone(&stopping));
    }

    // ── Main loop ─────────────────────────────────────────────────────────────
    print!("\x1b[2J");
    let render_interval = Duration::from_millis(500);
    let mut last_render = Instant::now();
    let mut force_render = true;
    let mut last_rotation_check = Instant::now();
    const LOG_ROTATION_INTERVAL_SECS: u64 = 30;
    const LOG_MAX_BYTES: u64 = 3_000_000;

    loop {
        // ── Detach (M) — leave TUI without stopping the stack ─────────────────
        if want_detach {
            want_detach = false;
            drop(raw_mode.take());
            mark_detached(&slug);
            write_worker_pid(&slug, std::process::id());
            print!("\x1b[H\x1b[2J");
            let _ = io::stdout().flush();
            compress_rotated_logs(&logs_dir);
            // Pause this process — the selector resumes it via SIGCONT when the
            // user reattaches.  Execution continues at the line below on resume.
            unsafe { libc::kill(libc::getpid(), libc::SIGSTOP); }
            // ── Resumed by SIGCONT ────────────────────────────────────────────
            let _ = fs::remove_file(detached_marker_path(&slug));
            remove_worker_pid(&slug);
            raw_mode = TuiGuard::enter();
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

            // Return to the selector process (which will decide whether to show
            // the workspace selector again or exit).
            print!("\x1b[H\x1b[2J");
            let _ = io::stdout().flush();
            return;
        }

        // ── Crash detection ───────────────────────────────────────────────────
        let mut auto_diagnose: Option<(usize, crate::services::Svc)> = None;
        {
            let mut svcs = state.lock().unwrap();
            for p in &mut procs {
                if let Some(code) = p.try_reap() {
                    let already_crashed = matches!(svcs[p.idx].health, Health::Crashed(_));
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
                    }
                    force_render = true;

                    let is_real_crash = matches!(svcs[p.idx].health, Health::Crashed(_));
                    if !already_crashed && !diagnosed.contains(&p.idx) && is_real_crash {
                        diagnosed.insert(p.idx);
                        let log_path = svcs[p.idx].log_path.clone();
                        let svc_idx = p.idx;
                        let tx = diag_tx.clone();
                        let llm = llm_cfg.clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(300));
                            if let Some(msg) = diagnosis::diagnose_crash(&log_path, llm.as_ref()) {
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
                                .unwrap_or_else(|| "xtm-default-registration-token".to_string());
                            if ws_file.exists() {
                                let mut fenv = parse_env_file(&ws_file);
                                fenv.insert("XTM__XTM_ONE_URL".to_string(), url.clone());
                                fenv.insert("XTM__XTM_ONE_TOKEN".to_string(), xtm_token.clone());
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
                        if ws_file.exists() {
                            let mut fenv = parse_env_file(&ws_file);
                            fenv.insert("OPENAEV_XTM_ONE_ENABLE".to_string(), "true".to_string());
                            fenv.insert("OPENAEV_XTM_ONE_URL".to_string(), url.clone());
                            fenv.insert(
                                "OPENAEV_XTM_ONE_TOKEN".to_string(),
                                "xtm-default-registration-token".to_string(),
                            );
                            write_env_file(&ws_file, &fenv);
                            deploy_workspace_env(&ws_file, &repo_file);
                        }
                        cmd.env
                            .insert("OPENAEV_XTM_ONE_ENABLE".to_string(), "true".to_string());
                        cmd.env
                            .insert("OPENAEV_XTM_ONE_URL".to_string(), url.clone());
                        cmd.env.insert(
                            "OPENAEV_XTM_ONE_TOKEN".to_string(),
                            "xtm-default-registration-token".to_string(),
                        );
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
                                let (cmd, log_path) = {
                                    let svcs = state.lock().unwrap();
                                    (svcs[idx].spawn_cmd.clone(), svcs[idx].log_path.clone())
                                };
                                if let Some(cmd) = cmd {
                                    if let Some(pos) = procs.iter().position(|p| p.idx == idx) {
                                        unsafe {
                                            libc::kill(-procs[pos].pgid, libc::SIGKILL);
                                        }
                                        procs.remove(pos);
                                    }
                                    let svc_name = {
                                        let svcs = state.lock().unwrap();
                                        svcs.get(idx).map(|s| s.name.clone()).unwrap_or_default()
                                    };
                                    let docker_ok = !cmd.requires_docker || docker_available();
                                    if !docker_ok {
                                        let mut svcs = state.lock().unwrap();
                                        svcs[idx].health = Health::Degraded(
                                            "Docker not running — start Docker first".into(),
                                        );
                                    } else if svc_name == "opencti-graphql"
                                        && !opensearch_ready(es_port)
                                    {
                                        // ES is still booting — defer to auto-spawn loop
                                        let mut svcs = state.lock().unwrap();
                                        svcs[idx].health =
                                            Health::Degraded("Waiting for OpenSearch/ES…".into());
                                    } else {
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
                                            }
                                            Err(e) => {
                                                let mut svcs = state.lock().unwrap();
                                                svcs[idx].health = Health::Degraded(e.to_string());
                                            }
                                        }
                                    }
                                    force_render = true;
                                } else {
                                    force_render = true;
                                }
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
                                    if let Some(pos) = procs.iter().position(|p| p.idx == idx) {
                                        unsafe {
                                            libc::kill(-procs[pos].pgid, libc::SIGKILL);
                                        }
                                        procs.remove(pos);
                                    }
                                    let mut svcs = state.lock().unwrap();
                                    svcs[idx].health = Health::Stopped;
                                    svcs[idx].pid = None;
                                    svcs[idx].diagnosis = None;
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
                            p(&format!(
                                "\n  {BOLD}{YLW}⚠  Full stack restart{R}\n"
                            ));
                            p("  The following will be restarted:");
                            for name in &svc_names {
                                p(&format!("    {DIM}•{R}  {name}"));
                            }
                            for dp in &docker_projects {
                                p(&format!(
                                    "    {DIM}•{R}  Docker — {} containers",
                                    dp.label
                                ));
                            }
                            p("");
                            p(&format!(
                                "  {DIM}Database data and volumes are NOT wiped.{R}"
                            ));
                            p("");
                            p(&format!(
                                "  {CYN}Enter{R} confirm   {DIM}q / Esc{R} cancel"
                            ));
                            let _ = io::stdout().flush();

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

                            if confirmed {
                                ensure_cooked_output();
                                print!("\x1b[H\x1b[2J");
                                let p = |s: &str| {
                                    print!("{s}\r\n");
                                };
                                p(&format!("\n  {BOLD}Restarting full stack…{R}\n"));
                                let _ = io::stdout().flush();

                                // Kill all processes
                                for proc in &mut procs {
                                    unsafe { libc::kill(-proc.pgid, libc::SIGKILL) };
                                }
                                procs.clear();
                                diagnosed.clear();

                                // Reset service states
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

                                // Restart Docker projects
                                for dp in &docker_projects {
                                    p(&format!(
                                        "  {DIM}docker compose restart  {}{R}",
                                        dp.label
                                    ));
                                    let _ = io::stdout().flush();
                                    let proj = dp.project.as_str();
                                    run_blocking(
                                        "docker",
                                        &["compose", "-p", proj, "restart"],
                                        &dp.work_dir,
                                    );
                                }

                                // Re-spawn all services
                                let spawn_targets: Vec<(usize, SpawnCmd, PathBuf)> = {
                                    let svcs = state.lock().unwrap();
                                    svcs.iter()
                                        .enumerate()
                                        .filter_map(|(i, s)| {
                                            s.spawn_cmd
                                                .clone()
                                                .map(|cmd| (i, cmd, s.log_path.clone()))
                                        })
                                        .collect()
                                };
                                for (idx, cmd, log_path) in spawn_targets {
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
                                        }
                                        Err(e) => {
                                            let mut svcs = state.lock().unwrap();
                                            svcs[idx].health = Health::Degraded(e.to_string());
                                        }
                                    }
                                }

                                thread::sleep(Duration::from_millis(400));
                            }

                            drain_input_events();
                            raw_mode = TuiGuard::enter();
                            mode = Mode::Overview { cursor: 0 };
                            force_render = true;
                        }
                        InputEvent::Back => {
                            stopping.store(true, Ordering::Relaxed);
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
                                            .map(|s| (s.spawn_cmd.clone(), s.log_path.clone()))
                                            .unwrap_or_default()
                                    };
                                    if let Some(cmd) = cmd {
                                        if let Some(pos) = procs.iter().position(|p| p.idx == idx) {
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
                                        svcs.get(idx).map(|s| s.name.clone()).unwrap_or_default()
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
                                        p(&format!("  {DIM}Logs ({} lines):{R}", log_tail.len()));
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

                                    let _ = crossterm::terminal::enable_raw_mode();
                                    let confirmed = loop {
                                        if stopping.load(Ordering::Relaxed) {
                                            break false;
                                        }
                                        if event::poll(Duration::from_millis(100)).unwrap_or(false)
                                        {
                                            if let Ok(Event::Key(k)) = event::read() {
                                                match k.code {
                                                    KeyCode::Enter => break true,
                                                    KeyCode::Char('q') | KeyCode::Esc => {
                                                        break false
                                                    }
                                                    KeyCode::Char('c')
                                                        if k.modifiers
                                                            .contains(KeyModifiers::CONTROL) =>
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

                                    if confirmed {
                                        print!("\r\n  Creating issue…\r\n");
                                        let _ = io::stdout().flush();
                                        match create_github_issue(
                                            f.kind, &svc_name, &f.title, &f.body, &log_tail, &ctx,
                                        ) {
                                            Ok(url) => {
                                                print!("\r  {GRN}✓{R}  Issue created: {url}\r\n")
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
                    Mode::Overview { cursor } => {
                        build_overview_lines(&svcs, &slug, &logs_dir, *cursor, has_tui, show_paths)
                    }
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
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();
    let workspace_root = resolve_workspace_root(&args);
    let ws_dir = workspaces_dir(&workspace_root);
    if args.session_worker {
        run_session_loop(&args, &workspace_root, &ws_dir);
    } else {
        run_as_selector(&args, &workspace_root, &ws_dir);
    }
}
