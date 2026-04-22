use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::tui::{GRN, R, RED, YLW};

/// Sentinel prefix used to store commit-pinned branches in workspace configs.
pub const COMMIT_PREFIX: &str = "commit:";

/// If `branch` is a commit-pinned ref (stored as `"commit:<hash>"`), return the hash part.
pub fn parse_commit_ref(branch: &str) -> Option<&str> {
    branch.strip_prefix(COMMIT_PREFIX)
}

/// Convert a branch name to a filesystem-safe slug (e.g. `issue/123-foo` → `issue-123-foo`).
/// Commit refs (`commit:<hash>`) become `commit-<hash>`.
pub fn branch_to_slug(branch: &str) -> String {
    if let Some(hash) = parse_commit_ref(branch) {
        return format!("commit-{hash}");
    }
    branch.replace('/', "-")
}

/// Return the currently checked-out branch name for `dir`, or empty string.
pub fn current_branch(dir: &Path) -> String {
    if !dir.is_dir() {
        return String::new();
    }
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
pub fn current_commit_short(dir: &Path) -> String {
    if !dir.is_dir() {
        return String::new();
    }
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Ensure a git worktree exists for `branch` (or a commit ref `commit:<hash>`).
/// Commit refs delegate to `ensure_worktree_at_commit`; branch names use the
/// standard `git worktree add` flow.
pub fn ensure_worktree(workspace: &Path, repo: &str, branch: &str) -> PathBuf {
    if let Some(commit) = parse_commit_ref(branch) {
        return ensure_worktree_at_commit(workspace, repo, commit);
    }
    ensure_worktree_branch(workspace, repo, branch)
}

/// Create (if missing) a detached worktree for a specific commit at
/// `{workspace}/{repo}-commit-{hash}`.
pub fn ensure_worktree_at_commit(workspace: &Path, repo: &str, commit: &str) -> PathBuf {
    let target = workspace.join(format!("{}-commit-{}", repo, commit));
    if target.is_dir() {
        return target;
    }

    let main_repo = workspace.join(repo);
    if !main_repo.is_dir() {
        println!("  {YLW}⚠{R}  {repo} main repo not found — cannot create worktree");
        return target;
    }

    println!("  Creating detached worktree {repo} @ {commit}…");
    let ok = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            target.to_str().unwrap_or(""),
            commit,
        ])
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
            .args([
                "worktree",
                "add",
                "--detach",
                target.to_str().unwrap_or(""),
                commit,
            ])
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

pub fn ensure_worktree_branch(workspace: &Path, repo: &str, branch: &str) -> PathBuf {
    let slug = branch_to_slug(branch);
    let target = workspace.join(format!("{}-{}", repo, slug));
    if target.is_dir() {
        return target;
    }

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
            .args([
                "worktree",
                "add",
                "--track",
                "-b",
                &slug,
                target.to_str().unwrap_or(""),
                &format!("origin/{branch}"),
            ])
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
pub fn worktree_dirty_reasons(dir: &Path) -> Vec<String> {
    let mut reasons = Vec::new();
    if !dir.is_dir() {
        return reasons;
    }

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

/// Read the current branch from a worktree path.  If the HEAD is detached, returns
/// `Some("commit:<short-hash>")`.  Returns `None` only if the path is not a git repo.
pub fn derive_branch_from_path(path: &Path) -> Option<String> {
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
