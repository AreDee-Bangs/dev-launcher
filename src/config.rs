use std::fs;
use std::path::PathBuf;

use crate::args::Args;

pub struct DevConfig {
    pub workspace_root: PathBuf,
    /// API key sent to the provider. Can also be set via FILIGRAN_LLM_KEY env var.
    /// May be empty for local providers like Ollama that require no authentication.
    pub llm_api_key: Option<String>,
    /// Base URL of the LLM provider, e.g.:
    ///   https://api.anthropic.com/v1          (Anthropic — default when key starts sk-ant-)
    ///   https://api.openai.com/v1             (OpenAI — default for other keys)
    ///   http://localhost:4000/v1              (LiteLLM proxy)
    ///   http://localhost:11434/v1             (Ollama)
    ///   https://<endpoint>.openai.azure.com/openai/deployments/<name>
    pub llm_url: Option<String>,
    /// Force provider format: "anthropic" or "openai" (auto-inferred from URL when omitted).
    pub llm_provider: Option<String>,
    /// Model name override — defaults to claude-haiku-4-5-20251001 (Anthropic) or gpt-4o-mini.
    pub llm_model: Option<String>,
}

/// `~/.dev-launcher/config`
pub fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".dev-launcher/config")
}

/// Expand a leading `~/` to the real home directory.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(s)
    }
}

pub fn load_config() -> Option<DevConfig> {
    use crate::tui::{DIM, R, YLW};
    let path = config_path();
    if !path.exists() {
        return None;
    }
    let map = crate::workspace::env::parse_env_file(&path);
    let root_str = map.get("workspace_root")?;
    let root = expand_tilde(root_str);
    if root.is_dir() {
        Some(DevConfig {
            workspace_root: root,
            llm_api_key: map.get("llm_api_key").cloned(),
            llm_url: map.get("llm_url").cloned(),
            llm_provider: map.get("llm_provider").cloned(),
            llm_model: map.get("llm_model").cloned(),
        })
    } else {
        // Saved path is stale — fall through to wizard.
        println!(
            "  {YLW}⚠{R}  Config workspace_root no longer exists: {}",
            root.display()
        );
        println!("  {DIM}(saved in {}){R}", config_path().display());
        println!();
        None
    }
}

pub fn save_config(config: &DevConfig) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut content = format!("workspace_root={}\n", config.workspace_root.display());
    if let Some(k) = &config.llm_api_key {
        content.push_str(&format!("llm_api_key={k}\n"));
    }
    if let Some(u) = &config.llm_url {
        content.push_str(&format!("llm_url={u}\n"));
    }
    if let Some(p) = &config.llm_provider {
        content.push_str(&format!("llm_provider={p}\n"));
    }
    if let Some(m) = &config.llm_model {
        content.push_str(&format!("llm_model={m}\n"));
    }
    let _ = fs::write(&path, content);
}

/// Resolve the workspace root. Priority:
///   1. `--workspace-root <path>` CLI flag
///   2. `FILIGRAN_WORKSPACE_ROOT` env var
///   3. `workspace_root` key in `~/.config/dev-launcher/config`
///   4. Interactive first-run wizard (saves result to config file)
pub fn resolve_workspace_root(args: &Args) -> PathBuf {
    // 1. CLI flag
    if let Some(root) = &args.workspace_root {
        let root = if root.starts_with("~/") {
            expand_tilde(root.to_str().unwrap_or(""))
        } else {
            root.clone()
        };
        if root.is_dir() {
            return root;
        }
        eprintln!("--workspace-root '{}' is not a directory.", root.display());
        std::process::exit(1);
    }

    // 2. Env var
    if let Ok(raw) = std::env::var("FILIGRAN_WORKSPACE_ROOT") {
        let root = expand_tilde(raw.trim());
        if root.is_dir() {
            return root;
        }
        eprintln!(
            "FILIGRAN_WORKSPACE_ROOT='{}' is not a directory.",
            root.display()
        );
        std::process::exit(1);
    }

    // 3. Config file
    if let Some(cfg) = load_config() {
        return cfg.workspace_root;
    }

    // 4. First-run wizard
    run_config_wizard()
}

/// First-run interactive wizard: ask for workspace root, optionally clone repos, persist.
fn run_config_wizard() -> PathBuf {
    use crate::tui::{BOLD, BUILD_VERSION, CYN, DIM, GRN, R, RED};
    use crate::workspace::env::{parse_env_file, write_env_file};
    use crate::workspace::repos::{clone_repos, load_repos, run_clone_selector, CloneChoice};
    use std::io::{self, Write};

    let sep = "─".repeat(60);
    println!("\n  {BOLD}{CYN}{BUILD_VERSION}  —  first-run setup{R}\n");
    println!("  {DIM}{sep}{R}");
    println!("  Workspace root not configured.\n");
    println!("  Enter the directory that will contain:");
    println!("  {DIM}  filigran-copilot/   opencti/   connectors/   openaev/{R}\n");
    println!("  The directory can be new — repositories can be cloned for you.");
    println!();
    println!("  You can override this setting later with:");
    println!("  {DIM}  --workspace-root <path>{R}");
    println!("  {DIM}  FILIGRAN_WORKSPACE_ROOT=<path>{R}");
    println!("  {DIM}  edit {}{R}", config_path().display());
    println!("\n  {DIM}{sep}{R}\n");

    // suppress unused warnings
    let _ = parse_env_file;
    let _ = write_env_file;

    loop {
        print!("  Workspace root path: ");
        let _ = io::stdout().flush();
        let input = match read_line_or_interrupt() {
            None => {
                println!("\n  {YLW}Aborted.{R}", YLW = crate::tui::YLW);
                std::process::exit(0);
            }
            Some(s) => s,
        };
        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }

        let candidate = expand_tilde(trimmed);

        // Create directory if it doesn't exist.
        if !candidate.exists() {
            print!("  Directory does not exist. Create it? {DIM}[Y/n]{R} ");
            let _ = io::stdout().flush();
            match read_line_or_interrupt() {
                None => {
                    println!("\n  {YLW}Aborted.{R}", YLW = crate::tui::YLW);
                    std::process::exit(0);
                }
                Some(s) if matches!(s.trim().to_ascii_lowercase().as_str(), "n" | "no") => {
                    println!("  Skipped.");
                    continue;
                }
                _ => {
                    if let Err(e) = fs::create_dir_all(&candidate) {
                        println!("  {RED}✗{R}  Could not create directory: {e}");
                        continue;
                    }
                    println!("  {GRN}✓{R}  Created {}", candidate.display());
                }
            }
        }

        if !candidate.is_dir() {
            println!("  {RED}✗{R}  Not a directory: {}", candidate.display());
            continue;
        }

        // Offer to clone any repositories not yet present.
        let repos = load_repos();
        let mut clone_choices: Vec<CloneChoice> = repos
            .into_iter()
            .map(|entry| {
                let present = candidate.join(&entry.dir).is_dir();
                CloneChoice {
                    entry,
                    enabled: !present,
                    present,
                }
            })
            .collect();

        let any_missing = clone_choices.iter().any(|c| !c.present);
        if any_missing {
            println!();
            println!("  {DIM}Some repositories are not yet present in this directory.{R}");
            if run_clone_selector(&candidate, &mut clone_choices) {
                let any_selected = clone_choices.iter().any(|c| c.enabled && !c.present);
                if any_selected {
                    clone_repos(&candidate, &clone_choices);
                }
            } else {
                println!("  {DIM}Cloning skipped — you can run git clone manually later.{R}\n");
            }
        }

        let cfg = DevConfig {
            workspace_root: candidate.clone(),
            llm_api_key: None,
            llm_url: None,
            llm_provider: None,
            llm_model: None,
        };
        save_config(&cfg);
        println!("  {GRN}✓{R}  Saved → {}", config_path().display());
        println!();
        return candidate;
    }
}

/// Read one line from stdin.
///
/// Returns `None` on:
/// - Ctrl+C  (SIGINT interrupts the blocking read → `Err(Interrupted)`)
/// - Ctrl+D  (EOF → `Ok(0)` bytes read)
/// - Any other I/O error
///
/// Returns `Some(line)` otherwise, with the trailing newline stripped.
pub fn read_line_or_interrupt() -> Option<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                byte.as_mut_ptr() as *mut libc::c_void,
                1,
            )
        };
        if n <= 0 {
            return None;
        }
        match byte[0] {
            b'\n' => break,
            b'\r' => continue, // skip CR from CRLF terminals (ICRNL may produce double newline)
            b => buf.push(b),
        }
    }
    String::from_utf8(buf)
        .ok()
        .map(|s| s.trim_end().to_string())
}
