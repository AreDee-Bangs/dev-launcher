pub mod engine;
pub mod github;
pub mod llm;
pub mod patterns;
pub mod recipe;

pub use engine::{check_diag_patterns, diagnose_crash, diagnose_service};
pub use github::{create_github_issue, venv_fix_steps, IssueContext};
pub use llm::{resolve_llm_config, LlmConfig, LlmProvider};
pub use patterns::{
    needs_recipe, DIAG_PATTERNS, KIND_BOOTSTRAP_RUN, KIND_CONNECTOR_LICENCE_MISSING,
    KIND_CONNECTOR_TYPE_MISSING, KIND_CRASH, KIND_DEGRADED_UNKNOWN, KIND_ENV_PLACEHOLDER,
    KIND_INFO, KIND_INFO_BOOTSTRAP_CHECK, KIND_INFO_LOG_PATTERNS, KIND_INFO_LOG_TAIL,
    KIND_INFO_NO_ISSUES, KIND_MINIO_DOWN, KIND_NODE_MODULES, KIND_OPENCTI_ES_PARTIAL_INIT,
    KIND_PYTHON_VENV, RECIPE_CATALOG,
};

use std::path::PathBuf;

use crate::workspace::EnvVar;

// ── DiagEvent ─────────────────────────────────────────────────────────────────

pub enum DiagEvent {
    Result { svc_idx: usize, msg: String },
}

// ── Finding data types ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct FixStep {
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl FixStep {
    pub fn new(args: &[&str], cwd: &std::path::Path) -> Self {
        Self {
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_path_buf(),
        }
    }
}

#[derive(Clone)]
pub enum FixAction {
    Steps {
        label: String,
        steps: Vec<FixStep>,
        restart_after: bool,
    },
    EnvWizard {
        env_path: PathBuf,
        deploy_to: Option<PathBuf>,
        vars: &'static [EnvVar],
        product: &'static str,
        restart_after: bool,
    },
    PatchEnvVar {
        label: String,
        env_path: PathBuf,
        key: &'static str,
        value: &'static str,
        restart_after: bool,
    },
}

impl FixAction {
    pub fn label(&self) -> &str {
        match self {
            FixAction::Steps { label, .. } => label.as_str(),
            FixAction::EnvWizard { product, .. } => product,
            FixAction::PatchEnvVar { label, .. } => label.as_str(),
        }
    }
    pub fn restart_after(&self) -> bool {
        match self {
            FixAction::Steps { restart_after, .. } => *restart_after,
            FixAction::EnvWizard { restart_after, .. } => *restart_after,
            FixAction::PatchEnvVar { restart_after, .. } => *restart_after,
        }
    }
}

#[derive(Clone)]
pub struct Finding {
    pub kind: String,
    pub title: String,
    pub body: Vec<String>,
    pub fix: Option<FixAction>,
    pub resolved: bool,
}

impl Finding {
    pub fn info(kind: impl Into<String>, title: impl Into<String>, body: Vec<String>) -> Self {
        Self {
            kind: kind.into(),
            title: title.into(),
            body,
            fix: None,
            resolved: false,
        }
    }
    pub fn fixable(
        kind: impl Into<String>,
        title: impl Into<String>,
        body: Vec<String>,
        fix: FixAction,
    ) -> Self {
        Self {
            kind: kind.into(),
            title: title.into(),
            body,
            fix: Some(fix),
            resolved: false,
        }
    }
}

/// Execute a fix action synchronously with visible output.
/// Must be called with raw mode already dropped.
/// Returns true when all steps completed successfully.
pub fn run_fix_action(action: &FixAction) -> bool {
    use crate::services::docker::run_blocking;
    use crate::tui::{BOLD, CYN, DIM, GRN, R, RED};
    use crate::workspace::{deploy_workspace_env, parse_env_file, run_env_wizard, write_env_file};
    use std::io::Write;

    let sep = "─".repeat(56);
    match action {
        FixAction::Steps { label, steps, .. } => {
            crate::launcher_log::log(&format!("[FIX] applying: {label}"));
            println!("\n  {BOLD}{CYN}Applying fix:{R}  {label}\n  {DIM}{sep}{R}\n");
            for step in steps {
                println!("  {DIM}$ {}{R}", step.args.join(" "));
                let prog = step.args[0].as_str();
                let argv: Vec<&str> = step.args[1..].iter().map(|s| s.as_str()).collect();
                let code = run_blocking(prog, &argv, &step.cwd); // run_blocking already logs
                if code != 0 {
                    crate::launcher_log::log(&format!(
                        "[FIX] failed (exit {code}) — remaining steps skipped"
                    ));
                    println!("\n  {RED}✗{R}  Command exited {code}. Remaining steps skipped.");
                    return false;
                }
            }
            crate::launcher_log::log("[FIX] applied successfully");
            println!("\n  {GRN}✓{R}  Fix applied.");
            true
        }
        FixAction::EnvWizard {
            env_path,
            deploy_to,
            vars,
            product,
            ..
        } => {
            run_env_wizard(env_path, vars, product);
            if let Some(dest) = deploy_to {
                deploy_workspace_env(env_path, dest);
            }
            true
        }
        FixAction::PatchEnvVar {
            label,
            env_path,
            key,
            value,
            ..
        } => {
            println!("\n  {BOLD}{CYN}Applying fix:{R}  {label}\n  {DIM}{sep}{R}\n");
            let mut env = parse_env_file(env_path);
            env.insert(key.to_string(), value.to_string());
            write_env_file(env_path, &env);
            println!("  {GRN}✓{R}  Set {key}={value}");
            println!("  {DIM}  in {}{R}", env_path.display());
            let _ = std::io::stdout().flush();
            true
        }
    }
}
