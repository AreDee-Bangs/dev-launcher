use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};

use crate::tui::{BOLD, BUILD_VERSION, CYN, DIM, GRN, R, RED, YLW};

// ── Env var descriptor ────────────────────────────────────────────────────────

/// A required variable that the wizard will prompt for when missing or placeholder.
pub struct EnvVar {
    pub key: &'static str,
    /// Short label shown in the audit table.
    pub label: &'static str,
    /// One-line hint shown below the variable name during prompting.
    pub hint: &'static str,
    /// True → mask the current value in the audit table (tokens, certs, keys).
    pub secret: bool,
    /// True → accept multiple lines until the user types END on its own line.
    pub multiline: bool,
    /// True → generate a random UUID v4 when the user leaves the prompt blank.
    pub auto_uuid: bool,
    /// True → generate 32 random bytes as base64 when the user leaves the prompt blank.
    pub auto_b64: bool,
}

/// Variables required to boot OpenCTI for the first time.
pub const OPENCTI_ENV_VARS: &[EnvVar] = &[
    EnvVar {
        key: "APP__ADMIN__EMAIL",
        label: "Admin e-mail",
        hint: "Login e-mail for the built-in admin account (any valid address works)",
        secret: false,
        multiline: false,
        auto_uuid: false,
        auto_b64: false,
    },
    EnvVar {
        key: "APP__ADMIN__PASSWORD",
        label: "Admin password",
        hint: "Password for the built-in admin account (anything except 'ChangeMe')",
        secret: true,
        multiline: false,
        auto_uuid: false,
        auto_b64: false,
    },
    EnvVar {
        key: "APP__ADMIN__TOKEN",
        label: "Admin API token (UUID)",
        hint: "Leave blank to auto-generate — copy this value into OPENCTI_TOKEN for the connector",
        secret: true,
        multiline: false,
        auto_uuid: true,
        auto_b64: false,
    },
    EnvVar {
        key: "APP__ENCRYPTION_KEY",
        label: "Encryption key (base64)",
        hint: "Leave blank to auto-generate — equivalent to: openssl rand -base64 32",
        secret: true,
        multiline: false,
        auto_uuid: false,
        auto_b64: true,
    },
];

/// Focused wizard for the licence key only.
pub const CONNECTOR_LICENCE_VARS: &[EnvVar] = &[EnvVar {
    key: "CONNECTOR_LICENCE_KEY_PEM",
    label: "Filigran licence certificate (PEM)",
    hint: "Paste the full -----BEGIN CERTIFICATE----- … -----END CERTIFICATE----- block",
    secret: true,
    multiline: true,
    auto_uuid: false,
    auto_b64: false,
}];

/// Variables that require real user-supplied values before the connector can start.
pub const CONNECTOR_ENV_VARS: &[EnvVar] = &[
    EnvVar {
        key: "OPENCTI_TOKEN",
        label: "OpenCTI API token",
        hint: "Same value as APP__ADMIN__TOKEN set during OpenCTI setup",
        secret: true,
        multiline: false,
        auto_uuid: false,
        auto_b64: false,
    },
    EnvVar {
        key: "CONNECTOR_LICENCE_KEY_PEM",
        label: "Filigran licence certificate (PEM)",
        hint: "Paste the full -----BEGIN CERTIFICATE----- … -----END CERTIFICATE----- block",
        secret: true,
        multiline: true,
        auto_uuid: false,
        auto_b64: false,
    },
];

/// Variables that need real values before the Copilot backend can start.
pub const COPILOT_ENV_VARS: &[EnvVar] = &[
    EnvVar {
        key: "ADMIN_EMAIL",
        label: "Admin e-mail",
        hint: "Login e-mail for the built-in Copilot admin account",
        secret: false,
        multiline: false,
        auto_uuid: false,
        auto_b64: false,
    },
    EnvVar {
        key: "ADMIN_PASSWORD",
        label: "Admin password",
        hint: "Password for the built-in admin account (anything except 'ChangeMe')",
        secret: true,
        multiline: false,
        auto_uuid: false,
        auto_b64: false,
    },
];

// ── Env file helpers ──────────────────────────────────────────────────────────

pub fn parse_env_file(path: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(f) = File::open(path) else { return out };
    for line in io::BufReader::new(f).lines().map_while(Result::ok) {
        let line = line.trim().to_string();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            // Unescape \n sequences written by write_env_file (e.g. multi-line PEM).
            let v = v.trim_matches('"').trim_matches('\'').replace("\\n", "\n");
            out.insert(k.into(), v);
        }
    }
    out
}

/// Rewrite `path` preserving comments and key ordering.
/// Actual newlines in values are escaped to `\n` so the file stays single-line per key.
pub fn write_env_file(path: &Path, env: &HashMap<String, String>) {
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
    if !content.ends_with('\n') {
        content.push('\n');
    }
    let _ = fs::write(path, content);
}

/// Path to a product's .env file inside the workspace directory.
pub fn ws_env_path(ws_env_dir: &Path, product: &str) -> PathBuf {
    ws_env_dir.join(format!("{product}.env"))
}

/// Initialise a workspace .env file from the best available source.
pub fn init_workspace_env(
    env_path: &Path,
    repo_existing: Option<&Path>,
    template_srcs: &[PathBuf],
    hardcoded: &str,
) {
    if env_path.exists() {
        return;
    }
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
    for src in template_srcs {
        if src.exists() {
            let _ = fs::copy(src, env_path);
            return;
        }
    }
    let _ = fs::write(env_path, hardcoded);
}

/// Copy a workspace .env file to its destination inside a repo worktree.
pub fn deploy_workspace_env(src: &Path, dest: &Path) {
    if !src.exists() {
        return;
    }
    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Values with \n escapes must be double-quoted so that python-dotenv expands
    // them into actual newlines (required for PEM certificates, etc.).
    let Ok(content) = fs::read_to_string(src) else {
        let _ = fs::copy(src, dest);
        return;
    };
    let mut out_lines: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out_lines.push(line.to_string());
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            if v.contains("\\n") {
                out_lines.push(format!("{}=\"{}\"", k.trim(), v));
            } else {
                out_lines.push(line.to_string());
            }
        } else {
            out_lines.push(line.to_string());
        }
    }
    let mut output = out_lines.join("\n");
    if !output.ends_with('\n') {
        output.push('\n');
    }
    let _ = fs::write(dest, output);
}

// ── Port helpers ──────────────────────────────────────────────────────────────

/// Parse the port number out of a URL like "http://localhost:4000/health".
pub fn extract_url_port(url: &str) -> Option<u16> {
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()?;
    let host_port = authority.rsplit('@').next().unwrap_or(authority);

    if let Some(rest) = host_port.strip_prefix('[') {
        let end = rest.find(']')?;
        let after_bracket = &rest[end + 1..];
        let port = after_bracket.strip_prefix(':')?;
        return port.parse().ok();
    }

    let port_str = host_port.rsplit(':').next()?;
    port_str.parse().ok()
}

/// Returns None when the port is free, or Some(human-readable message + PIDs) when occupied.
pub fn port_in_use(port: u16) -> Option<String> {
    let out = Command::new("lsof")
        .args(["-ti", &format!(":{port}")])
        .output()
        .ok()?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let pids: Vec<&str> = raw.split_whitespace().collect();
    if pids.is_empty() {
        return None;
    }
    let procs: Vec<String> = pids
        .iter()
        .filter_map(|pid| {
            Command::new("ps")
                .args(["-p", pid, "-o", "comm="])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| format!("{} (PID {})", s.trim(), pid))
        })
        .collect();
    let desc = if procs.is_empty() {
        pids.join(", ")
    } else {
        procs.join(", ")
    };
    Some(format!(
        "Port {port} already in use by {desc} — stop it then press R to retry"
    ))
}

/// Patch a URL key's port in the env file from `from_port` to `to_port`.
pub fn patch_url_default(env_path: &Path, key: &str, from_port: u16, to_port: u16) {
    if from_port == to_port || !env_path.exists() {
        return;
    }
    let mut map = parse_env_file(env_path);
    let Some(current) = map.get(key).cloned() else {
        return;
    };
    if extract_url_port(&current) != Some(from_port) {
        return;
    }
    let patched = replace_port_in_value(&current, to_port);
    if patched == current {
        return;
    }
    println!("  {YLW}⚡{R}  {key}: {current} → {patched}  {DIM}(dev-launcher port){R}");
    map.insert(key.to_string(), patched);
    write_env_file(env_path, &map);
}

/// Read a URL-valued key from the env file and return its port number.
pub fn read_env_url_port(env_path: &Path, key: &str, default: u16) -> u16 {
    if !env_path.exists() {
        return default;
    }
    parse_env_file(env_path)
        .get(key)
        .and_then(|v| extract_url_port(v))
        .unwrap_or(default)
}

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

/// A single port-alignment check.
pub struct PortCheck {
    pub label: &'static str,
    pub env_key: &'static str,
    pub default_value: &'static str,
    pub container_port: u16,
}

pub fn preflight_port_checks(env_path: &Path, compose_file: &Path, checks: &[PortCheck]) {
    if !env_path.exists() || !compose_file.exists() {
        return;
    }
    let mut map = parse_env_file(env_path);
    let mut changed = false;

    for c in checks {
        let Some(host_port) =
            crate::services::docker::compose_host_port(compose_file, c.container_port)
        else {
            continue;
        };
        if host_port == c.container_port {
            continue;
        }

        let current = map
            .get(c.env_key)
            .cloned()
            .unwrap_or_else(|| c.default_value.to_string());
        let patched = replace_port_in_value(&current, host_port);
        if patched != current {
            println!(
                "  {YLW}⚡{R}  {}: {} → {}  {DIM}(compose maps :{} → :{}){R}",
                c.label, current, patched, c.container_port, host_port
            );
            map.insert(c.env_key.to_string(), patched);
            changed = true;
        }
    }

    if changed {
        write_env_file(env_path, &map);
    }
}

// ── Port offset application ───────────────────────────────────────────────────

/// Legacy marker from the now-removed delta-based migration. We strip it on
/// rewrite to keep env files clean.
const LEGACY_OFFSET_MARKER: &str = "_DEVLAUNCHER_PORT_OFFSET";

#[derive(Copy, Clone)]
enum PortKind {
    /// Plain integer port: `KEY=4000`.
    Plain,
    /// URL value with an embedded port: `KEY=redis://localhost:6379`. The
    /// port portion is rewritten in place; the rest of the URL is preserved
    /// (so e.g. credentials in `redis://:pw@host:6379` are kept).
    Url,
    /// URL value that should be injected when absent. Used for keys whose
    /// service falls back to a JSON default (e.g. `APP__ELASTICSEARCH__URL`).
    UrlInject(&'static str),
}

struct PortKey {
    key: &'static str,
    base_port: u16,
    kind: PortKind,
}

const COPILOT_PORT_KEYS: &[PortKey] = &[
    PortKey {
        key: "DATABASE_URL",
        base_port: 5432,
        kind: PortKind::Url,
    },
    // Redis dev compose remaps 6379→6380, MinIO remaps 9000→9002, so the
    // base-port-with-offset-zero is already the post-remap value.
    PortKey {
        key: "REDIS_URL",
        base_port: 6380,
        kind: PortKind::Url,
    },
    PortKey {
        key: "S3_ENDPOINT",
        base_port: 9002,
        kind: PortKind::Url,
    },
    PortKey {
        key: "INFINITY_URL",
        base_port: 7997,
        kind: PortKind::Url,
    },
    PortKey {
        key: "DEFAULT_EMBEDDING_PROVIDER_BASE_URL",
        base_port: 7997,
        kind: PortKind::Url,
    },
    PortKey {
        key: "AUTORESEARCH_URL",
        base_port: 8400,
        kind: PortKind::Url,
    },
    PortKey {
        key: "BASE_URL",
        base_port: 8100,
        kind: PortKind::Url,
    },
    PortKey {
        key: "FRONTEND_URL",
        base_port: 3100,
        kind: PortKind::Url,
    },
];

const OPENCTI_PORT_KEYS: &[PortKey] = &[
    PortKey {
        key: "APP__PORT",
        base_port: 4000,
        kind: PortKind::Plain,
    },
    PortKey {
        key: "APP__ELASTICSEARCH__URL",
        base_port: 9200,
        kind: PortKind::UrlInject("http://localhost"),
    },
    PortKey {
        key: "APP__REDIS__PORT",
        base_port: 6379,
        kind: PortKind::Plain,
    },
    PortKey {
        key: "APP__RABBITMQ__PORT",
        base_port: 5672,
        kind: PortKind::Plain,
    },
    PortKey {
        key: "APP__MINIO__PORT",
        base_port: 9000,
        kind: PortKind::Plain,
    },
];

const OPENAEV_PORT_KEYS: &[PortKey] = &[PortKey {
    key: "SERVER_PORT",
    base_port: 8080,
    kind: PortKind::Plain,
}];

const CONNECTOR_PORT_KEYS: &[PortKey] = &[PortKey {
    key: "OPENCTI_URL",
    base_port: 4000,
    kind: PortKind::Url,
}];

fn port_keys_for(product: &str) -> &'static [PortKey] {
    match product {
        "copilot" => COPILOT_PORT_KEYS,
        "opencti" => OPENCTI_PORT_KEYS,
        "openaev" => OPENAEV_PORT_KEYS,
        "connector" => CONNECTOR_PORT_KEYS,
        _ => &[],
    }
}

/// Idempotently set every port-bearing key in the workspace `.env` to
/// `base_port + port_offset`. Designed for the dynamic-offset model: each
/// launch recomputes the offset from current host port availability, then
/// rewrites the env from scratch — no marker, no migration, no delta logic.
///
/// For URL keys the existing value's port portion is replaced (preserving
/// credentials, query strings, etc.). Absent URL keys with `UrlInject` get
/// a freshly composed `{prefix}:{port}`. Plain-number keys are simply set.
pub fn apply_port_offset_to_env(env_path: &Path, product: &str, port_offset: u16) {
    if !env_path.exists() {
        return;
    }
    let mut map = parse_env_file(env_path);
    let mut changed = false;

    for spec in port_keys_for(product) {
        let target = spec.base_port.saturating_add(port_offset);
        let new_val = match spec.kind {
            PortKind::Plain => Some(target.to_string()),
            PortKind::Url => match map.get(spec.key) {
                Some(v) if !v.is_empty() => Some(replace_port_in_value(v, target)),
                _ => None,
            },
            PortKind::UrlInject(prefix) => match map.get(spec.key) {
                Some(v) if !v.is_empty() => Some(replace_port_in_value(v, target)),
                _ => Some(format!("{prefix}:{target}")),
            },
        };
        if let Some(v) = new_val {
            if map.get(spec.key) != Some(&v) {
                map.insert(spec.key.to_string(), v);
                changed = true;
            }
        }
    }

    let had_marker = map.remove(LEGACY_OFFSET_MARKER).is_some();
    if had_marker {
        changed = true;
    }

    if changed {
        write_env_file(env_path, &map);
    }
    if had_marker {
        // write_env_file preserves the original line for keys not in `map`,
        // which would leave the marker behind. Strip it explicitly.
        strip_env_keys(env_path, &[LEGACY_OFFSET_MARKER]);
    }
}

/// Remove every line in `path` whose key matches one of `keys`. Used to drop
/// keys that are no longer managed (e.g. the legacy offset marker).
fn strip_env_keys(path: &Path, keys: &[&str]) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let mut changed = false;
    let kept: Vec<&str> = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            if let Some((k, _)) = trimmed.split_once('=') {
                if keys.contains(&k.trim()) {
                    changed = true;
                    return false;
                }
            }
            true
        })
        .collect();
    if !changed {
        return;
    }
    let mut out = kept.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    let _ = fs::write(path, out);
}

/// All host-side base ports the workspace will try to bind for this product
/// (compose mappings + app-level ports). Used by `find_free_port_offset`.
pub fn base_ports_for(product: &str, compose_file: Option<&Path>) -> Vec<u16> {
    let mut bases: Vec<u16> = Vec::new();
    if let Some(path) = compose_file {
        if path.exists() {
            bases.extend(crate::services::compose_host_ports(path));
        }
    }
    for spec in port_keys_for(product) {
        bases.push(spec.base_port);
    }
    bases
}

/// Scan offsets in steps of 10 starting at 0 and return the smallest one for
/// which every `base + offset` is currently free on the host. Returns 0 (and
/// logs a warning) if no offset within 1000 works — at that point the user
/// has bigger problems than this picker.
pub fn find_free_port_offset(bases: &[u16]) -> u16 {
    const STEP: u16 = 10;
    const MAX: u16 = 1000;
    let mut bases = bases.to_vec();
    bases.sort();
    bases.dedup();

    let mut offset: u16 = 0;
    while offset <= MAX {
        if bases.iter().all(|b| is_port_free(b.saturating_add(offset))) {
            return offset;
        }
        offset = offset.saturating_add(STEP);
    }
    eprintln!(
        "  [dev-launcher] could not find a free port offset within +{MAX}; falling back to 0"
    );
    0
}

fn is_port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

// ── Platform mode selector ────────────────────────────────────────────────────

/// Interactive platform-mode selector shown when Copilot runs standalone (no OpenCTI).
pub fn run_platform_mode_selector(env_path: &Path, stopping: &Arc<AtomicBool>) {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return;
    }
    crate::tui::ensure_cooked_output();

    let mut map = parse_env_file(env_path);
    let current = map
        .get("PLATFORM_MODE")
        .cloned()
        .unwrap_or_else(|| "xtm_one".to_string());

    let options: &[(&str, &str, &str)] = &[
        (
            "xtm_one",
            "XTM One",
            "open platform — XTM One UI, EE features via license",
        ),
        (
            "copilot",
            "Filigran Copilot",
            "enterprise — Copilot UI, license required",
        ),
        ("dev", "Dev", "Copilot UI + XTM One seeding (testing)"),
    ];

    let mut cursor = options
        .iter()
        .position(|(v, _, _)| *v == current.as_str())
        .unwrap_or(0);

    let block_lines = options.len() + 3;

    let render_raw = |cur: usize| {
        print!("  {BOLD}Platform mode{R}  {DIM}↑↓  Enter to confirm  Esc back to menu{R}\r\n\r\n");
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

    println!("  {BOLD}Platform mode{R}  {DIM}↑↓  Enter to confirm  Esc back to menu{R}");
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
        if stopping.load(Ordering::Relaxed) {
            break false;
        }
        if !event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
            continue;
        }
        if let Ok(Event::Key(k)) = event::read() {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = cursor.saturating_sub(1);
                    print!("\x1b[{}A\x1b[0J", block_lines);
                    render_raw(cursor);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if cursor < options.len() - 1 {
                        cursor += 1;
                    }
                    print!("\x1b[{}A\x1b[0J", block_lines);
                    render_raw(cursor);
                }
                KeyCode::Enter => break true,
                KeyCode::Esc => {
                    let _ = disable_raw_mode();
                    crate::tui::exit_to_selector_menu();
                }
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    stopping.store(true, Ordering::Relaxed);
                    break false;
                }
                _ => {}
            }
        }
    };
    let _ = disable_raw_mode();
    crate::tui::ensure_cooked_output();

    let selected = options[cursor].0;
    if confirmed && selected != current.as_str() {
        let name = options[cursor].1;
        println!("  {GRN}✓{R}  PLATFORM_MODE → {BOLD}{selected}{R}  {DIM}({name}){R}");
        map.insert("PLATFORM_MODE".to_string(), selected.to_string());
        write_env_file(env_path, &map);
    } else {
        println!("  {DIM}Unchanged — {current}{R}");
    }
    println!();
}

// ── Env wizard ────────────────────────────────────────────────────────────────

/// Placeholder values that count as "not set".
pub fn is_placeholder(v: &str) -> bool {
    matches!(v.trim(), "" | "ChangeMe" | "changeme" | "TODO" | "CHANGEME")
}

pub fn dirs_base_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".config").join("dev-launcher"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/dev-launcher-prefs"))
}

pub fn global_prefs_path() -> PathBuf {
    dirs_base_dir().join("defaults.env")
}

fn rand_bytes(n: usize) -> Vec<u8> {
    use io::Read as _;
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf
}

fn gen_uuid() -> String {
    let mut b = rand_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],b[1],b[2],b[3], b[4],b[5], b[6],b[7], b[8],b[9], b[10],b[11],b[12],b[13],b[14],b[15],
    )
}

fn gen_base64_key() -> String {
    use std::fmt::Write as _;
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = rand_bytes(33);
    let mut out = String::with_capacity(44);
    for chunk in bytes[..33].chunks(3) {
        let n = match chunk.len() {
            3 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8 | chunk[2] as u32,
            2 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8,
            _ => (chunk[0] as u32) << 16,
        };
        let _ = write!(
            out,
            "{}{}{}{}",
            TABLE[(n >> 18 & 63) as usize] as char,
            TABLE[(n >> 12 & 63) as usize] as char,
            if chunk.len() > 1 {
                TABLE[(n >> 6 & 63) as usize] as char
            } else {
                '='
            },
            if chunk.len() > 2 {
                TABLE[(n & 63) as usize] as char
            } else {
                '='
            }
        );
    }
    out
}

fn gen_password() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    rand_bytes(24)
        .iter()
        .map(|b| CHARS[(b % CHARS.len() as u8) as usize] as char)
        .collect()
}

/// Generate a random 32-hex-char API token prefixed with `ar_`.
/// Used to auto-provision AUTORESEARCH_API_KEY on first launch.
pub fn gen_api_token() -> String {
    let b = rand_bytes(16);
    format!(
        "ar_{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\
         {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15],
    )
}

fn auto_generate_value(v: &EnvVar, prefs: &mut HashMap<String, String>) -> String {
    if let Some(existing) = prefs.get(v.key) {
        if !is_placeholder(existing) {
            return existing.clone();
        }
    }
    let generated = if v.auto_uuid {
        gen_uuid()
    } else if v.auto_b64 {
        gen_base64_key()
    } else if v.key.to_uppercase().contains("EMAIL") {
        "dev@dev.local".to_string()
    } else {
        gen_password()
    };
    prefs.insert(v.key.to_string(), generated.clone());
    generated
}

fn auto_generate_missing(
    env: &mut HashMap<String, String>,
    missing: &[&EnvVar],
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

/// Prompt the user for a single env variable value using a Ratatui text-area overlay.
fn read_value_tui(v: &EnvVar, step: usize, total: usize) -> Option<String> {
    use ratatui::{
        backend::CrosstermBackend,
        layout::{Constraint, Layout},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Paragraph},
        Terminal,
    };
    use tui_textarea::TextArea;

    let _guard = crate::tui::TuiGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).ok()?;

    let mut ta = TextArea::default();
    let border_style = Style::default().fg(Color::Cyan);
    let input_title = if v.multiline {
        format!(" {} — Alt+Enter or Ctrl+D to confirm ", v.key)
    } else {
        format!(" {} — Enter to confirm ", v.key)
    };
    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(
                input_title.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    ta.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    ta.set_style(Style::default());
    if !v.multiline {
        ta.set_hard_tab_indent(false);
    }

    let abort_hint = "  Esc / Ctrl+C  back to menu";
    let footer_style = Style::default().fg(Color::DarkGray);

    loop {
        let term = &mut terminal;
        let _ = term.draw(|f| {
            let area = f.area();
            let cols = area.width as usize;
            let rows = area.height as usize;

            let hdr = Line::from(vec![Span::styled(
                format!("  {}  —  env wizard  ({}/{})", BUILD_VERSION, step, total),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]);

            let info = vec![
                Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(v.label, Style::default().add_modifier(Modifier::BOLD)),
                ]),
                Line::from(vec![Span::styled(
                    format!("  {}", v.hint),
                    Style::default().fg(Color::DarkGray),
                )]),
            ];

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
            let info_h: u16 = (info.len() as u16) + 1;
            let footer_h: u16 = 1;

            let chunks = Layout::vertical([
                Constraint::Length(header_h),
                Constraint::Length(info_h),
                Constraint::Length(ta_h),
                Constraint::Length(footer_h),
                Constraint::Min(0),
            ])
            .split(area);

            f.render_widget(Paragraph::new(hdr), chunks[0]);
            f.render_widget(Paragraph::new(info), chunks[1]);
            f.render_widget(&ta, chunks[2]);
            f.render_widget(Paragraph::new(footer_text).style(footer_style), chunks[3]);

            let _ = cols;
        });

        if event::poll(std::time::Duration::from_millis(20)).unwrap_or(false) {
            let Ok(ev) = event::read() else { continue };
            let Event::Key(ke) = ev else { continue };

            let is_confirm = if v.multiline {
                (ke.code == KeyCode::Enter && ke.modifiers.contains(KeyModifiers::ALT))
                    || (ke.code == KeyCode::Char('d')
                        && ke.modifiers.contains(KeyModifiers::CONTROL))
            } else {
                ke.code == KeyCode::Enter && ke.modifiers == KeyModifiers::NONE
            };
            if is_confirm {
                let value = ta.lines().join("\n");
                return Some(value);
            }

            let is_abort = ke.code == KeyCode::Esc
                || (ke.code == KeyCode::Char('c') && ke.modifiers.contains(KeyModifiers::CONTROL));
            if is_abort {
                return None;
            }

            ta.input(ke);
        }
    }
}

/// Interactive env setup wizard for a single `.env` file.
pub fn run_env_wizard(env_path: &Path, vars: &[EnvVar], service_label: &str) {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return;
    }
    crate::tui::ensure_cooked_output();

    let mut env = parse_env_file(env_path);

    let missing: Vec<&EnvVar> = vars
        .iter()
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

    println!(
        "  {YLW}{} variable{} not set.{R}",
        missing.len(),
        if missing.len() == 1 { " is" } else { "s are" }
    );
    print!(
        "  Configure {} now? {DIM}[Y]es  [a]uto-generate  [n]o  [q]uit{R}  ",
        if missing.len() == 1 { "it" } else { "them" }
    );
    let _ = io::stdout().flush();

    let answer = match crate::config::read_line_or_interrupt() {
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

    let total = missing.len();
    let mut changed = false;
    for (step, v) in missing.iter().enumerate() {
        let raw_value = match read_value_tui(v, step + 1, total) {
            None => {
                crate::tui::ensure_cooked_output();
                if changed {
                    write_env_file(env_path, &env);
                }
                crate::tui::exit_to_selector_menu();
            }
            Some(s) => s,
        };
        crate::tui::ensure_cooked_output();

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

    if changed {
        write_env_file(env_path, &env);
        println!("  {GRN}Saved → {}{R}", env_path.display());
    } else {
        println!("  {YLW}Nothing changed.{R}");
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::{
        apply_port_offset_to_env, extract_url_port, find_free_port_offset, parse_env_file,
        replace_port_in_value, write_env_file, LEGACY_OFFSET_MARKER,
    };
    use std::collections::HashMap;

    #[test]
    fn extracts_port_from_http_urls() {
        assert_eq!(
            extract_url_port("http://localhost:8500/api/health"),
            Some(8500)
        );
        assert_eq!(extract_url_port("https://example.com:443/path"), Some(443));
    }

    #[test]
    fn extracts_port_from_service_connection_strings() {
        assert_eq!(
            extract_url_port("postgresql+asyncpg://copilot:secret@localhost:5432/copilot"),
            Some(5432)
        );
        assert_eq!(extract_url_port("redis://localhost:6380"), Some(6380));
        assert_eq!(extract_url_port("localhost:9002"), Some(9002));
    }

    #[test]
    fn replaces_port_in_service_connection_strings() {
        assert_eq!(
            replace_port_in_value(
                "postgresql+asyncpg://copilot:secret@localhost:5432/copilot",
                5832
            ),
            "postgresql+asyncpg://copilot:secret@localhost:5832/copilot"
        );
        assert_eq!(
            replace_port_in_value("redis://localhost:6380", 6780),
            "redis://localhost:6780"
        );
        assert_eq!(
            replace_port_in_value("localhost:9002", 9402),
            "localhost:9402"
        );
    }

    #[test]
    fn apply_port_offset_to_env_is_idempotent_and_strips_legacy_marker() {
        // Mixed legacy state: some keys at +400, some stuck at default,
        // plus the old `_DEVLAUNCHER_PORT_OFFSET` marker. After one apply
        // every known port-bearing key is at base+offset and the marker is
        // gone — no migration logic, no inference, just a clean rewrite.
        let dir =
            std::env::temp_dir().join(format!("devlauncher-idempotent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("copilot.env");

        let mut map: HashMap<String, String> = HashMap::new();
        map.insert(
            "DATABASE_URL".to_string(),
            "postgresql+asyncpg://copilot:secret@localhost:5832/copilot".to_string(),
        );
        map.insert(
            "REDIS_URL".to_string(),
            "redis://localhost:6380".to_string(),
        );
        map.insert("S3_ENDPOINT".to_string(), "localhost:9002".to_string());
        map.insert("BASE_URL".to_string(), "http://localhost:8500".to_string());
        map.insert(LEGACY_OFFSET_MARKER.to_string(), "400".to_string());
        write_env_file(&path, &map);

        apply_port_offset_to_env(&path, "copilot", 20);
        let after = parse_env_file(&path);
        assert_eq!(extract_url_port(&after["DATABASE_URL"]), Some(5452));
        assert_eq!(extract_url_port(&after["REDIS_URL"]), Some(6400));
        assert_eq!(extract_url_port(&after["S3_ENDPOINT"]), Some(9022));
        assert_eq!(extract_url_port(&after["BASE_URL"]), Some(8120));
        assert!(!after.contains_key(LEGACY_OFFSET_MARKER));

        // Calling again with the same offset is a no-op.
        let before_second = std::fs::read_to_string(&path).unwrap();
        apply_port_offset_to_env(&path, "copilot", 20);
        let after_second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before_second, after_second);

        // Offset 0 brings everything back to the defaults.
        apply_port_offset_to_env(&path, "copilot", 0);
        let after_zero = parse_env_file(&path);
        assert_eq!(extract_url_port(&after_zero["DATABASE_URL"]), Some(5432));
        assert_eq!(extract_url_port(&after_zero["REDIS_URL"]), Some(6380));
        assert_eq!(extract_url_port(&after_zero["S3_ENDPOINT"]), Some(9002));
        assert_eq!(extract_url_port(&after_zero["BASE_URL"]), Some(8100));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_free_port_offset_picks_zero_when_bases_are_free() {
        // Pick port numbers high enough that nothing ought to be bound on
        // them in CI / dev machines.
        let bases = vec![54000u16, 54001, 54002];
        assert_eq!(find_free_port_offset(&bases), 0);
    }

    #[test]
    fn find_free_port_offset_skips_offset_when_a_base_is_busy() {
        use std::net::TcpListener;
        // Bind one of the bases so offset 0 fails; offset 10 should succeed.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy_port = listener.local_addr().unwrap().port();
        // Pick a base such that base + 10 is unlikely to collide. We use the
        // busy port itself as the base: offset=0 collides, offset=10 is free
        // (assuming nothing on busy_port+10).
        let bases = vec![busy_port];
        let offset = find_free_port_offset(&bases);
        assert_ne!(offset, 0);
        assert_eq!(offset % 10, 0);
        drop(listener);
    }
}
