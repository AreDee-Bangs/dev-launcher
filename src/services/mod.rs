pub mod docker;
pub mod manifest;
pub mod process;

pub use docker::{
    compose_host_port, docker_available, docker_compose_down, docker_compose_running_count,
    docker_compose_up, docker_down_workspace, docker_kill_by_name_fragment,
    docker_running_for_workspace, opensearch_ready, parse_compose_container_names,
    replace_port_in_value, resolve_product_docker_for_down, run_blocking, run_blocking_logged,
    ensure_gitignore_entries, wait_for_opensearch, wipe_opencti_es_indices_if_stale,
    write_compose_override, ws_docker_project, DockerProject,
};
pub use manifest::{
    ensure_opencti_env, ensure_opencti_graphql_python_deps, infer_repo_manifest,
    load_repo_manifest, parse_compose_project_name, parse_dev_launcher_conf, patch_manifest_ports,
    read_compose_postgres_password, resolve_docker_project, resolve_docker_project_base,
    run_manifest_bootstrap, save_dev_launcher_conf, split_health_url_parts, BootstrapDef,
    ManifestDocker, RepoManifest, SvcDef,
};
pub use process::{
    alive_pid_count, compress_rotated_logs, detached_marker_path, kill_orphaned_pids,
    mark_detached, open_log, pid_file_path, probe, read_worker_pid, record_pid, remove_worker_pid,
    rotate_log, sighup_handler, shutdown_detached_session, spawn_svc, worker_pid_path,
    workspace_run_status, write_worker_pid,
    Proc, WorkspaceRunStatus, SIGHUP_STOP,
};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::tui::{CYN, DIM, GRN, R, RED, YLW};

// ── Health ────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
pub enum Health {
    Pending,
    Launching,
    Probing(u32),
    Up,
    Running,
    Stopped,
    Degraded(String),
    Crashed(i32),
}

impl Health {
    pub fn label(&self) -> String {
        match self {
            Health::Pending => format!("{DIM}pending{R}"),
            Health::Launching => format!("{YLW}launching{R}"),
            Health::Probing(n) => format!("{YLW}health probe #{n}{R}"),
            Health::Up => format!("{GRN}up{R}"),
            Health::Running => format!("{CYN}running{R}"),
            Health::Stopped => format!("{DIM}stopped{R}"),
            Health::Degraded(msg) => format!("{RED}degraded ({msg}){R}"),
            Health::Crashed(code) => format!("{RED}crashed ({code}){R}"),
        }
    }

    pub fn label_plain(&self) -> String {
        match self {
            Health::Pending => "pending".into(),
            Health::Launching => "launching".into(),
            Health::Probing(n) => format!("health probe #{n}"),
            Health::Up => "up".into(),
            Health::Running => "running".into(),
            Health::Stopped => "stopped".into(),
            Health::Degraded(msg) => format!("degraded ({msg})"),
            Health::Crashed(code) => format!("crashed ({code})"),
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(
            self,
            Health::Up | Health::Running | Health::Stopped | Health::Degraded(_) | Health::Crashed(_)
        )
    }
}

// ── SpawnCmd ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct SpawnCmd {
    pub prog: String,
    pub args: Vec<String>,
    pub dir: PathBuf,
    pub env: HashMap<String, String>,
    pub requires_docker: bool,
}

// ── Svc ───────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Svc {
    pub name: String,
    pub url: Option<String>,
    pub health_path: String,
    pub health: Health,
    pub pid: Option<u32>,
    pub started_at: Option<Instant>,
    pub restarted_at: Option<Instant>,
    pub startup_timeout: Duration,
    pub log_path: PathBuf,
    pub diagnosis: Option<String>,
    pub spawn_cmd: Option<SpawnCmd>,
    pub requires: Vec<String>,
}

impl Svc {
    pub fn new(
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
            restarted_at: None,
            startup_timeout: Duration::from_secs(timeout_secs),
            log_path,
            diagnosis: None,
            spawn_cmd: None,
            requires: Vec::new(),
        }
    }

    pub fn health_url(&self) -> Option<String> {
        self.url
            .as_deref()
            .map(|b| format!("{b}{}", self.health_path))
    }

    pub fn secs(&self) -> u64 {
        self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0)
    }

    pub fn is_healthy(&self) -> bool {
        matches!(self.health, Health::Up | Health::Running)
    }

    pub fn recently_restarted(&self) -> bool {
        self.restarted_at
            .map(|t| t.elapsed().as_secs() < 5)
            .unwrap_or(false)
    }

    pub fn is_waiting_for_requires(&self) -> bool {
        matches!(&self.health, Health::Degraded(m) if m.starts_with("Waiting for "))
            && self.spawn_cmd.is_some()
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

pub type State = Arc<Mutex<Vec<Svc>>>;

// ── Paths ─────────────────────────────────────────────────────────────────────

pub struct Paths {
    pub copilot: PathBuf,
    pub opencti: PathBuf,
    pub connector: PathBuf,
    pub openaev: PathBuf,
    pub grafana: PathBuf,
    pub langfuse: PathBuf,
}
