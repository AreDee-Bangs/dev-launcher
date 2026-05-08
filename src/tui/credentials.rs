use std::path::Path;

use crate::services::Paths;
use crate::tui::{BOLD, BUILD_VERSION, CYN, DIM, GRN, R, YLW};
use crate::workspace::parse_env_file;

pub struct CredEntry {
    pub product: &'static str,
    pub label: &'static str,
    pub value: String,
    /// When true, renders as a full-width dimmed note line (value is ignored).
    pub note: bool,
}

impl CredEntry {
    fn entry(product: &'static str, label: &'static str, value: impl Into<String>) -> Self {
        Self {
            product,
            label,
            value: value.into(),
            note: false,
        }
    }
    fn note(product: &'static str, text: &'static str) -> Self {
        Self {
            product,
            label: text,
            value: String::new(),
            note: true,
        }
    }
}

/// Read a key from a parsed .env map, falling back to `default` when absent.
fn env_or<'a>(
    map: &'a std::collections::HashMap<String, String>,
    key: &str,
    default: &'a str,
) -> &'a str {
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
                out.push(CredEntry::entry("Copilot", label, v.clone()));
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
                out.push(CredEntry::entry("OpenCTI", label, v.clone()));
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
                out.push(CredEntry::entry("OpenAEV", label, v.clone()));
            }
        }
    }

    let connector_env = ws_env_dir.join("connector.env");
    if connector_env.exists() {
        let map = parse_env_file(&connector_env);
        if let Some(v) = map.get("OPENCTI_TOKEN") {
            out.push(CredEntry::entry("Connector", "OpenCTI token", v.clone()));
        }
    }

    // AutoResearch -- API key and URL are stored in the copilot workspace env.
    if copilot_env.exists() {
        let map = parse_env_file(&copilot_env);
        if let (Some(url), Some(key)) =
            (map.get("AUTORESEARCH_URL"), map.get("AUTORESEARCH_API_KEY"))
        {
            out.push(CredEntry::entry("AutoResearch", "Runner URL", url.clone()));
            out.push(CredEntry::entry("AutoResearch", "API key", key.clone()));
        }
    }

    // Langfuse -- reads from the infra .env, falls back to compose defaults.
    if paths.langfuse.is_dir() {
        let map = parse_env_file(&paths.langfuse.join(".env"));
        let port = env_or(&map, "LANGFUSE_PORT", "3201");
        let email = env_or(&map, "LANGFUSE_ADMIN_EMAIL", "admin@example.com").to_string();
        let pass = env_or(&map, "LANGFUSE_ADMIN_PASSWORD", "changeme").to_string();
        let pk = env_or(&map, "LANGFUSE_PUBLIC_KEY", "lf_pk_dev_changeme_publickey").to_string();
        let sk = env_or(&map, "LANGFUSE_SECRET_KEY", "lf_sk_dev_changeme_secretkey").to_string();
        let url = format!("http://localhost:{port}");

        out.push(CredEntry::entry("Langfuse", "URL", url.clone()));
        out.push(CredEntry::entry("Langfuse", "Admin e-mail", email));
        out.push(CredEntry::entry("Langfuse", "Admin password", pass));
        out.push(CredEntry::entry("Langfuse", "Public key", pk.clone()));
        out.push(CredEntry::entry("Langfuse", "Secret key", sk.clone()));

        // Copilot Settings hint: the three values to paste in Settings → Langfuse.
        out.push(CredEntry::note(
            "Copilot → Settings → Langfuse",
            "Enable tracing: open Copilot → Settings → Langfuse and enter:",
        ));
        out.push(CredEntry::entry(
            "Copilot → Settings → Langfuse",
            "Host",
            url,
        ));
        out.push(CredEntry::entry(
            "Copilot → Settings → Langfuse",
            "Public key",
            pk,
        ));
        out.push(CredEntry::entry(
            "Copilot → Settings → Langfuse",
            "Secret key",
            sk,
        ));
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
            out.push(String::new());
            out.push(format!("  {BOLD}{current_product}{R}"));
            out.push(format!("  {DIM}{}{R}", "─".repeat(50)));
        }
        if entry.note {
            out.push(format!("  {YLW}↳ {}{R}", entry.label));
        } else {
            out.push(format!("  {:<24}{GRN}{}{R}", entry.label, entry.value));
        }
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
