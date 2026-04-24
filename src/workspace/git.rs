use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::tui::{GRN, R, YLW};

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

fn run_git_quiet(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn main_checkout_label(dir: &Path) -> String {
    let branch = current_branch(dir);
    if !branch.is_empty() {
        return format!("main checkout on {branch}");
    }

    let commit = current_commit_short(dir);
    if !commit.is_empty() {
        return format!("main checkout at {commit}");
    }

    "main local checkout".to_string()
}

fn print_worktree_fallback(repo: &str, ref_kind: &str, requested_ref: &str, main_repo: &Path) {
    println!(
        "  {GRN}✓{R}  {repo}: continuing from the {} because requested {ref_kind} {requested_ref} is not available for a new worktree",
        main_checkout_label(main_repo),
    );
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
        println!("  {YLW}⚠{R}  {repo} repo not found locally, skipping worktree setup");
        return target;
    }

    println!("  Creating detached worktree {repo} @ {commit}…");
    let ok = run_git_quiet(
        &main_repo,
        &[
            "worktree",
            "add",
            "--detach",
            target.to_str().unwrap_or(""),
            commit,
        ],
    );

    if !ok {
        // Commit may not be present locally — fetch and retry.
        println!("  Checking origin for {commit}…");
        let _ = run_git_quiet(&main_repo, &["fetch", "origin"]);
        let ok2 = run_git_quiet(
            &main_repo,
            &[
                "worktree",
                "add",
                "--detach",
                target.to_str().unwrap_or(""),
                commit,
            ],
        );
        if ok2 {
            println!("  {GRN}✓{R}  Worktree created: {}", target.display());
        } else {
            print_worktree_fallback(repo, "commit", commit, &main_repo);
        }
    } else {
        println!("  {GRN}✓{R}  Worktree created: {}", target.display());
    }
    target
}

fn ref_exists(dir: &Path, refname: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", refname])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_git_visible(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn ensure_worktree_branch(workspace: &Path, repo: &str, branch: &str) -> PathBuf {
    let slug = branch_to_slug(branch);
    let target = workspace.join(format!("{}-{}", repo, slug));
    if target.is_dir() {
        return target;
    }

    let main_repo = workspace.join(repo);
    if !main_repo.is_dir() {
        println!("  {YLW}⚠{R}  {repo} repo not found locally, skipping worktree setup");
        return target;
    }

    // If the main checkout is already on this branch a worktree would fail
    // ("already used by worktree").  Use the main repo directly instead.
    if current_branch(&main_repo) == branch {
        println!("  {GRN}✓{R}  {repo} already on {branch}, using the main checkout");
        return main_repo.clone();
    }

    println!("  Creating worktree {repo} @ {branch}…");

    // Try 1: direct add (works if branch exists locally and isn't checked out elsewhere).
    if run_git_quiet(
        &main_repo,
        &["worktree", "add", target.to_str().unwrap_or(""), branch],
    ) {
        println!("  {GRN}✓{R}  Worktree created: {}", target.display());
        return target;
    }

    // Branch not found locally — fetch with an explicit refspec so that
    // refs/remotes/origin/<branch> is created (plain `fetch origin <branch>`
    // only writes FETCH_HEAD and does not create the remote-tracking ref).
    println!("  Checking origin/{branch}…");
    let refspec = format!("{branch}:refs/remotes/origin/{branch}");
    let _ = run_git_quiet(&main_repo, &["fetch", "origin", &refspec]);

    let local_ref = format!("refs/heads/{branch}");
    let remote_ref = format!("refs/remotes/origin/{branch}");

    let ok2 = if ref_exists(&main_repo, &local_ref) {
        // Local branch already exists — add the worktree directly.
        run_git_visible(
            &main_repo,
            &["worktree", "add", target.to_str().unwrap_or(""), branch],
        )
    } else if ref_exists(&main_repo, &remote_ref) {
        // Remote tracking ref exists — create a local tracking branch via -b.
        run_git_visible(
            &main_repo,
            &[
                "worktree",
                "add",
                "--track",
                "-b",
                &slug,
                target.to_str().unwrap_or(""),
                &format!("origin/{branch}"),
            ],
        )
    } else {
        // Branch doesn't exist locally or on origin — create it from origin/main.
        let base = if ref_exists(&main_repo, "refs/remotes/origin/main") {
            "origin/main"
        } else if ref_exists(&main_repo, "refs/remotes/origin/master") {
            "origin/master"
        } else {
            "HEAD"
        };
        println!("  Branch {branch} not found — creating from {base}…");
        run_git_visible(
            &main_repo,
            &[
                "worktree",
                "add",
                "-b",
                &slug,
                target.to_str().unwrap_or(""),
                base,
            ],
        )
    };

    if ok2 {
        println!("  {GRN}✓{R}  Worktree created: {}", target.display());
    } else {
        print_worktree_fallback(repo, "branch", branch, &main_repo);
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

fn worktree_has_uncommitted_changes(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }

    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|line| !line.trim().is_empty())
        })
        .unwrap_or(false)
}

enum OriginBranchStatus {
    Present,
    Missing,
    Unknown,
}

fn origin_branch_status(main_repo: &Path, branch: &str) -> OriginBranchStatus {
    if !main_repo.is_dir() {
        return OriginBranchStatus::Unknown;
    }

    let origin_ref = format!("refs/remotes/origin/{branch}");
    if run_git_quiet(main_repo, &["show-ref", "--verify", "--quiet", &origin_ref]) {
        return OriginBranchStatus::Present;
    }

    let remote_head = format!("refs/heads/{branch}");
    let status = Command::new("git")
        .args([
            "ls-remote",
            "--exit-code",
            "--heads",
            "origin",
            &remote_head,
        ])
        .current_dir(main_repo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match status.ok().and_then(|s| s.code()) {
        Some(0) => OriginBranchStatus::Present,
        Some(2) => OriginBranchStatus::Missing,
        _ => OriginBranchStatus::Unknown,
    }
}

pub fn worktree_delete_blockers(main_repo: &Path, worktree: &Path, branch: &str) -> Vec<String> {
    let mut blockers = Vec::new();

    if worktree_has_uncommitted_changes(worktree) {
        blockers.push("uncommitted changes are still present".to_string());
    }

    if parse_commit_ref(branch).is_none() {
        match origin_branch_status(main_repo, branch) {
            OriginBranchStatus::Present => {}
            OriginBranchStatus::Missing => {
                blockers.push(format!("branch {branch} is not available on origin"));
            }
            OriginBranchStatus::Unknown => {
                blockers.push(format!("origin could not be checked for branch {branch}"));
            }
        }
    }

    blockers
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
