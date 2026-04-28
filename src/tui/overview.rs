use std::path::Path;

use crate::services::{Health, Svc};
use crate::tui::{pad_ansi, BOLD, BUILD_VERSION, CYN, DIM, GRN, R, RED, YLW};

pub fn build_overview_lines(
    svcs: &[Svc],
    slug: &str,
    logs_dir: &Path,
    cursor: usize,
    has_tui: bool,
    show_paths: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "\n  {BOLD}{CYN}{BUILD_VERSION}{R}  {DIM}{slug}{R}\n"
    ));

    out.push(format!(
        "  {BOLD}  {:<26}{:<32}{:<7}{R}",
        "Service", "Status", "PID"
    ));
    out.push(format!("  {DIM}  {}{R}", "─".repeat(67)));

    let visible: Vec<usize> = svcs
        .iter()
        .enumerate()
        .filter(|(_, s)| s.health != Health::Pending)
        .map(|(i, _)| i)
        .collect();

    for (row, &i) in visible.iter().enumerate() {
        let s = &svcs[i];
        let pid = s.pid.map(|p| p.to_string()).unwrap_or_default();
        let url_str = s.url.as_deref().unwrap_or("");
        let elapsed = s.started_at.map(|_| s.secs());

        let (marker, name_str) = if has_tui && row == cursor {
            (format!("{CYN}{BOLD}▶{R} "), format!("{BOLD}{}{R}", s.name))
        } else {
            ("  ".to_string(), s.name.to_string())
        };

        let restart_marker = if s.recently_restarted() {
            format!("{BOLD}{GRN}↺{R} ")
        } else {
            String::new()
        };
        let status_col = if has_tui && row == cursor {
            pad_ansi(&format!("{restart_marker}{}", s.health.label_plain()), 32)
        } else {
            pad_ansi(&format!("{restart_marker}{}", s.health.label()), 32)
        };

        let mut line = format!("  {marker}{:<26}{status_col}{:<7}", name_str, pid);
        if !url_str.is_empty() {
            line.push_str(&format!("  {DIM}{url_str}{R}"));
        }
        if let Some(s) = elapsed {
            line.push_str(&format!("  {DIM}{s}s{R}"));
        }
        out.push(line);

        if let Some(diag) = &s.diagnosis {
            out.push(format!("        {YLW}▸ {diag}{R}"));
        }

        if show_paths {
            if let Some(dir) = s.spawn_cmd.as_ref().map(|c| c.dir.display().to_string()) {
                out.push(format!("        {DIM}{dir}{R}"));
            }
        }
    }

    out.push(String::new());

    let active: Vec<_> = svcs
        .iter()
        .filter(|s| s.health != Health::Pending)
        .collect();
    let all_up = !active.is_empty()
        && active
            .iter()
            .all(|s| matches!(s.health, Health::Up | Health::Running));
    let any_bad = active
        .iter()
        .any(|s| matches!(s.health, Health::Crashed(_) | Health::Degraded(_)));

    if any_bad {
        out.push(format!("  {RED}{BOLD}One or more services failed.{R}"));
    } else if all_up {
        out.push(format!("  {GRN}{BOLD}All services up.{R}"));
    } else {
        out.push("  Waiting for services…".to_string());
    }

    if has_tui {
        let paths_hint = if show_paths {
            "p hide paths"
        } else {
            "p paths"
        };
        out.push(format!("  {DIM}↑↓ navigate   Enter/→ logs   d diagnose   R restart   s stop   ^R restart all   e credentials   o code   {paths_hint}   m menu   q quit{R}"));
    } else {
        out.push(format!(
            "  {DIM}Ctrl+C to stop   tail -f {}/*.log{R}",
            logs_dir.display()
        ));
    }
    out.push(String::new());
    out
}
