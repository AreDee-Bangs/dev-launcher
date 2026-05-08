use std::path::PathBuf;

use clap::{Args as ClapArgs, Parser, Subcommand};

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Manipulate or inspect saved/running workspaces without entering the TUI.
    Workspace(WorkspaceCommand),
}

#[derive(ClapArgs, Debug, Clone)]
pub struct WorkspaceCommand {
    #[command(subcommand)]
    pub action: WorkspaceAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum WorkspaceAction {
    /// List known workspaces with their current runtime status.
    List {
        /// Emit JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Inspect one workspace, including live service state when a session is running.
    Status {
        /// Workspace hash (8 chars).
        hash: String,
        /// Emit JSON instead of the human-readable view.
        #[arg(long)]
        json: bool,
    },
    /// Stop a running workspace session.
    Stop {
        /// Workspace hash (8 chars).
        hash: String,
    },
    /// Restart a running workspace, or a single service within it.
    Restart {
        /// Workspace hash (8 chars).
        hash: String,
        /// Restart one named service, e.g. copilot-backend.
        #[arg(long, conflicts_with = "all")]
        service: Option<String>,
        /// Restart the full workspace stack.
        #[arg(long, default_value_t = false)]
        all: bool,
    },
}

#[derive(Parser)]
#[command(
    name = "dev-launcher",
    about = "Launch the full multi-product dev stack for a feature branch.\n\
               Each service runs in its own process group; Ctrl+C kills the entire tree.",
    version
)]
pub struct Args {
    /// Non-interactive workspace control commands.
    #[command(subcommand)]
    pub command: Option<Command>,

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
