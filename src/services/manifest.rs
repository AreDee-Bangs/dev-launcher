use std::fs;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::services::docker::run_blocking;
use crate::tui::{DIM, GRN, R, YLW};

// ── Manifest data types ───────────────────────────────────────────────────────

#[derive(Default)]
pub struct ManifestDocker {
    pub compose_dev: Option<String>,
    pub project: Option<String>,
}

pub struct SvcDef {
    pub name: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub health: Option<String>,
    pub timeout_secs: u64,
    pub requires_docker: bool,
    pub log_name: Option<String>,
    pub requires: Vec<String>,
}

pub enum BootstrapDef {
    Check {
        path: String,
        missing_hint: String,
    },
    RunIfMissing {
        check: String,
        command: Vec<String>,
        cwd: Option<String>,
    },
    /// Keep a venv in sync with a requirements file.
    /// Computes SHA-256 of `requirements`, compares to `.launcher-reqs-hash`
    /// inside the venv directory, and runs `pip install -r <requirements>` when
    /// the hash differs or the sentinel is missing.
    SyncPip {
        requirements: String,
        pip: String,
    },
}

#[derive(Default)]
pub struct RepoManifest {
    pub docker: ManifestDocker,
    pub services: Vec<SvcDef>,
    pub bootstrap: Vec<BootstrapDef>,
    /// Required Python major.minor (e.g. "3.13"). When set, the launcher
    /// checks any venv Python binary against this before starting services.
    pub python_version: Option<String>,
}

// ── Manifest loading ──────────────────────────────────────────────────────────

pub fn parse_compose_project_name(compose_file: &Path) -> Option<String> {
    let content = fs::read_to_string(compose_file).ok()?;
    for line in content.lines() {
        if !line.starts_with("name:") {
            continue;
        }
        let val = line["name:".len()..]
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        if !val.is_empty() {
            return Some(val);
        }
    }
    None
}

// ── YAML serde structs (parse only) ──────────────────────────────────────────

#[derive(Deserialize)]
struct YamlConf {
    python: Option<YamlPython>,
    docker: Option<YamlDocker>,
    services: Option<Vec<YamlService>>,
    bootstrap: Option<Vec<YamlBootstrap>>,
}

#[derive(Deserialize)]
struct YamlPython {
    version: Option<String>,
}

#[derive(Deserialize)]
struct YamlDocker {
    compose_dev: Option<String>,
    project: Option<String>,
}

#[derive(Deserialize)]
struct YamlService {
    name: String,
    command: Option<String>,
    cwd: Option<String>,
    health: Option<String>,
    timeout: Option<u64>,
    requires_docker: Option<bool>,
    log: Option<String>,
    requires: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum YamlBootstrap {
    Check {
        check: String,
        missing: String,
    },
    SyncPip {
        requirements: String,
        pip: String,
    },
    RunIfMissing {
        run_if_missing: String,
        command: String,
        cwd: Option<String>,
    },
}

// ── Parse ─────────────────────────────────────────────────────────────────────

pub fn parse_dev_launcher_conf(path: &Path) -> Option<RepoManifest> {
    let content = fs::read_to_string(path).ok()?;

    // Detect old INI format by presence of [section] headers.
    let is_ini = content.lines().any(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#') && t.starts_with('[') && t.ends_with(']')
    });

    if is_ini {
        let manifest = parse_ini_content(&content)?;
        let repo_name = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        println!("  {YLW}Migrating .dev-launcher.conf to YAML format…{R}");
        save_dev_launcher_conf(path, repo_name, &manifest);
        return Some(manifest);
    }

    parse_yaml_content(&content)
}

fn parse_yaml_content(content: &str) -> Option<RepoManifest> {
    let conf: YamlConf = serde_yaml::from_str(content).ok()?;

    let docker = ManifestDocker {
        compose_dev: conf.docker.as_ref().and_then(|d| d.compose_dev.clone()),
        project: conf.docker.as_ref().and_then(|d| d.project.clone()),
    };

    let services = conf
        .services
        .unwrap_or_default()
        .into_iter()
        .map(|s| SvcDef {
            name: s.name,
            args: s
                .command
                .map(|c| c.split_whitespace().map(|t| t.to_string()).collect())
                .unwrap_or_default(),
            cwd: s.cwd.unwrap_or_default(),
            health: s.health,
            timeout_secs: s.timeout.unwrap_or(30),
            requires_docker: s.requires_docker.unwrap_or(false),
            log_name: s.log,
            requires: s.requires.unwrap_or_default(),
        })
        .collect();

    let bootstrap = conf
        .bootstrap
        .unwrap_or_default()
        .into_iter()
        .map(|b| match b {
            YamlBootstrap::Check { check, missing } => BootstrapDef::Check {
                path: check,
                missing_hint: missing,
            },
            YamlBootstrap::SyncPip { requirements, pip } => {
                BootstrapDef::SyncPip { requirements, pip }
            }
            YamlBootstrap::RunIfMissing {
                run_if_missing,
                command,
                cwd,
            } => BootstrapDef::RunIfMissing {
                check: run_if_missing,
                command: command.split_whitespace().map(|t| t.to_string()).collect(),
                cwd,
            },
        })
        .collect();

    Some(RepoManifest {
        docker,
        services,
        bootstrap,
        python_version: conf.python.and_then(|p| p.version),
    })
}

/// Parse the legacy INI-style `.dev-launcher.conf` format.
#[allow(unused_assignments)]
fn parse_ini_content(content: &str) -> Option<RepoManifest> {
    let mut docker = ManifestDocker::default();
    let mut services: Vec<SvcDef> = Vec::new();
    let mut bootstrap: Vec<BootstrapDef> = Vec::new();

    enum Section { None, Docker, Service, Bootstrap, Python }

    let mut section = Section::None;
    let mut python_version: Option<String> = None;
    let mut svc_name = String::new();
    let mut svc_args: Vec<String> = Vec::new();
    let mut svc_cwd = String::new();
    let mut svc_health: Option<String> = None;
    let mut svc_timeout: u64 = 30;
    let mut svc_req_docker = false;
    let mut svc_log: Option<String> = None;
    let mut svc_requires: Vec<String> = Vec::new();
    let mut bs_check = String::new();
    let mut bs_missing = String::new();
    let mut bs_run_if = String::new();
    let mut bs_command: Vec<String> = Vec::new();
    let mut bs_cwd: Option<String> = None;
    let mut bs_requirements = String::new();
    let mut bs_pip = String::new();

    macro_rules! flush_service {
        () => {{
            if !svc_name.is_empty() {
                services.push(SvcDef {
                    name: svc_name.clone(),
                    args: svc_args.clone(),
                    cwd: svc_cwd.clone(),
                    health: svc_health.clone(),
                    timeout_secs: svc_timeout,
                    requires_docker: svc_req_docker,
                    log_name: svc_log.clone(),
                    requires: svc_requires.clone(),
                });
                svc_name = String::new(); svc_args = Vec::new(); svc_cwd = String::new();
                svc_health = None; svc_timeout = 30; svc_req_docker = false;
                svc_log = None; svc_requires = Vec::new();
            }
        }};
    }

    macro_rules! flush_bootstrap {
        () => {{
            if !bs_check.is_empty() && !bs_missing.is_empty() {
                bootstrap.push(BootstrapDef::Check {
                    path: bs_check.clone(), missing_hint: bs_missing.clone(),
                });
            } else if !bs_run_if.is_empty() && !bs_command.is_empty() {
                bootstrap.push(BootstrapDef::RunIfMissing {
                    check: bs_run_if.clone(), command: bs_command.clone(), cwd: bs_cwd.clone(),
                });
            } else if !bs_requirements.is_empty() && !bs_pip.is_empty() {
                bootstrap.push(BootstrapDef::SyncPip {
                    requirements: bs_requirements.clone(), pip: bs_pip.clone(),
                });
            }
            bs_check = String::new(); bs_missing = String::new(); bs_run_if = String::new();
            bs_command = Vec::new(); bs_cwd = None;
            bs_requirements = String::new(); bs_pip = String::new();
        }};
    }

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { continue; }

        if line.starts_with('[') && line.ends_with(']') {
            match section {
                Section::Service   => flush_service!(),
                Section::Bootstrap => flush_bootstrap!(),
                _ => {}
            }
            let inner = line[1..line.len() - 1].trim();
            section = if inner == "docker" { Section::Docker }
                else if inner == "bootstrap" { Section::Bootstrap }
                else if inner == "python"    { Section::Python }
                else if let Some(rest) = inner.strip_prefix("service ") {
                    svc_name = rest.trim().to_string();
                    Section::Service
                } else { Section::None };
            continue;
        }

        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim(); let v = v.trim().to_string();
            match section {
                Section::Docker => match k {
                    "compose_dev" => docker.compose_dev = Some(v),
                    "project"     => docker.project = Some(v),
                    _ => {}
                },
                Section::Service => match k {
                    "command"        => svc_args = v.split_whitespace().map(|s| s.to_string()).collect(),
                    "cwd"            => svc_cwd = v,
                    "health"         => svc_health = if v.is_empty() { None } else { Some(v) },
                    "timeout"        => svc_timeout = v.parse().unwrap_or(30),
                    "requires_docker"=> svc_req_docker = matches!(v.as_str(), "true"|"1"|"yes"),
                    "log"            => svc_log = if v.is_empty() { None } else { Some(v) },
                    "requires"       => svc_requires = v.split_whitespace().map(|s| s.to_string()).collect(),
                    _ => {}
                },
                Section::Bootstrap => match k {
                    "check"          => bs_check = v,
                    "missing"        => bs_missing = v,
                    "run_if_missing" => bs_run_if = v,
                    "command"        => bs_command = v.split_whitespace().map(|s| s.to_string()).collect(),
                    "cwd"            => bs_cwd = if v.is_empty() { None } else { Some(v) },
                    "requirements"   => bs_requirements = v,
                    "pip"            => bs_pip = v,
                    _ => {}
                },
                Section::Python => if k == "version" {
                    python_version = if v.is_empty() { None } else { Some(v) };
                },
                _ => {}
            }
        }
    }

    match section {
        Section::Service   => flush_service!(),
        Section::Bootstrap => flush_bootstrap!(),
        _ => {}
    }

    Some(RepoManifest { docker, services, bootstrap, python_version })
}

pub fn infer_repo_manifest(repo_dir: &Path) -> RepoManifest {
    let mut docker = ManifestDocker::default();
    let mut services: Vec<SvcDef> = Vec::new();
    let mut bootstrap: Vec<BootstrapDef> = Vec::new();

    let compose_file = repo_dir.join("docker-compose.dev.yml");
    if compose_file.exists() {
        docker.compose_dev = Some("docker-compose.dev.yml".to_string());
        docker.project = parse_compose_project_name(&compose_file).or_else(|| {
            repo_dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| format!("{n}-dev"))
        });
    }

    let backend_dir = repo_dir.join("backend");
    let python = backend_dir.join(".venv/bin/python");
    if backend_dir.is_dir() && backend_dir.join("app/main.py").exists() {
        services.push(SvcDef {
            name: "backend".to_string(),
            args: vec![
                ".venv/bin/python".to_string(),
                "-m".to_string(),
                "uvicorn".to_string(),
                "app.main:application".to_string(),
                "--reload".to_string(),
                "--host".to_string(),
                "0.0.0.0".to_string(),
                "--port".to_string(),
                "8100".to_string(),
                "--timeout-graceful-shutdown".to_string(),
                "3".to_string(),
            ],
            cwd: "backend".to_string(),
            health: Some("http://localhost:8100/api/health".to_string()),
            timeout_secs: 120,
            requires_docker: true,
            log_name: None,
            requires: Vec::new(),
        });
        services.push(SvcDef {
            name: "worker".to_string(),
            args: vec![
                ".venv/bin/python".to_string(),
                "-m".to_string(),
                "saq".to_string(),
                "app.worker.settings".to_string(),
            ],
            cwd: "backend".to_string(),
            health: None,
            timeout_secs: 10,
            requires_docker: true,
            log_name: Some("copilot-worker.log".to_string()),
            requires: Vec::new(),
        });
        bootstrap.push(BootstrapDef::Check {
            path: "backend/.venv/bin/python".to_string(),
            missing_hint: "Run ./dev.sh once to create the Python venv".to_string(),
        });
        // Keep the venv in sync with requirements.txt automatically.
        let reqs = backend_dir.join("requirements.txt");
        if reqs.exists() {
            bootstrap.push(BootstrapDef::SyncPip {
                requirements: "backend/requirements.txt".to_string(),
                pip: "backend/.venv/bin/pip".to_string(),
            });
        }
        let _ = python;
    }

    let frontend_dir = repo_dir.join("frontend");
    if frontend_dir.join("package.json").exists() {
        services.push(SvcDef {
            name: "frontend".to_string(),
            args: vec!["yarn".to_string(), "dev".to_string()],
            cwd: "frontend".to_string(),
            health: Some("http://localhost:3100".to_string()),
            timeout_secs: 90,
            requires_docker: false,
            log_name: None,
            requires: Vec::new(),
        });
        bootstrap.push(BootstrapDef::RunIfMissing {
            check: "frontend/node_modules".to_string(),
            command: vec!["yarn".to_string(), "install".to_string()],
            cwd: Some("frontend".to_string()),
        });
    }

    RepoManifest {
        docker,
        services,
        bootstrap,
        python_version: detect_python_version(repo_dir),
    }
}

/// Detect the required Python major.minor for a repo by inspecting:
/// 1. `.python-version` file (pyenv / mise convention)
/// 2. `backend/pyproject.toml` — `requires-python` or ruff `target-version`
fn detect_python_version(repo_dir: &Path) -> Option<String> {
    // .python-version: first line is e.g. "3.13.2" or "3.13"
    if let Ok(content) = fs::read_to_string(repo_dir.join(".python-version")) {
        let line = content.lines().next().unwrap_or("").trim().to_string();
        if !line.is_empty() {
            return Some(major_minor(&line));
        }
    }
    // backend/pyproject.toml
    let ppt = repo_dir.join("backend/pyproject.toml");
    if let Ok(content) = fs::read_to_string(&ppt) {
        for line in content.lines() {
            let t = line.trim();
            // requires-python = ">=3.13" or "==3.13.*"
            if let Some(rest) = t.strip_prefix("requires-python") {
                let v = rest.trim_start_matches([' ', '=', '"', '\'', '>', '<', '~', '^', '!']);
                let v = v.trim_matches(['"', '\'', ' ']);
                if !v.is_empty() {
                    return Some(major_minor(v));
                }
            }
            // ruff target-version = "py313"
            if let Some(rest) = t.strip_prefix("target-version") {
                let v = rest.trim_start_matches([' ', '=', '"', '\''])
                    .trim_matches(['"', '\'', ' ']);
                if let Some(pyver) = v.strip_prefix("py") {
                    if pyver.len() >= 3 {
                        let (major, minor) = pyver.split_at(1);
                        return Some(format!("{}.{}", major, minor));
                    }
                }
            }
        }
    }
    None
}

fn major_minor(v: &str) -> String {
    let parts: Vec<&str> = v.splitn(3, '.').collect();
    match parts.as_slice() {
        [major, minor, ..] => format!("{}.{}", major, minor),
        [major] => major.to_string(),
        _ => v.to_string(),
    }
}

pub fn save_dev_launcher_conf(conf_path: &Path, repo_name: &str, manifest: &RepoManifest) {
    let mut out = format!("# {} — dev-launcher configuration\n", repo_name);
    out.push_str("# Auto-generated. Edit to customise. Re-run dev-launcher to apply changes.\n\n");

    if let Some(ref v) = manifest.python_version {
        out.push_str("python:\n");
        out.push_str(&format!("  version: {}\n\n", ys(v)));
    }

    if manifest.docker.compose_dev.is_some() || manifest.docker.project.is_some() {
        out.push_str("docker:\n");
        if let Some(ref cd) = manifest.docker.compose_dev {
            out.push_str(&format!("  compose_dev: {}\n", ys(cd)));
        }
        if let Some(ref p) = manifest.docker.project {
            out.push_str(&format!("  project: {}\n", ys(p)));
        }
        out.push('\n');
    }

    if !manifest.services.is_empty() {
        out.push_str("services:\n");
        for svc in &manifest.services {
            out.push_str(&format!("  - name: {}\n", ys(&svc.name)));
            if !svc.args.is_empty() {
                out.push_str(&format!("    command: {}\n", ys(&svc.args.join(" "))));
            }
            if !svc.cwd.is_empty() {
                out.push_str(&format!("    cwd: {}\n", ys(&svc.cwd)));
            }
            if let Some(ref h) = svc.health {
                out.push_str(&format!("    health: {}\n", ys(h)));
            }
            out.push_str(&format!("    timeout: {}\n", svc.timeout_secs));
            if svc.requires_docker {
                out.push_str("    requires_docker: true\n");
            }
            if let Some(ref l) = svc.log_name {
                out.push_str(&format!("    log: {}\n", ys(l)));
            }
            if !svc.requires.is_empty() {
                out.push_str("    requires:\n");
                for r in &svc.requires {
                    out.push_str(&format!("      - {}\n", ys(r)));
                }
            }
        }
        out.push('\n');
    }

    if !manifest.bootstrap.is_empty() {
        out.push_str("bootstrap:\n");
        for step in &manifest.bootstrap {
            match step {
                BootstrapDef::Check { path, missing_hint } => {
                    out.push_str(&format!("  - check: {}\n", ys(path)));
                    out.push_str(&format!("    missing: {}\n", ys(missing_hint)));
                }
                BootstrapDef::RunIfMissing { check, command, cwd } => {
                    out.push_str(&format!("  - run_if_missing: {}\n", ys(check)));
                    out.push_str(&format!("    command: {}\n", ys(&command.join(" "))));
                    if let Some(ref c) = cwd {
                        out.push_str(&format!("    cwd: {}\n", ys(c)));
                    }
                }
                BootstrapDef::SyncPip { requirements, pip } => {
                    out.push_str(&format!("  - requirements: {}\n", ys(requirements)));
                    out.push_str(&format!("    pip: {}\n", ys(pip)));
                }
            }
        }
        out.push('\n');
    }

    let _ = fs::write(conf_path, out);
}

/// Quote a YAML scalar value when it contains characters that could be
/// misinterpreted by a YAML parser (colons, hashes, brackets, etc.) or
/// that look like a YAML keyword / bare number.
fn ys(s: &str) -> String {
    let needs_quoting = s.is_empty()
        || s.contains(": ")
        || s.starts_with('[')
        || s.starts_with('{')
        || s.starts_with('*')
        || s.starts_with('&')
        || s.starts_with('!')
        || s.starts_with('"')
        || s.starts_with('\'')
        || s.starts_with('#')
        || matches!(s, "true" | "false" | "null" | "yes" | "no")
        || s.parse::<f64>().is_ok();
    if needs_quoting {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

pub fn load_repo_manifest(repo_dir: &Path, repo_name: &str) -> RepoManifest {
    let conf_path = repo_dir.join(".dev-launcher.conf");
    if conf_path.exists() {
        if let Some(m) = parse_dev_launcher_conf(&conf_path) {
            return m;
        }
    }
    let manifest = infer_repo_manifest(repo_dir);
    if !manifest.services.is_empty() {
        println!("  {DIM}Auto-generating .dev-launcher.conf for {repo_name}…{R}");
        save_dev_launcher_conf(&conf_path, repo_name, &manifest);
    }
    manifest
}

/// Patch in-memory manifest port numbers to match the workspace env's BASE_URL / FRONTEND_URL.
pub fn patch_manifest_ports(manifest: &mut RepoManifest, backend_port: u16, frontend_port: u16) {
    const DEFAULT_BACKEND: u16 = 8100;
    const DEFAULT_FRONTEND: u16 = 3100;
    for svc in &mut manifest.services {
        let (from, to) = match svc.name.as_str() {
            "backend" => (DEFAULT_BACKEND, backend_port),
            "frontend" => (DEFAULT_FRONTEND, frontend_port),
            _ => continue,
        };
        if from == to {
            continue;
        }
        if let Some(ref mut h) = svc.health {
            *h = h.replace(&format!(":{from}"), &format!(":{to}"));
        }
        for arg in &mut svc.args {
            if arg == &from.to_string() {
                *arg = to.to_string();
            }
        }
    }
}

/// Compute a simple SHA-256 hex digest of a file's contents.
fn file_sha256(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    // FNV-1a — stable across Rust versions, collision-resistant enough for
    // drift detection (not cryptographic, but sufficient here).
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in &data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(format!("{:016x}-{}", hash, data.len()))
}

/// Run `python --version` and return the major.minor string (e.g. "3.14").
fn venv_python_version(python_bin: &Path) -> Option<String> {
    let out = Command::new(python_bin)
        .arg("--version")
        .output()
        .ok()?;
    // output is on stdout for Python 3, e.g. "Python 3.14.2\n"
    let raw = String::from_utf8(out.stdout).ok()?;
    let version_str = raw.trim().strip_prefix("Python ")?;
    Some(major_minor(version_str))
}

pub fn run_manifest_bootstrap(repo_dir: &Path, manifest: &RepoManifest) -> bool {
    let mut ok = true;
    for step in &manifest.bootstrap {
        match step {
            BootstrapDef::Check { path, missing_hint } => {
                let full = repo_dir.join(path);
                if !full.exists() {
                    println!("  {YLW}⚠{R}  {missing_hint}");
                    ok = false;
                    continue;
                }
                // If this is a Python binary and a required version is set, verify it.
                if let Some(ref required) = manifest.python_version {
                    if path.contains("python") {
                        if let Some(actual) = venv_python_version(&full) {
                            if !actual.starts_with(required.as_str()) {
                                println!(
                                    "  {YLW}⚠{R}  venv uses Python {actual} but Python {required} is required"
                                );
                                println!(
                                    "  {DIM}    → delete {path} directory and re-run ./dev.sh{R}"
                                );
                                ok = false;
                            }
                        }
                    }
                }
            }
            BootstrapDef::RunIfMissing {
                check,
                command,
                cwd,
            } => {
                if !repo_dir.join(check).exists() {
                    let work_dir = cwd
                        .as_deref()
                        .map(|c| repo_dir.join(c))
                        .unwrap_or_else(|| repo_dir.to_owned());
                    if let Some((prog, args)) = command.split_first() {
                        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                        run_blocking(prog, &args_ref, &work_dir);
                    }
                }
            }
            BootstrapDef::SyncPip { requirements, pip } => {
                let reqs_path = repo_dir.join(requirements);
                let pip_path  = repo_dir.join(pip);
                if !reqs_path.exists() || !pip_path.exists() {
                    continue;
                }
                // Sentinel lives inside the venv directory alongside the pip binary.
                let sentinel = pip_path.parent()
                    .map(|p| p.join(".launcher-reqs-hash"))
                    .unwrap_or_else(|| repo_dir.join(".launcher-reqs-hash"));

                let current_hash = file_sha256(&reqs_path);
                let stored_hash  = fs::read_to_string(&sentinel).ok();

                if current_hash.as_deref() != stored_hash.as_deref() {
                    println!("  {DIM}requirements.txt changed — running pip install…{R}");
                    let pip_str  = pip_path.to_string_lossy().into_owned();
                    let reqs_str = reqs_path.to_string_lossy().into_owned();
                    let code = run_blocking(&pip_str, &["install", "-q", "-r", &reqs_str], repo_dir);
                    if code == 0 {
                        if let Some(ref h) = current_hash {
                            let _ = fs::write(&sentinel, h);
                        }
                        println!("  {GRN}✓{R}  pip install done");
                    } else {
                        println!("  {YLW}⚠{R}  pip install failed (exit {code})");
                        ok = false;
                    }
                }
            }
        }
    }
    ok
}

pub fn split_health_url_parts(full: Option<&str>) -> (Option<String>, String) {
    match full {
        None => (None, String::new()),
        Some(url) => {
            if let Some(pos) = url.find("://") {
                let after = &url[pos + 3..];
                if let Some(slash) = after.find('/') {
                    (
                        Some(url[..pos + 3 + slash].to_string()),
                        url[pos + 3 + slash..].to_string(),
                    )
                } else {
                    (Some(url.to_string()), String::new())
                }
            } else {
                (Some(url.to_string()), String::new())
            }
        }
    }
}

/// Derive the base Docker project name for a repo (without workspace suffix).
pub fn resolve_docker_project_base(repo_dir: &Path, manifest: &RepoManifest) -> String {
    if let Some(p) = &manifest.docker.project {
        return p.clone();
    }
    let compose_file = manifest
        .docker
        .compose_dev
        .as_deref()
        .unwrap_or("docker-compose.dev.yml");
    if let Some(name) = parse_compose_project_name(&repo_dir.join(compose_file)) {
        return name;
    }
    repo_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| format!("{n}-dev"))
        .unwrap_or_else(|| "dev".to_string())
}

pub fn resolve_docker_project(repo_dir: &Path, manifest: &RepoManifest, ws_hash: &str) -> String {
    use crate::services::docker::ws_docker_project;
    ws_docker_project(&resolve_docker_project_base(repo_dir, manifest), ws_hash)
}

/// Read POSTGRES_PASSWORD from a docker-compose YAML file.
pub fn read_compose_postgres_password(compose_file: &Path) -> Option<String> {
    let content = fs::read_to_string(compose_file).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("POSTGRES_PASSWORD:") {
            let val = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

/// Ensure OpenCTI graphql's Python deps are installed.
pub fn ensure_opencti_graphql_python_deps(dir: &Path) -> Option<String> {
    let venv = dir.join(".python-venv");
    let venv_python = venv.join("bin/python3");
    let reqs = dir.join("src/python/requirements.txt");
    if !reqs.exists() {
        return None;
    }

    if !venv_python.exists() {
        println!("  Creating OpenCTI graphql Python venv…");
        let ok = run_blocking("python3", &["-m", "venv", venv.to_str().unwrap()], dir);
        if ok != 0 {
            println!("  {YLW}Could not create Python venv — opencti-graphql may fail.{R}");
            return None;
        }
    }

    let already = Command::new(venv_python.to_str().unwrap())
        .args(["-c", "import eql"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !already {
        println!("  Installing OpenCTI graphql Python deps (eql, yara, pycti…)");
        let pip = venv.join("bin/pip3").to_string_lossy().into_owned();
        run_blocking(&pip, &["install", "-q", "-r", reqs.to_str().unwrap()], dir);
    }

    let site_packages = Command::new(venv_python.to_str().unwrap())
        .args(["-c", "import site; print(site.getsitepackages()[0])"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    site_packages
}

/// Ensure OpenCTI graphql .env.dev exists.
pub fn ensure_opencti_env(gql_dir: &Path) {
    let path = gql_dir.join(".env.dev");
    if !path.exists() {
        let _ = fs::write(
            &path,
            "\
# OpenCTI graphql dev environment — generated by dev-launcher\n\
# Leave TOKEN and ENCRYPTION_KEY as ChangeMe; the wizard will auto-generate them.\n\
APP__ADMIN__EMAIL=admin@opencti.io\n\
APP__ADMIN__PASSWORD=ChangeMe\n\
APP__ADMIN__TOKEN=ChangeMe\n\
APP__ENCRYPTION_KEY=ChangeMe\n\
",
        );
    }
}
