use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "dev-launcher",
    about = "Launch the full multi-product dev stack for a feature branch.\n\
               Each service runs in its own process group; Ctrl+C kills the entire tree.",
    version
)]
pub struct Args {
    // ── Workspace shortcuts ───────────────────────────────────────────────────
    /// Open an existing workspace by its 8-char hash ID (shown in the workspace list).
    #[arg(long)]
    pub workspace: Option<String>,

    /// Branch for Filigran Copilot — creates or finds a workspace matching all supplied branches.
    #[arg(long)]
    pub copilot_branch: Option<String>,

    /// Branch for OpenCTI.
    #[arg(long)]
    pub opencti_branch: Option<String>,

    /// Branch for OpenAEV.
    #[arg(long)]
    pub openaev_branch: Option<String>,

    /// Branch for the ImportDoc connector.
    #[arg(long)]
    pub connector_branch: Option<String>,

    // ── Per-product worktree path overrides (runtime-only, not saved) ───────────
    /// Use an existing worktree directory directly for Filigran Copilot.
    #[arg(long)]
    pub copilot_worktree: Option<PathBuf>,

    /// Use an existing worktree directory directly for OpenCTI.
    #[arg(long)]
    pub opencti_worktree: Option<PathBuf>,

    /// Use an existing worktree directory directly for OpenAEV.
    #[arg(long)]
    pub openaev_worktree: Option<PathBuf>,

    /// Use an existing worktree directory directly for the ImportDoc connector.
    #[arg(long)]
    pub connector_worktree: Option<PathBuf>,

    // ── Per-product commit pinning (saved to workspace, creates detached worktree) ─
    /// Launch Filigran Copilot at a specific commit (creates a detached worktree).
    #[arg(long)]
    pub copilot_commit: Option<String>,

    /// Launch OpenCTI at a specific commit (creates a detached worktree).
    #[arg(long)]
    pub opencti_commit: Option<String>,

    /// Launch OpenAEV at a specific commit (creates a detached worktree).
    #[arg(long)]
    pub openaev_commit: Option<String>,

    /// Launch the ImportDoc connector at a specific commit (creates a detached worktree).
    #[arg(long)]
    pub connector_commit: Option<String>,

    // ── Runtime-only overrides (not saved to workspace) ───────────────────────
    /// Skip the OpenCTI React frontend only.
    #[arg(long)]
    pub no_opencti_front: bool,

    /// Skip the OpenAEV React frontend only.
    #[arg(long)]
    pub no_openaev_front: bool,

    /// Override the log directory (default: /tmp/dev-launcher-logs/{workspace-hash}).
    #[arg(long)]
    pub logs_dir: Option<PathBuf>,

    // ── Root configuration ────────────────────────────────────────────────────
    /// Path to the filigran workspace root (overrides env var and config file).
    /// Same effect as setting FILIGRAN_WORKSPACE_ROOT.
    #[arg(long)]
    pub workspace_root: Option<PathBuf>,

    // ── Internal subprocess flags (not shown in --help) ───────────────────────
    /// [internal] This process is a session worker — skip the workspace selector.
    #[arg(long, hide = true)]
    pub session_worker: bool,

    /// [internal] Force a clean start for this session (wipe Docker volumes etc.).
    #[arg(long, hide = true)]
    pub clean_start: bool,
}
