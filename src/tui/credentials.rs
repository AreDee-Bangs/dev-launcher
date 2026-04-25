use std::path::Path;

use crate::services::Paths;
use crate::tui::{BOLD, BUILD_VERSION, CYN, DIM, GRN, R};
use crate::workspace::parse_env_file;

pub struct CredEntry {
    pub product: &'static str,
    pub label: &'static str,
    pub value: String,
}

/// Read a key from a parsed .env map, falling back to `default` when absent.
fn env_or<'a>(map: &'a std::collections::HashMap<String, String>, key: &str, default: &'a str) -> &'a str {
    map.get(key).map(|s| s.as_str()).unwrap_or(default)
}

/// Collect user-facing credentials from each product's workspace .env file.
pub fn gather_credentials(ws_env_dir: &Path, paths: &Paths) -> Vec<CredEntry> {
    let mut out: Vec<CredEntry> = Vec::new();

    let copilot_env = ws_env_dir.join("copilot.env");
    if copilot_env.exists() {
        let map = parse_env_file(&copilot_env);
        for (key, label) in [
            ("ADMIN_EMAIL", "Admin e-mail"),
            ("ADMIN_PASSWORD", "Admin password"),
        ] {
            if let Some(v) = map.get(key) {
                out.push(CredEntry {
                    product: "Copilot",
                    label,
                    value: v.clone(),
                });
            }
        }
    }

    let opencti_env = ws_env_dir.join("opencti.env");
    if opencti_env.exists() {
        let map = parse_env_file(&opencti_env);
        for (key, label) in [
            ("APP__ADMIN__EMAIL", "Admin e-mail"),
            ("APP__ADMIN__PASSWORD", "Admin password"),
            ("APP__ADMIN__TOKEN", "API token"),
        ] {
            if let Some(v) = map.get(key) {
                out.push(CredEntry {
                    product: "OpenCTI",
                    label,
                    value: v.clone(),
                });
            }
        }
    }

    let openaev_env = ws_env_dir.join("openaev.env");
    if openaev_env.exists() {
        let map = parse_env_file(&openaev_env);
        for (key, label) in [
            ("PGADMIN_USER", "pgAdmin e-mail"),
            ("PGADMIN_PASSWORD", "pgAdmin password"),
        ] {
            if let Some(v) = map.get(key) {
                out.push(CredEntry {
                    product: "OpenAEV",
                    label,
                    value: v.clone(),
                });
            }
        }
    }

    let connector_env = ws_env_dir.join("connector.env");
    if connector_env.exists() {
        let map = parse_env_file(&connector_env);
        if let Some(v) = map.get("OPENCTI_TOKEN") {
            out.push(CredEntry {
                product: "Connector",
                label: "OpenCTI token",
                value: v.clone(),
            });
        }
    }

    // Langfuse -- reads from the infra .env, falls back to compose defaults.
    if paths.langfuse.is_dir() {
        let map = parse_env_file(&paths.langfuse.join(".env"));
        let port = env_or(&map, "LANGFUSE_PORT", "3201");
        for (label, key, default) in [
            ("URL",           "",                      ""),
            ("Admin e-mail",  "LANGFUSE_ADMIN_EMAIL",  "admin@example.com"),
            ("Admin password","LANGFUSE_ADMIN_PASSWORD","changeme"),
            ("Public key",    "LANGFUSE_PUBLIC_KEY",   "lf_pk_dev_changeme_publickey"),
            ("Secret key",    "LANGFUSE_SECRET_KEY",   "lf_sk_dev_changeme_secretkey"),
        ] {
            let value = if label == "URL" {
                format!("http://localhost:{port}")
            } else {
                env_or(&map, key, default).to_string()
            };
            out.push(CredEntry { product: "Langfuse", label, value });
        }
    }

    out
}

pub fn build_credentials_lines(creds: &[CredEntry], slug: &str) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}  {BOLD}— credentials{R}\n"
    ));

    let mut current_product = "";
    for entry in creds {
        if entry.product != current_product {
            current_product = entry.product;
            out.push(format!("  {BOLD}{current_product}{R}"));
            out.push(format!("  {DIM}{}{R}", "─".repeat(50)));
        }
        out.push(format!("  {:<24}{GRN}{}{R}", entry.label, entry.value));
    }

    if creds.is_empty() {
        out.push(format!(
            "  {DIM}No .env files found. Run the stack at least once to generate them.{R}"
        ));
    }

    out.push(String::new());
    out.push(format!("  {DIM}q/Esc back{R}"));
    out.push(String::new());
    out
}
