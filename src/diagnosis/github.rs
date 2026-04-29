use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::diagnosis::FixStep;

pub struct IssueContext {
    pub health: String,
    pub uptime_secs: u64,
    pub log_path: PathBuf,
    pub spawn_cmd: Option<String>,
}

/// Build the fix steps needed to create a Python venv and install dependencies.
pub fn venv_fix_steps(backend_dir: &std::path::Path) -> Vec<FixStep> {
    let mut steps = vec![FixStep::new(
        &["python3", "-m", "venv", ".venv"],
        backend_dir,
    )];
    if backend_dir.join("requirements.txt").exists() {
        steps.push(FixStep::new(
            &[".venv/bin/pip", "install", "-r", "requirements.txt"],
            backend_dir,
        ));
    } else if backend_dir.join("pyproject.toml").exists() {
        steps.push(FixStep::new(
            &[".venv/bin/pip", "install", "-e", "."],
            backend_dir,
        ));
    }
    steps
}

pub fn create_github_issue(
    kind: &str,
    svc_name: &str,
    title: &str,
    body_lines: &[String],
    log_tail: &[String],
    ctx: &IssueContext,
) -> Result<String, String> {
    let issue_title = format!("recipe needed: {} ({})", title, kind);

    let uptime_str = if ctx.uptime_secs == 0 {
        "< 1s".to_string()
    } else if ctx.uptime_secs < 60 {
        format!("{}s", ctx.uptime_secs)
    } else {
        format!("{}m {}s", ctx.uptime_secs / 60, ctx.uptime_secs % 60)
    };
    let cmd_str = ctx.spawn_cmd.as_deref().unwrap_or("unknown");
    let log_str = ctx.log_path.to_string_lossy();

    let svc_state = format!(
        "### Service state\n\
         | Field | Value |\n\
         |-------|-------|\n\
         | **Health**  | {health} |\n\
         | **Uptime**  | {uptime} |\n\
         | **Command** | `{cmd}` |\n\
         | **Log**     | `{log}` |\n",
        health = ctx.health,
        uptime = uptime_str,
        cmd = cmd_str,
        log = log_str,
    );

    let os_version = Command::new("uname")
        .args(["-srm"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));

    let env_section = format!(
        "### Environment\n\
         | Field | Value |\n\
         |-------|-------|\n\
         | **OS**   | {os} |\n\
         | **Arch** | {arch} |\n",
        os = os_version,
        arch = std::env::consts::ARCH,
    );

    let log_section = if log_tail.is_empty() {
        String::new()
    } else {
        format!("### Logs\n```\n{}\n```\n\n", log_tail.join("\n"))
    };

    let issue_body = format!(
        "## Missing fix recipe\n\n\
         A finding was encountered that has no automated fix yet.\n\n\
         | Field   | Value |\n\
         |---------|-------|\n\
         | **Kind**    | `{kind}` |\n\
         | **Service** | `{svc_name}` |\n\
         | **Finding** | {title} |\n\n\
         {svc_state}\n\
         ### Details\n\
         ```\n{details}\n```\n\n\
         {log_section}\
         {env_section}\n\
         Please implement a recipe (fix action) for this kind in `diagnose_service`.",
        details = body_lines.join("\n"),
        svc_state = svc_state,
        log_section = log_section,
        env_section = env_section,
    );

    const GH_BODY_LIMIT: usize = 65_000;
    let body_arg = if issue_body.len() > GH_BODY_LIMIT {
        format!(
            "{}\n\n*(truncated — body exceeded GitHub limit)*",
            &issue_body[..GH_BODY_LIMIT]
        )
    } else {
        issue_body
    };

    let out = Command::new("gh")
        .args([
            "issue",
            "create",
            "--repo",
            "AreDee-Bangs/dev-launcher",
            "--title",
            &issue_title,
            "--body",
            &body_arg,
            "--label",
            "recipe-needed",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("gh not found: {e}"))?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}
