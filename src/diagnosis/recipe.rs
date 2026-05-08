use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::diagnosis::{Finding, FixAction, FixStep};
use crate::services::Svc;
use crate::tui::logview::tail_file;

static RECIPES: OnceLock<Vec<Recipe>> = OnceLock::new();

#[derive(Deserialize)]
pub struct Recipe {
    pub id: String,
    pub health: String,
    pub service: Option<String>,
    pub service_prefix: Option<String>,
    pub log_tail: Option<usize>,
    #[serde(rename = "match", default)]
    pub matches: Vec<MatchBlock>,
    pub title: String,
    pub body: Vec<String>,
    pub fix: Option<RecipeFix>,
}

#[derive(Deserialize)]
pub struct MatchBlock {
    pub any_of: Option<Vec<String>>,
    pub all_of: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct RecipeFix {
    pub label: String,
    pub restart_after: bool,
    pub steps: Vec<RecipeStep>,
}

#[derive(Deserialize)]
pub struct RecipeStep {
    pub command: Vec<String>,
    pub cwd: String,
}

/// Load recipes from `recipes_dir` into the global store.
/// Call this after the splash screen has synced the recipe cache.
pub fn init(recipes_dir: &Path) {
    let recipes = load_from_dir(recipes_dir);
    crate::llog!(
        "[RECIPES] loaded {} recipe(s) from {}",
        recipes.len(),
        recipes_dir.display()
    );
    let _ = RECIPES.set(recipes);
}

fn load_from_dir(dir: &Path) -> Vec<Recipe> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut recipes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        match toml::from_str::<Recipe>(&src) {
            Ok(r) => recipes.push(r),
            Err(e) => crate::llog!("[RECIPES] parse error in {}: {e}", path.display()),
        }
    }
    recipes
}

fn get() -> &'static [Recipe] {
    RECIPES.get().map(|v| v.as_slice()).unwrap_or(&[])
}

/// Evaluate all loaded recipes against a crashed service.
/// Matches recipes with `health = "crashed"` or `health = "any"`.
pub fn apply_to_crash(
    svc: &Svc,
    exit_code: i32,
    backend_dir: &Path,
    frontend_dir: &Path,
    repo_dir: &Path,
) -> Vec<Finding> {
    apply_inner(svc, Some(exit_code), backend_dir, frontend_dir, repo_dir)
}

/// Evaluate all loaded recipes against a degraded service's log output.
/// Matches recipes with `health = "degraded"` or `health = "any"`.
/// Use this to replace the hardcoded DIAG_PATTERNS loop for degraded services.
pub fn apply_to_log_patterns(
    svc: &Svc,
    backend_dir: &Path,
    frontend_dir: &Path,
    repo_dir: &Path,
) -> Vec<Finding> {
    apply_inner(svc, None, backend_dir, frontend_dir, repo_dir)
}

fn apply_inner(
    svc: &Svc,
    exit_code: Option<i32>,
    backend_dir: &Path,
    frontend_dir: &Path,
    repo_dir: &Path,
) -> Vec<Finding> {
    let recipes = get();
    let mut findings = Vec::new();
    let exit_code_str = exit_code.map(|c| c.to_string()).unwrap_or_default();

    for recipe in recipes {
        let health_ok = match recipe.health.as_str() {
            "crashed" => exit_code.is_some(),
            "degraded" => exit_code.is_none(),
            _ => true,
        };
        if !health_ok {
            continue;
        }

        let svc_ok = match (&recipe.service, &recipe.service_prefix) {
            (Some(name), _) => svc.name == *name,
            (_, Some(prefix)) => svc.name.starts_with(prefix.as_str()),
            _ => true,
        };
        if !svc_ok {
            continue;
        }

        let log_lines = tail_file(&svc.log_path, recipe.log_tail.unwrap_or(30));

        let all_match = recipe.matches.iter().all(|block| {
            let any_ok = block.any_of.as_ref().is_none_or(|needles| {
                needles.iter().any(|n| {
                    let nl = n.to_lowercase();
                    log_lines.iter().any(|l| l.to_lowercase().contains(&nl))
                })
            });
            let all_ok = block.all_of.as_ref().is_none_or(|needles| {
                needles.iter().all(|n| {
                    let nl = n.to_lowercase();
                    log_lines.iter().any(|l| l.to_lowercase().contains(&nl))
                })
            });
            any_ok && all_ok
        });
        if !all_match {
            continue;
        }

        let pip = backend_dir
            .join(".venv/bin/pip")
            .to_string_lossy()
            .into_owned();
        let python = backend_dir
            .join(".venv/bin/python")
            .to_string_lossy()
            .into_owned();
        let repo_s = repo_dir.to_string_lossy().into_owned();
        let be_s = backend_dir.to_string_lossy().into_owned();
        let fe_s = frontend_dir.to_string_lossy().into_owned();

        let expand = |s: &str| -> String {
            s.replace("{pip}", &pip)
                .replace("{python}", &python)
                .replace("{repo_dir}", &repo_s)
                .replace("{backend_dir}", &be_s)
                .replace("{frontend_dir}", &fe_s)
                .replace("{exit_code}", &exit_code_str)
        };

        let title = expand(&recipe.title);
        let body: Vec<String> = recipe.body.iter().map(|l| expand(l)).collect();

        let finding = if let Some(fix) = &recipe.fix {
            let steps: Vec<FixStep> = fix
                .steps
                .iter()
                .map(|step| {
                    let args: Vec<String> = step.command.iter().map(|c| expand(c)).collect();
                    let cwd = match step.cwd.as_str() {
                        "backend" => backend_dir.to_path_buf(),
                        "frontend" => frontend_dir.to_path_buf(),
                        "repo" => repo_dir.to_path_buf(),
                        other => repo_dir.join(other),
                    };
                    FixStep { args, cwd }
                })
                .collect();

            Finding::fixable(
                recipe.id.clone(),
                title,
                body,
                FixAction::Steps {
                    label: expand(&fix.label),
                    steps,
                    restart_after: fix.restart_after,
                },
            )
        } else {
            Finding::info(recipe.id.clone(), title, body)
        };

        findings.push(finding);
    }

    findings
}
