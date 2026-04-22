use std::fs;
use std::path::Path;
use std::process::Command;

use crate::services::docker::run_blocking;
use crate::tui::{DIM, R, YLW};

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
}

#[derive(Default)]
pub struct RepoManifest {
    pub docker: ManifestDocker,
    pub services: Vec<SvcDef>,
    pub bootstrap: Vec<BootstrapDef>,
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

pub fn parse_dev_launcher_conf(path: &Path) -> Option<RepoManifest> {
    let content = fs::read_to_string(path).ok()?;
    let mut docker = ManifestDocker::default();
    let mut services: Vec<SvcDef> = Vec::new();
    let mut bootstrap: Vec<BootstrapDef> = Vec::new();

    enum Section {
        None,
        Docker,
        Service,
        Bootstrap,
    }

    let mut section = Section::None;
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

    let flush_service = |name: &str,
                         args: &Vec<String>,
                         cwd: &str,
                         health: &Option<String>,
                         timeout: u64,
                         req_docker: bool,
                         log: &Option<String>,
                         requires: &Vec<String>,
                         svcs: &mut Vec<SvcDef>| {
        if !name.is_empty() {
            svcs.push(SvcDef {
                name: name.to_string(),
                args: args.clone(),
                cwd: cwd.to_string(),
                health: health.clone(),
                timeout_secs: timeout,
                requires_docker: req_docker,
                log_name: log.clone(),
                requires: requires.clone(),
            });
        }
    };

    let flush_bootstrap = |check: &str,
                           missing: &str,
                           run_if: &str,
                           command: &Vec<String>,
                           cwd: &Option<String>,
                           bootstrap: &mut Vec<BootstrapDef>| {
        if !check.is_empty() && !missing.is_empty() {
            bootstrap.push(BootstrapDef::Check {
                path: check.to_string(),
                missing_hint: missing.to_string(),
            });
        } else if !run_if.is_empty() && !command.is_empty() {
            bootstrap.push(BootstrapDef::RunIfMissing {
                check: run_if.to_string(),
                command: command.clone(),
                cwd: cwd.clone(),
            });
        }
    };

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            match &section {
                Section::Service => {
                    flush_service(
                        &svc_name,
                        &svc_args,
                        &svc_cwd,
                        &svc_health,
                        svc_timeout,
                        svc_req_docker,
                        &svc_log,
                        &svc_requires,
                        &mut services,
                    );
                    svc_name = String::new();
                    svc_args = Vec::new();
                    svc_cwd = String::new();
                    svc_health = None;
                    svc_timeout = 30;
                    svc_req_docker = false;
                    svc_log = None;
                    svc_requires = Vec::new();
                }
                Section::Bootstrap => {
                    flush_bootstrap(
                        &bs_check,
                        &bs_missing,
                        &bs_run_if,
                        &bs_command,
                        &bs_cwd,
                        &mut bootstrap,
                    );
                    bs_check = String::new();
                    bs_missing = String::new();
                    bs_run_if = String::new();
                    bs_command = Vec::new();
                    bs_cwd = None;
                }
                _ => {}
            }

            let inner = line[1..line.len() - 1].trim();
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
                    "project" => docker.project = Some(v),
                    _ => {}
                },
                Section::Service => match k {
                    "command" => svc_args = v.split_whitespace().map(|s| s.to_string()).collect(),
                    "cwd" => svc_cwd = v,
                    "health" => svc_health = if v.is_empty() { None } else { Some(v) },
                    "timeout" => svc_timeout = v.parse().unwrap_or(30),
                    "requires_docker" => {
                        svc_req_docker = matches!(v.as_str(), "true" | "1" | "yes")
                    }
                    "log" => svc_log = if v.is_empty() { None } else { Some(v) },
                    "requires" => {
                        svc_requires = v.split_whitespace().map(|s| s.to_string()).collect()
                    }
                    _ => {}
                },
                Section::Bootstrap => match k {
                    "check" => bs_check = v,
                    "missing" => bs_missing = v,
                    "run_if_missing" => bs_run_if = v,
                    "command" => bs_command = v.split_whitespace().map(|s| s.to_string()).collect(),
                    "cwd" => bs_cwd = if v.is_empty() { None } else { Some(v) },
                    _ => {}
                },
                _ => {}
            }
        }
    }

    match &section {
        Section::Service => {
            flush_service(
                &svc_name,
                &svc_args,
                &svc_cwd,
                &svc_health,
                svc_timeout,
                svc_req_docker,
                &svc_log,
                &svc_requires,
                &mut services,
            );
        }
        Section::Bootstrap => {
            flush_bootstrap(
                &bs_check,
                &bs_missing,
                &bs_run_if,
                &bs_command,
                &bs_cwd,
                &mut bootstrap,
            );
        }
        _ => {}
    }

    Some(RepoManifest {
        docker,
        services,
        bootstrap,
    })
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
    }
}

pub fn save_dev_launcher_conf(conf_path: &Path, repo_name: &str, manifest: &RepoManifest) {
    let mut out = format!("# {} — dev-launcher launcher configuration\n", repo_name);
    out.push_str("# Auto-generated. Edit to customize. Re-run dev-launcher to apply changes.\n\n");

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
            BootstrapDef::RunIfMissing {
                check,
                command,
                cwd,
            } => {
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

pub fn run_manifest_bootstrap(repo_dir: &Path, manifest: &RepoManifest) -> bool {
    let mut ok = true;
    for step in &manifest.bootstrap {
        match step {
            BootstrapDef::Check { path, missing_hint } => {
                if !repo_dir.join(path).exists() {
                    println!("  {YLW}⚠{R}  {missing_hint}");
                    ok = false;
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
