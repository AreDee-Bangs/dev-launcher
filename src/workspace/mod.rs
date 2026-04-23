pub mod env;
pub mod git;
pub mod repos;
pub mod selector;

pub use env::{
    deploy_workspace_env, extract_url_port, global_prefs_path, init_workspace_env, is_placeholder,
    parse_env_file, patch_url_default, port_in_use, preflight_port_checks, read_env_url_port,
    run_env_wizard, run_platform_mode_selector, write_env_file, ws_env_path, EnvVar, PortCheck,
    CONNECTOR_ENV_VARS, CONNECTOR_LICENCE_VARS, COPILOT_ENV_VARS, OPENCTI_ENV_VARS,
};
pub use git::{
    branch_to_slug, current_branch, current_commit_short, derive_branch_from_path, ensure_worktree,
    ensure_worktree_at_commit, ensure_worktree_branch, parse_commit_ref, worktree_delete_blockers,
    worktree_dirty_reasons, COMMIT_PREFIX,
};
pub use repos::{
    clone_repos, load_repos, run_clone_selector, CloneChoice, RepoEntry, DEFAULT_REPOS_CONF,
};
pub use selector::{
    choices_to_workspace, default_product_choices, discover_flags_in_dir, read_active_flags,
    run_flag_selector, run_product_selector, run_workspace_delete, run_workspace_selector,
    workspace_to_choices, write_active_flags, FlagChoice, LaunchMode, ProductChoice,
    WorkspaceAction,
};

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── Workspace constants ───────────────────────────────────────────────────────

/// Fixed product registry — (repo dir, display label, short key, service desc).
pub const PRODUCTS: &[(&str, &str, &str, &str)] = &[
    (
        "filigran-copilot",
        "Filigran Copilot",
        "copilot",
        "backend · worker · frontend",
    ),
    ("opencti", "OpenCTI", "opencti", "graphql · frontend"),
    ("openaev", "OpenAEV", "openaev", "backend · frontend"),
    (
        "connectors",
        "ImportDoc connector",
        "connector",
        "import-document-ai",
    ),
];

/// Return `{workspace_root}/.dev-workspaces`.
pub fn workspaces_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".dev-workspaces")
}

// ── Workspace data types ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct WorkspaceEntry {
    pub repo: String,
    pub enabled: bool,
    pub branch: String,
}

#[derive(Clone, Debug)]
pub struct WorkspaceConfig {
    pub hash: String,
    pub created: String,
    pub entries: Vec<WorkspaceEntry>,
}

impl WorkspaceConfig {
    pub fn summary(&self) -> String {
        let parts: Vec<String> = self
            .entries
            .iter()
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
        if parts.is_empty() {
            "(empty)".to_string()
        } else {
            parts.join("  ")
        }
    }
}

// ── Date helper ───────────────────────────────────────────────────────────────

pub fn today() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

// ── Workspace hash ────────────────────────────────────────────────────────────

pub fn compute_workspace_hash(entries: &[WorkspaceEntry]) -> String {
    let mut pairs: Vec<String> = entries
        .iter()
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

// ── Workspace persistence ─────────────────────────────────────────────────────

pub fn save_workspace(dir: &Path, config: &WorkspaceConfig) {
    let wdir = dir.join(&config.hash);
    let _ = fs::create_dir_all(&wdir);
    let path = wdir.join("workspace.conf");
    let mut out = format!("hash={}\ncreated={}\n", config.hash, config.created);
    for (e, (_, _, key, _)) in config.entries.iter().zip(PRODUCTS.iter()) {
        out.push_str(&format!(
            "{}_enabled={}\n{}_branch={}\n",
            key, e.enabled, key, e.branch
        ));
    }
    let _ = fs::write(&path, out);
}

pub fn load_workspace(dir: &Path, hash: &str) -> Option<WorkspaceConfig> {
    let path = dir.join(hash).join("workspace.conf");
    if !path.exists() {
        return None;
    }
    let map = parse_env_file(&path);
    if map.contains_key("deleted") {
        return None;
    }
    let entries = PRODUCTS
        .iter()
        .map(|(repo, _, key, _)| WorkspaceEntry {
            repo: repo.to_string(),
            enabled: map
                .get(&format!("{key}_enabled"))
                .is_some_and(|v| v == "true"),
            branch: map
                .get(&format!("{key}_branch"))
                .cloned()
                .unwrap_or_default(),
        })
        .collect();
    Some(WorkspaceConfig {
        hash: hash.to_string(),
        created: map.get("created").cloned().unwrap_or_default(),
        entries,
    })
}

pub fn list_workspaces(dir: &Path) -> Vec<WorkspaceConfig> {
    if !dir.is_dir() {
        return vec![];
    }
    let mut configs: Vec<WorkspaceConfig> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let hash = e.file_name().to_string_lossy().to_string();
            load_workspace(dir, &hash)
        })
        .collect();
    configs.sort_by(|a, b| b.created.cmp(&a.created));
    configs
}

pub fn tombstone_workspace(dir: &Path, hash: &str) {
    use std::io::Write as _;
    let path = dir.join(hash).join("workspace.conf");
    if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&path) {
        let _ = writeln!(f, "deleted={}", today());
    }
}
