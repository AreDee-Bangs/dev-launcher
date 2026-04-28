use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::tui::{DIM, GRN, R, RED};

// ── Workspace-scoped project name ─────────────────────────────────────────────

/// Build a workspace-scoped Docker project name: `{base}-{ws_hash[..8]}`.
pub fn ws_docker_project(base: &str, ws_hash: &str) -> String {
    format!("{}-{}", base, &ws_hash[..8.min(ws_hash.len())])
}

// ── Container-name discovery ───────────────────────────────────────────────────

/// Parse a docker-compose file and return `(service_name, container_name)` pairs
/// for every service that has an explicit `container_name:` directive.
pub fn parse_compose_container_names(compose_file: &Path) -> Vec<(String, String)> {
    let content = match fs::read_to_string(compose_file) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut result = Vec::new();
    let mut in_svcs = false;
    let mut cur_svc = String::new();

    for line in content.lines() {
        if line == "services:" {
            in_svcs = true;
            continue;
        }
        if !in_svcs {
            continue;
        }
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

/// Write a compose override file next to `compose_file` that:
/// - Appends `{ws_hash[..8]}` to every explicit `container_name:` to prevent
///   collisions between workspaces.
/// - When `port_offset > 0`, also remaps every host port by adding `port_offset`
///   so that Docker services from two concurrent workspaces bind different host
///   ports.
///
/// Returns `None` if there is nothing to override.
pub fn write_compose_override(
    compose_file: &Path,
    ws_hash: &str,
    port_offset: u16,
) -> Option<PathBuf> {
    let name_mappings = parse_compose_container_names(compose_file);
    let port_bindings = if port_offset > 0 {
        parse_compose_port_bindings(compose_file)
    } else {
        Vec::new()
    };

    if name_mappings.is_empty() && port_bindings.is_empty() {
        return None;
    }

    let suffix = &ws_hash[..8.min(ws_hash.len())];
    let dir = compose_file.parent().unwrap_or(Path::new("."));
    let out_path = dir.join("docker-compose.override-devlauncher.yml");

    // Collect all service names that need an entry (union of both lists).
    let mut seen = std::collections::HashSet::new();
    let mut all_svcs: Vec<String> = Vec::new();
    for (s, _) in &name_mappings {
        if seen.insert(s.clone()) {
            all_svcs.push(s.clone());
        }
    }
    for (s, _, _) in &port_bindings {
        if seen.insert(s.clone()) {
            all_svcs.push(s.clone());
        }
    }

    let mut lines = vec!["services:".to_string()];
    for svc in &all_svcs {
        lines.push(format!("  {}:", svc));
        if let Some((_, cn)) = name_mappings.iter().find(|(s, _)| s == svc) {
            lines.push(format!("    container_name: {cn}-{suffix}"));
        }
        let svc_ports: Vec<_> = port_bindings
            .iter()
            .filter(|(s, _, _)| s == svc)
            .collect();
        if !svc_ports.is_empty() {
            lines.push("    ports:".to_string());
            for (_, host_port, cont_port) in &svc_ports {
                let new_host = host_port.saturating_add(port_offset);
                lines.push(format!("      - \"{new_host}:{cont_port}\""));
            }
        }
    }

    fs::write(&out_path, lines.join("\n") + "\n").ok()?;
    ensure_gitignore_entries(dir, &["docker-compose.override-devlauncher.yml"]);
    Some(out_path)
}

/// Parse every `host:container` port binding in a docker-compose YAML file.
/// Returns `(service_name, host_port, container_port)` triples.
fn parse_compose_port_bindings(compose_file: &Path) -> Vec<(String, u16, u16)> {
    let content = match fs::read_to_string(compose_file) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut result = Vec::new();
    let mut in_svcs = false;
    let mut cur_svc = String::new();
    let mut in_ports = false;

    for line in content.lines() {
        if line == "services:" {
            in_svcs = true;
            continue;
        }
        if !in_svcs {
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let indent = line.len() - line.trim_start().len();

        // Service name at 2-space indent
        if indent == 2 {
            if let Some(svc) = trimmed.strip_suffix(':') {
                if !svc.is_empty() && !svc.contains(' ') {
                    cur_svc = svc.to_string();
                }
            }
            in_ports = false;
            continue;
        }

        // Property keys at 4-space indent
        if indent == 4 {
            in_ports = trimmed == "ports:";
            continue;
        }

        // Port list entries at 6-space indent
        if in_ports && indent == 6 && trimmed.starts_with('-') {
            let entry = trimmed
                .trim_start_matches('-')
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if let Some(pos) = entry.find(':') {
                let host_s = entry[..pos].trim();
                let cont_s = entry[pos + 1..].trim();
                if let (Ok(h), Ok(c)) = (host_s.parse::<u16>(), cont_s.parse::<u16>()) {
                    result.push((cur_svc.clone(), h, c));
                }
            }
        }
    }
    result
}

/// Append `patterns` to `<dir>/.gitignore` if not already present.
pub fn ensure_gitignore_entries(dir: &Path, patterns: &[&str]) {
    let gitignore = dir.join(".gitignore");
    let existing = fs::read_to_string(&gitignore).unwrap_or_default();
    let to_add: Vec<&&str> = patterns
        .iter()
        .filter(|&&p| !existing.lines().any(|l| l.trim() == p))
        .collect();
    if to_add.is_empty() {
        return;
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("\n# dev-launcher\n");
    for &&p in &to_add {
        content.push_str(p);
        content.push('\n');
    }
    let _ = fs::write(&gitignore, content);
}

/// Stop and remove any containers whose name contains `name_fragment`.
pub fn docker_kill_by_name_fragment(name_fragment: &str) {
    let out = Command::new("docker")
        .args([
            "ps",
            "-a",
            "-q",
            "--filter",
            &format!("name={name_fragment}"),
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let ids: Vec<String> = out
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
                .collect()
        })
        .unwrap_or_default();
    for id in &ids {
        let _ = Command::new("docker")
            .args(["rm", "-f", id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

// ── Blocking process helpers ───────────────────────────────────────────────────

pub fn run_blocking(program: &str, args: &[&str], dir: &Path) -> i32 {
    crate::launcher_log::log(&format!("[CMD] {} {}", program, args.join(" ")));
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) => {
            crate::launcher_log::log_output(&o.stdout, &o.stderr);
            let code = o.status.code().unwrap_or(-1);
            crate::launcher_log::log(&format!("[CMD] exit {code}"));
            code
        }
        Err(e) => {
            crate::launcher_log::log(&format!("[CMD] failed to start: {e}"));
            -1
        }
    }
}

/// Like `run_blocking` but prints the full command, working directory, and exit code.
pub fn run_blocking_logged(program: &str, args: &[&str], dir: &Path) -> i32 {
    crate::launcher_log::log(&format!("[CMD] {} {} (cwd: {})", program, args.join(" "), dir.display()));
    println!("    {DIM}$ {program} {args}{R}", args = args.join(" "));
    println!("    {DIM}  cwd: {}{R}", dir.display());
    let _ = io::stdout().flush();
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) => {
            crate::launcher_log::log_output(&o.stdout, &o.stderr);
            let code = o.status.code().unwrap_or(-1);
            println!("    {DIM}  exit: {code}{R}");
            crate::launcher_log::log(&format!("[CMD] exit {code}"));
            code
        }
        Err(e) => {
            crate::launcher_log::log(&format!("[CMD] failed to start: {e}"));
            -1
        }
    }
}

// ── Docker availability ────────────────────────────────────────────────────────

/// Returns true when the Docker daemon is reachable.
pub fn docker_available() -> bool {
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
pub fn docker_compose_running_count(project: &str) -> usize {
    let out = Command::new("docker")
        .args([
            "compose",
            "-p",
            project,
            "ps",
            "--services",
            "--filter",
            "status=running",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Check whether any Docker containers for a workspace are still running.
/// Probes the five well-known base project names derived from the workspace hash.
pub fn docker_running_for_workspace(hash: &str) -> bool {
    let suffix = &hash[..8.min(hash.len())];
    for base in &["copilot-dev", "opencti-dev", "openaev-dev", "grafana-dev", "langfuse-dev"] {
        if docker_compose_running_count(&format!("{base}-{suffix}")) > 0 {
            return true;
        }
    }
    false
}

/// Stop all Docker containers for a workspace using only the project label.
/// Docker Compose v2 finds containers via the label, so no compose file is required.
pub fn docker_down_workspace(hash: &str) {
    let suffix = &hash[..8.min(hash.len())];
    for base in &["copilot-dev", "opencti-dev", "openaev-dev", "grafana-dev", "langfuse-dev"] {
        let project = format!("{base}-{suffix}");
        if docker_compose_running_count(&project) == 0 {
            continue;
        }
        print!("  Stopping {base} containers…\r\n");
        let _ = io::stdout().flush();
        let code = run_blocking("docker", &["compose", "-p", &project, "down"], Path::new("/tmp"));
        if code == 0 {
            print!("  {GRN}✓{R}  {base} containers stopped.\r\n");
        } else {
            print!("  {RED}✗{R}  docker down for {base} failed (exit {code}).\r\n");
        }
        let _ = io::stdout().flush();
    }
}

// ── DockerProject ─────────────────────────────────────────────────────────────

/// A Docker Compose project that was brought up by this session.
#[derive(Clone)]
pub struct DockerProject {
    pub label: String,
    pub project: String,
    pub compose_file: PathBuf,
    pub work_dir: PathBuf,
    pub override_file: Option<PathBuf>,
}

/// Run `docker compose -p <project> down`.
///
/// We intentionally omit `-f` here: Docker Compose v2 locates containers via
/// the `com.docker.compose.project` label, so the project name alone is
/// sufficient.  Passing `-f` would require the override file (written to /tmp
/// at startup) to still exist, which is not guaranteed across reboots or
/// session restarts.
pub fn docker_compose_down(dp: &DockerProject) {
    print!("  Stopping {} containers…\r\n", dp.label);
    let _ = io::stdout().flush();
    let argv: Vec<&str> = vec!["compose", "-p", &dp.project, "down"];
    let code = run_blocking("docker", &argv, &dp.work_dir);
    if code == 0 {
        print!("  {GRN}✓{R}  {} containers stopped.\r\n", dp.label);
    } else {
        print!(
            "  {RED}✗{R}  {} docker down failed (exit {code}).\r\n",
            dp.label
        );
    }
    let _ = io::stdout().flush();
}

/// Run `docker compose -p <project> -f <file> up -d [extra…]`.
pub fn docker_compose_up(
    label: &str,
    project: &str,
    compose_file: &Path,
    work_dir: &Path,
    extra: &[&str],
) -> bool {
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
            println!(
                "  {GRN}✓{R}  {label} docker deps started ({started} new, {running_after} total)"
            );
        }
        true
    } else {
        let label_already_up = Command::new("docker")
            .args([
                "ps",
                "-q",
                "--filter",
                &format!("label=com.docker.compose.project={project}"),
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
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

// ── Resolve docker project for teardown ───────────────────────────────────────

pub fn resolve_product_docker_for_down(
    repo: &str,
    repo_dir: &Path,
    ws_hash: &str,
) -> Option<(String, String, PathBuf)> {
    use crate::services::manifest::{parse_compose_project_name, parse_dev_launcher_conf};

    if repo == "connectors" {
        return None;
    }

    if repo == "opencti" {
        let compose = repo_dir.join("opencti-platform/opencti-dev/docker-compose.yml");
        let base = "opencti-dev".to_string();
        let ws_proj = ws_docker_project(&base, ws_hash);
        return Some((ws_proj, base, compose));
    }

    if repo == "openaev" {
        let dev_dir = repo_dir.join("openaev-dev");
        let compose = dev_dir.join("docker-compose.yml");
        let conf =
            parse_dev_launcher_conf(&repo_dir.join(".dev-launcher.conf")).unwrap_or_default();
        let base = conf
            .docker
            .project
            .unwrap_or_else(|| "openaev-dev".to_string());
        let ws_proj = ws_docker_project(&base, ws_hash);
        return Some((ws_proj, base, compose));
    }

    let conf_path = repo_dir.join(".dev-launcher.conf");
    let manifest = parse_dev_launcher_conf(&conf_path).unwrap_or_default();

    let compose_name = manifest
        .docker
        .compose_dev
        .as_deref()
        .unwrap_or("docker-compose.dev.yml");
    let compose_file = repo_dir.join(compose_name);

    let base = if let Some(p) = manifest.docker.project {
        p
    } else if let Some(name) = parse_compose_project_name(&compose_file) {
        name
    } else {
        repo_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| format!("{n}-dev"))
            .unwrap_or_else(|| "dev".to_string())
    };

    let ws_proj = ws_docker_project(&base, ws_hash);
    Some((ws_proj, base, compose_file))
}

// ── OpenSearch / Elasticsearch readiness ─────────────────────────────────────

/// Return `true` if Elasticsearch/OpenSearch at `port` is accepting HTTP
/// connections (any response code counts — even a 503 "red" cluster is ready
/// enough for opencti-graphql to start).  Uses a short 500 ms connect timeout
/// so callers in the TUI event loop are not blocked for long.
pub fn opensearch_ready(port: u16) -> bool {
    let url = format!("http://localhost:{port}/_cluster/health");
    match ureq::get(&url).timeout(Duration::from_millis(500)).call() {
        Ok(_) | Err(ureq::Error::Status(_, _)) => true,
        Err(_) => false,
    }
}

/// Block until Elasticsearch/OpenSearch at `port` responds, or `max_secs` have
/// elapsed.  Prints a one-time "waiting…" line while polling.  Returns `true`
/// if ES became reachable before the timeout.
///
/// Safe to call from the main thread before the TUI starts (no lock held by
/// other threads at that point).
pub fn wait_for_opensearch(port: u16, max_secs: u64) -> bool {
    use crate::tui::YLW;

    let url = format!("http://localhost:{port}/_cluster/health");
    let deadline = std::time::Instant::now() + Duration::from_secs(max_secs);
    let mut ticks: u32 = 0;
    loop {
        match ureq::get(&url).timeout(Duration::from_secs(2)).call() {
            Ok(_) | Err(ureq::Error::Status(_, _)) => {
                if ticks > 0 {
                    println!("  {GRN}✓{R}  OpenSearch/ES ready on :{port}");
                }
                return true;
            }
            Err(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        if ticks == 0 {
            println!("  {DIM}Waiting for OpenSearch/ES on :{port}…{R}");
        }
        ticks += 1;
        std::thread::sleep(Duration::from_secs(2));
    }
    println!(
        "  {YLW}⚠{R}  OpenSearch/ES did not respond within {max_secs}s — opencti-graphql may fail on startup"
    );
    false
}

// ── Elasticsearch index wipe ───────────────────────────────────────────────────

/// Before spawning opencti-graphql, delete any stale `opencti*` Elasticsearch
/// indices so that opencti-graphql can perform a clean `[INIT]` on startup.
pub fn wipe_opencti_es_indices_if_stale(es_port: u16) {
    use crate::tui::YLW;

    let cat_url = format!("http://localhost:{es_port}/_cat/indices?h=index");
    println!("  {DIM}[ES pre-flight] querying {cat_url}{R}");
    let _ = io::stdout().flush();

    let resp = match ureq::get(&cat_url).timeout(Duration::from_secs(2)).call() {
        Ok(r) => {
            println!(
                "  {DIM}[ES pre-flight] ES responded (HTTP {}){R}",
                r.status()
            );
            r
        }
        Err(e) => {
            println!("  {DIM}[ES pre-flight] ES not reachable ({e}) — skipping index wipe{R}");
            return;
        }
    };

    let body = resp.into_string().unwrap_or_default();
    let all_indices: Vec<&str> = body
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    println!(
        "  {DIM}[ES pre-flight] total indices: {}  {:?}{R}",
        all_indices.len(),
        all_indices
    );

    let stale: Vec<String> = all_indices
        .iter()
        .filter(|l| l.starts_with("opencti"))
        .map(|l| l.to_string())
        .collect();

    if stale.is_empty() {
        println!("  {DIM}[ES pre-flight] no opencti* indices — nothing to wipe{R}");
        return;
    }

    println!(
        "  {YLW}⚠{R}  ES has {} stale OpenCTI index(es) — wiping for clean init:",
        stale.len()
    );
    for idx in &stale {
        let url = format!("http://localhost:{es_port}/{idx}");
        print!("    {DIM}DELETE {url} … {R}");
        let _ = io::stdout().flush();
        match ureq::request("DELETE", &url)
            .timeout(Duration::from_secs(5))
            .call()
        {
            Ok(r) => println!("{GRN}{}{R}", r.status()),
            Err(ureq::Error::Status(404, _)) => println!("{DIM}404 already gone{R}"),
            Err(ureq::Error::Status(code, _)) => println!("{RED}HTTP {code}{R}"),
            Err(e) => println!("{RED}error: {e}{R}"),
        }
    }
}

// ── Port helpers ───────────────────────────────────────────────────────────────

/// Scan a docker-compose file for a port mapping that exposes `container_port`
/// and return the host-side port number.
pub fn compose_host_port(compose_file: &Path, container_port: u16) -> Option<u16> {
    let content = fs::read_to_string(compose_file).ok()?;
    let _suffix = format!(":{container_port}");
    for line in content.lines() {
        let t = line
            .trim()
            .trim_start_matches('-')
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        if let Some(pos) = t.find(':') {
            let host_part = t[..pos].trim();
            let cont_part = t[pos + 1..].trim();
            if cont_part == container_port.to_string() {
                if let Ok(port) = host_part.parse::<u16>() {
                    return Some(port);
                }
            }
        }
    }
    None
}

/// Rewrite the port in a URL-like string.
pub fn replace_port_in_value(value: &str, new_port: u16) -> String {
    if let Some(colon) = value.rfind(':') {
        let (base, rest) = value.split_at(colon);
        let after_colon = &rest[1..];
        let port_end = after_colon.find('/').unwrap_or(after_colon.len());
        format!("{}:{}{}", base, new_port, &after_colon[port_end..])
    } else {
        format!("{}:{}", value, new_port)
    }
}
