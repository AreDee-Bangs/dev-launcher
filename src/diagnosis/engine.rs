use std::path::Path;
use std::process::{Command, Stdio};

use crate::diagnosis::{
    github::venv_fix_steps,
    llm::{llm_diagnose, LlmConfig},
    patterns::*,
    Finding, FixAction, FixStep,
};
use crate::services::manifest::{parse_dev_launcher_conf, BootstrapDef};
use crate::services::{Health, Paths, Svc};
use crate::tui::logview::tail_file;
use crate::workspace::{
    parse_env_file, CONNECTOR_ENV_VARS, CONNECTOR_LICENCE_VARS, OPENCTI_ENV_VARS,
};

/// Scan the last 200 lines of a log for a known pattern.
pub fn check_diag_patterns(log_path: &Path) -> Option<String> {
    let lines = tail_file(log_path, 200);
    for line in &lines {
        let lower = line.to_lowercase();
        for (needle, reason) in DIAG_PATTERNS {
            if lower.contains(needle) {
                return Some(reason.to_string());
            }
        }
    }
    None
}

/// Diagnose a crash: pattern match first; fall back to LLM if no pattern found.
pub fn diagnose_crash(log_path: &Path, llm: Option<&LlmConfig>) -> Option<String> {
    if let Some(reason) = check_diag_patterns(log_path) {
        return Some(reason);
    }
    let cfg = llm?;
    let tail = tail_file(log_path, 60).join("\n");
    if tail.trim().is_empty() {
        return None;
    }
    llm_diagnose(cfg, &tail)
}

/// Analyse a service and return a list of `Finding` structs.
pub fn diagnose_service(svc: &Svc, paths: &Paths, ws_env_dir: &Path) -> Vec<Finding> {
    let repo_dir: &Path = if svc.name.starts_with("copilot") {
        &paths.copilot
    } else if svc.name.starts_with("opencti") {
        &paths.opencti
    } else if svc.name.starts_with("connector") {
        &paths.connector
    } else if svc.name.starts_with("openaev") {
        &paths.openaev
    } else {
        &paths.copilot
    };

    let mut findings: Vec<Finding> = Vec::new();

    // ── 1. Degraded: surface reason with automated fix ────────────────────────
    if let Health::Degraded(msg) = &svc.health {
        let body = vec![msg.clone()];

        let backend_dir_for_check = || {
            if repo_dir.join("backend").is_dir() {
                repo_dir.join("backend")
            } else {
                repo_dir.to_path_buf()
            }
        };
        let fe_dir_for_check = || {
            if repo_dir.join("frontend").is_dir() {
                repo_dir.join("frontend")
            } else {
                repo_dir.to_path_buf()
            }
        };

        enum DegradedOutcome {
            NeedsRestart,
            Fixable(FixAction, &'static str),
            Unknown,
        }

        let outcome = if msg.contains("venv") || msg.contains(".venv") {
            let bd = backend_dir_for_check();
            let venv_ok = bd.join(".venv/bin/python").exists()
                || bd.join(".venv/bin/python3").exists()
                || std::fs::read_dir(bd.join(".venv/bin"))
                    .ok()
                    .and_then(|mut d| d.next())
                    .is_some();
            if venv_ok {
                DegradedOutcome::NeedsRestart
            } else {
                DegradedOutcome::Fixable(
                    FixAction::Steps {
                        label: "Create Python virtual environment and install dependencies".into(),
                        steps: venv_fix_steps(&bd),
                        restart_after: true,
                    },
                    KIND_PYTHON_VENV,
                )
            }
        } else if msg.contains("node_modules") {
            let fe = fe_dir_for_check();
            if fe.join("node_modules").is_dir() {
                DegradedOutcome::NeedsRestart
            } else {
                DegradedOutcome::Fixable(
                    FixAction::Steps {
                        label: "Install JavaScript dependencies (yarn install)".into(),
                        steps: vec![FixStep::new(&["yarn", "install"], &fe)],
                        restart_after: false,
                    },
                    KIND_NODE_MODULES,
                )
            }
        } else if msg.contains("APP__ADMIN__PASSWORD") || msg.contains("credentials") {
            DegradedOutcome::Fixable(
                FixAction::EnvWizard {
                    env_path: ws_env_dir.join("opencti.env"),
                    deploy_to: Some(
                        paths
                            .opencti
                            .join("opencti-platform/opencti-graphql/.env.dev"),
                    ),
                    vars: OPENCTI_ENV_VARS,
                    product: "OpenCTI",
                    restart_after: false,
                },
                KIND_ENV_PLACEHOLDER,
            )
        } else if msg.contains("OPENCTI_TOKEN") {
            DegradedOutcome::Fixable(
                FixAction::EnvWizard {
                    env_path: ws_env_dir.join("connector.env"),
                    deploy_to: Some(paths.connector.join(".env.dev")),
                    vars: CONNECTOR_ENV_VARS,
                    product: "ImportDocumentAI connector",
                    restart_after: false,
                },
                KIND_ENV_PLACEHOLDER,
            )
        } else {
            DegradedOutcome::Unknown
        };

        match outcome {
            DegradedOutcome::NeedsRestart => {
                let mut restart_body = body;
                restart_body
                    .push("  Fix already applied — restart the service to pick it up.".into());
                restart_body.push("  Press Ctrl+C and run dev-launcher again.".into());
                let mut f = Finding::info(
                    KIND_INFO,
                    "Dependency installed — restart needed",
                    restart_body,
                );
                f.resolved = true;
                findings.push(f);
            }
            DegradedOutcome::Fixable(fix, kind) => {
                findings.push(Finding::fixable(kind, "Service did not start", body, fix));
            }
            DegradedOutcome::Unknown => {
                findings.push(Finding::info(
                    KIND_DEGRADED_UNKNOWN,
                    "Service did not start",
                    body,
                ));
            }
        }
    }

    // ── 1b. Crashed: check for known patterns ────────────────────────────────
    if let Health::Crashed(code) = &svc.health {
        let crash_log = tail_file(&svc.log_path, 30);
        let mut crash_handled = false;

        // opencti-graphql: Elasticsearch partial init
        if svc.name == "opencti-graphql"
            && crash_log.iter().any(|l| l.contains("index already exists"))
        {
            let es_container = Command::new("docker")
                .args([
                    "ps",
                    "-a",
                    "--filter",
                    "name=opencti-dev-elasticsearch",
                    "--format",
                    "{{.Names}}",
                ])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
                .filter(|s| !s.is_empty());
            let es_volume = Command::new("docker")
                .args(["volume", "ls", "--filter", "name=esdata", "-q"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
                .filter(|s| !s.is_empty());

            match (es_container, es_volume) {
                (Some(container), Some(volume)) => {
                    let compose_project = Command::new("docker")
                        .args([
                            "inspect",
                            &container,
                            "--format",
                            "{{index .Config.Labels \"com.docker.compose.project\"}}",
                        ])
                        .output()
                        .ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    let compose_file = paths
                        .opencti
                        .join("opencti-platform/opencti-dev/docker-compose.yml");

                    let mut steps = vec![
                        FixStep::new(&["docker", "stop", &container], &paths.opencti),
                        FixStep::new(&["docker", "volume", "rm", &volume], &paths.opencti),
                    ];
                    if let (Some(ref project), true) = (&compose_project, compose_file.exists()) {
                        let file_str = compose_file.to_string_lossy().into_owned();
                        steps.push(FixStep::new(
                            &[
                                "docker",
                                "compose",
                                "-p",
                                project,
                                "-f",
                                &file_str,
                                "up",
                                "-d",
                                "opencti-dev-elasticsearch",
                            ],
                            &paths.opencti,
                        ));
                    }

                    let mut body = vec![
                        format!("  Exit code : {code}"),
                        "  A previous run was interrupted during first-time schema init.".into(),
                        "  Elasticsearch holds a partial index that blocks re-initialization."
                            .into(),
                        format!("  Container : {container}"),
                        format!("  Volume    : {volume}"),
                    ];
                    if compose_project.is_some() {
                        body.push("  Fix: stop ES, wipe volume, restart ES, then re-launch opencti-graphql.".into());
                    } else {
                        body.push("  Fix: stop ES and wipe volume — restart opencti-graphql manually after.".into());
                    }

                    findings.push(Finding::fixable(
                        KIND_OPENCTI_ES_PARTIAL_INIT,
                        "Elasticsearch index partially initialized (interrupted previous run)",
                        body,
                        FixAction::Steps {
                            label: "Wipe stale ES data, restart Elasticsearch, re-launch service"
                                .into(),
                            steps,
                            restart_after: compose_project.is_some(),
                        },
                    ));
                    crash_handled = true;
                }
                (container, volume) => {
                    findings.push(Finding::info(
                        KIND_OPENCTI_ES_PARTIAL_INIT,
                        "Elasticsearch index partially initialized (interrupted previous run)",
                        vec![
                            format!("  Exit code : {code}"),
                            "  A previous run was interrupted during first-time schema init.".into(),
                            "  Could not locate Docker resources to auto-fix:".into(),
                            format!("  Container : {}", container.as_deref().unwrap_or("not found")),
                            format!("  Volume    : {}", volume.as_deref().unwrap_or("not found")),
                            "  Manual fix: docker stop <es-container> && docker volume rm <esdata-volume>".into(),
                        ],
                    ));
                    crash_handled = true;
                }
            }
        }

        // connector: missing CONNECTOR_TYPE
        if !crash_handled
            && svc.name == "connector"
            && crash_log
                .iter()
                .any(|l| l.contains("None is not a valid ConnectorType"))
        {
            let env_path = paths.connector.join(".env.dev");
            findings.push(Finding::fixable(
                KIND_CONNECTOR_TYPE_MISSING,
                "CONNECTOR_TYPE not configured",
                vec![
                    format!("  Exit code : {code}"),
                    "  The connector env file is missing CONNECTOR_TYPE.".into(),
                    "  Python resolved it to None → ConnectorType(None) → ValueError.".into(),
                    format!("  Env file  : {}", env_path.display()),
                    "  Fix: add CONNECTOR_TYPE=INTERNAL_IMPORT_FILE".into(),
                ],
                FixAction::PatchEnvVar {
                    label: "Add CONNECTOR_TYPE=INTERNAL_IMPORT_FILE to connector env".into(),
                    env_path,
                    key: "CONNECTOR_TYPE",
                    value: "INTERNAL_IMPORT_FILE",
                    restart_after: true,
                },
            ));
            crash_handled = true;
        }

        // connector: licence key not configured
        if !crash_handled
            && svc.name == "connector"
            && crash_log
                .iter()
                .any(|l| l.contains("NoneType' object has no attribute 'encode'"))
        {
            let ws_connector = ws_env_dir.join("connector.env");
            let repo_connector = paths.connector.join(".env.dev");
            findings.push(Finding::fixable(
                KIND_CONNECTOR_LICENCE_MISSING,
                "Filigran licence key not configured",
                vec![
                    format!("  Exit code : {code}"),
                    "  CONNECTOR_LICENCE_KEY_PEM is empty or missing.".into(),
                    "  Python called base64.b64encode(None.encode()) → AttributeError.".into(),
                    format!("  Env file  : {}", ws_connector.display()),
                    "  Fix: paste the PEM certificate in the wizard below.".into(),
                ],
                FixAction::EnvWizard {
                    env_path: ws_connector,
                    deploy_to: Some(repo_connector),
                    vars: CONNECTOR_LICENCE_VARS,
                    product: "ImportDocumentAI connector — licence key",
                    restart_after: true,
                },
            ));
            crash_handled = true;
        }

        if !crash_handled {
            findings.push(Finding::info(
                KIND_CRASH,
                format!("Service crashed (exit {})", code),
                vec![
                    format!("  Exit code: {}", code),
                    format!("  Log: {}", svc.log_path.display()),
                    "  No automated fix is available — use r to file a GitHub issue.".into(),
                ],
            ));
        }
    }

    // ── 2. Log pattern analysis ───────────────────────────────────────────────
    if matches!(svc.health, Health::Crashed(_) | Health::Degraded(_)) {
        let log_lines = tail_file(&svc.log_path, 150);

        // copilot-backend/worker: MinIO container down
        if (svc.name.contains("copilot-backend") || svc.name.contains("copilot-worker"))
            && log_lines
                .iter()
                .any(|l| l.to_lowercase().contains("minio not ready"))
        {
            let minio_info = Command::new("docker")
                .args([
                    "ps",
                    "-a",
                    "--filter",
                    "name=copilot-minio",
                    "--format",
                    "{{.Names}}\t{{.Status}}",
                ])
                .stdin(Stdio::null())
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.lines().next().map(|l| l.to_string()))
                .filter(|s| !s.is_empty());

            match minio_info {
                Some(ref line) => {
                    let parts: Vec<&str> = line.splitn(2, '\t').collect();
                    let cname = parts[0].trim().to_string();
                    let status = parts.get(1).copied().unwrap_or("").trim().to_string();
                    let is_running = status.starts_with("Up");
                    if is_running {
                        findings.push(Finding::info(
                            KIND_MINIO_DOWN,
                            "MinIO unreachable (container running but connection failed)",
                            vec![
                                format!("  Container : {cname}"),
                                format!("  Status    : {status}"),
                                "  MinIO is running but the backend cannot connect to it.".into(),
                                "  Check MINIO_ENDPOINT in your .env (expected http://localhost:9000).".into(),
                            ],
                        ));
                    } else {
                        findings.push(Finding::fixable(
                            KIND_MINIO_DOWN,
                            "MinIO container is stopped",
                            vec![
                                format!("  Container : {cname}"),
                                format!("  Status    : {status}"),
                                "  MinIO (S3-compatible storage) is not running.".into(),
                                "  Fix: start the container, then restart the backend.".into(),
                            ],
                            FixAction::Steps {
                                label: format!("Start MinIO container ({cname})"),
                                steps: vec![FixStep::new(
                                    &["docker", "start", &cname],
                                    &paths.copilot,
                                )],
                                restart_after: true,
                            },
                        ));
                    }
                }
                None => {
                    let compose_file = paths.copilot.join("docker-compose.dev.yml");
                    let compose_str = compose_file.to_string_lossy().into_owned();
                    if compose_file.exists() {
                        findings.push(Finding::fixable(
                            KIND_MINIO_DOWN,
                            "MinIO container not found — Docker stack not started",
                            vec![
                                "  No MinIO container detected (may never have been created)."
                                    .into(),
                                "  Fix: start the full Copilot Docker stack.".into(),
                            ],
                            FixAction::Steps {
                                label: "Start Copilot Docker services (docker compose up -d)"
                                    .into(),
                                steps: vec![FixStep::new(
                                    &["docker", "compose", "-f", &compose_str, "up", "-d"],
                                    &paths.copilot,
                                )],
                                restart_after: true,
                            },
                        ));
                    } else {
                        findings.push(Finding::info(
                            KIND_MINIO_DOWN,
                            "MinIO container not found",
                            vec![
                                "  No MinIO container detected — start Docker services first."
                                    .into(),
                                "  Run: docker compose -f docker-compose.dev.yml up -d".into(),
                            ],
                        ));
                    }
                }
            }
        }

        let mut matched: Vec<String> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for line in &log_lines {
            let lower = line.to_lowercase();
            for (needle, reason) in DIAG_PATTERNS {
                if lower.contains(needle) && !seen.contains(reason) {
                    seen.push(reason);
                    matched.push(format!("  — {reason}"));
                }
            }
        }
        if !matched.is_empty() {
            findings.push(Finding::info(
                KIND_INFO_LOG_PATTERNS,
                "Log patterns detected",
                matched,
            ));
        }
    }

    // ── 3. Env file placeholder values ────────────────────────────────────────
    struct EnvCheck {
        label: &'static str,
        ws_path: std::path::PathBuf,
        repo_path: std::path::PathBuf,
        vars: &'static [crate::workspace::EnvVar],
        product: &'static str,
    }
    let env_checks = [
        EnvCheck {
            label: "OpenCTI .env.dev",
            ws_path: ws_env_dir.join("opencti.env"),
            repo_path: paths
                .opencti
                .join("opencti-platform/opencti-graphql/.env.dev"),
            vars: OPENCTI_ENV_VARS,
            product: "OpenCTI",
        },
        EnvCheck {
            label: "Connector .env.dev",
            ws_path: ws_env_dir.join("connector.env"),
            repo_path: paths.connector.join(".env.dev"),
            vars: CONNECTOR_ENV_VARS,
            product: "ImportDocumentAI connector",
        },
    ];
    for ec in &env_checks {
        let check_path = if ec.ws_path.exists() {
            &ec.ws_path
        } else {
            &ec.repo_path
        };
        if !check_path.exists() {
            continue;
        }
        let bad_keys: Vec<String> = parse_env_file(check_path)
            .into_iter()
            .filter(|(_, v)| v == "ChangeMe")
            .map(|(k, _)| format!("  — {k} is still 'ChangeMe'"))
            .collect();
        if !bad_keys.is_empty() {
            findings.push(Finding::fixable(
                KIND_ENV_PLACEHOLDER,
                format!("Placeholder credentials in {}", ec.label),
                bad_keys,
                FixAction::EnvWizard {
                    env_path: ec.ws_path.clone(),
                    deploy_to: Some(ec.repo_path.clone()),
                    vars: ec.vars,
                    product: ec.product,
                    restart_after: false,
                },
            ));
        }
    }

    // ── 4. Python venv missing ────────────────────────────────────────────────
    if svc.name.contains("backend") || svc.name.contains("worker") || svc.name.contains("connector")
    {
        let backend_dir = if repo_dir.join("backend").is_dir() {
            repo_dir.join("backend")
        } else {
            repo_dir.to_path_buf()
        };
        let venv_python = backend_dir.join(".venv/bin/python");
        if !venv_python.exists() {
            findings.push(Finding::fixable(
                KIND_PYTHON_VENV,
                "Python virtual environment missing",
                vec![format!("  Expected: {}", venv_python.display())],
                FixAction::Steps {
                    label: "Create Python virtual environment and install dependencies".into(),
                    steps: venv_fix_steps(&backend_dir),
                    restart_after: true,
                },
            ));
        }
    }

    // ── 5. node_modules missing ───────────────────────────────────────────────
    if svc.name.contains("frontend") {
        let fe_candidates = [repo_dir.join("frontend"), repo_dir.to_path_buf()];
        for fe_dir in &fe_candidates {
            if fe_dir.is_dir() && !fe_dir.join("node_modules").is_dir() {
                findings.push(Finding::fixable(
                    KIND_NODE_MODULES,
                    "JavaScript dependencies not installed",
                    vec![format!("  node_modules missing in {}", fe_dir.display())],
                    FixAction::Steps {
                        label: "Install JavaScript dependencies (yarn install)".into(),
                        steps: vec![FixStep::new(&["yarn", "install"], fe_dir)],
                        restart_after: false,
                    },
                ));
                break;
            }
        }
    }

    // ── 6. Bootstrap RunIfMissing steps from .dev-launcher.conf ──────────────
    let conf_path = repo_dir.join(".dev-launcher.conf");
    if let Some(manifest) = parse_dev_launcher_conf(&conf_path) {
        for step in &manifest.bootstrap {
            match step {
                BootstrapDef::Check { path, missing_hint } => {
                    if !repo_dir.join(path).exists() {
                        findings.push(Finding::info(
                            KIND_INFO_BOOTSTRAP_CHECK,
                            "Bootstrap check failed",
                            vec![format!("  {missing_hint}")],
                        ));
                    }
                }
                BootstrapDef::RunIfMissing {
                    check,
                    command,
                    cwd,
                } => {
                    if !repo_dir.join(check).exists() {
                        if let Some((prog, args)) = command.split_first() {
                            let work_dir = cwd
                                .as_deref()
                                .map(|c| repo_dir.join(c))
                                .unwrap_or_else(|| repo_dir.to_path_buf());
                            let mut all_args = vec![prog.as_str()];
                            all_args.extend(args.iter().map(|s| s.as_str()));
                            findings.push(Finding::fixable(
                                KIND_BOOTSTRAP_RUN,
                                format!("Bootstrap step needed: {}", prog),
                                vec![format!("  Triggers when {} is missing", check)],
                                FixAction::Steps {
                                    label: command.join(" ").to_string(),
                                    steps: vec![FixStep::new(&all_args, &work_dir)],
                                    restart_after: false,
                                },
                            ));
                        }
                    }
                }
                BootstrapDef::SyncPip { .. } | BootstrapDef::SyncYarn { .. } => {
                    // Handled at startup; nothing to diagnose here.
                }
            }
        }
    }

    // ── 7. Recent log tail ────────────────────────────────────────────────────
    let tail = tail_file(&svc.log_path, 20);
    if !tail.is_empty() {
        use crate::tui::{DIM, R};
        let body = tail.iter().map(|l| format!("  {DIM}{l}{R}")).collect();
        findings.push(Finding::info(KIND_INFO_LOG_TAIL, "Recent log output", body));
    }

    if findings.is_empty() {
        findings.push(Finding::info(
            KIND_INFO_NO_ISSUES,
            "No issues detected",
            vec![
                "  Service appears healthy.".into(),
                format!("  Log: {}", svc.log_path.display()),
            ],
        ));
    }

    findings
}
