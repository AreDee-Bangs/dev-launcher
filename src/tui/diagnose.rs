use crate::diagnosis::{needs_recipe, Finding};
use crate::services::{Health, Svc};
use crate::tui::{terminal_size, BOLD, CYN, DIM, ENTER_RUN_FIX, GRN, R, RED, YLW};

pub fn build_diagnose_lines(svc: &Svc, findings: &[Finding], cursor: usize) -> Vec<String> {
    let (cols, rows) = terminal_size();
    let header = 4usize;
    let footer = 2usize;
    let content = rows.saturating_sub(header + footer);
    let sep = "─".repeat(cols.saturating_sub(4));

    let mut out = Vec::new();

    out.push(String::new());
    let mut hdr = format!("  {BOLD}{CYN}{}{R}  {BOLD}diagnosis{R}", svc.name);
    match &svc.health {
        Health::Crashed(c) => hdr.push_str(&format!("  {RED}crashed ({c}){R}")),
        Health::Degraded(m) => hdr.push_str(&format!("  {RED}degraded{R}  {DIM}{m}{R}")),
        Health::Up => hdr.push_str(&format!("  {GRN}up{R}")),
        other => hdr.push_str(&format!("  {DIM}{}{R}", other.label_plain())),
    }
    out.push(hdr);
    out.push(format!("  {DIM}{}{R}", svc.log_path.display()));
    out.push(format!("  {DIM}{sep}{R}"));

    let mut lines: Vec<String> = Vec::new();
    for (i, f) in findings.iter().enumerate() {
        let marker = if i == cursor {
            format!("{CYN}{BOLD}▶{R} ")
        } else {
            "  ".to_string()
        };
        let check = if f.resolved {
            format!("{GRN}✓{R}")
        } else if f.fix.is_some() {
            format!("{YLW}●{R}")
        } else {
            format!("{DIM}·{R}")
        };
        lines.push(format!("  {marker}{check}  {BOLD}{}{R}", f.title));
        for b in &f.body {
            lines.push(format!("       {b}"));
        }
        if let Some(fix) = &f.fix {
            if f.resolved {
                lines.push(format!("       {GRN}✓ Fixed{R}"));
            } else {
                lines.push(format!(
                    "       \x1b[1;38;5;214m→ Enter to run:\x1b[0m  {}",
                    fix.label()
                ));
            }
        } else if needs_recipe(f) {
            lines.push(format!("       {DIM}no recipe yet — press r to report{R}"));
        }
        lines.push(String::new());
    }

    let cursor_line = {
        let mut n = 0usize;
        for (i, f) in findings.iter().enumerate() {
            if i == cursor {
                break;
            }
            let extra = if f.fix.is_some() || needs_recipe(f) {
                1
            } else {
                0
            };
            n += 2 + f.body.len() + extra + 1;
        }
        n
    };
    let start = cursor_line.min(lines.len().saturating_sub(content));
    let end = (start + content).min(lines.len());
    let page = &lines[start..end];

    for line in page {
        out.push(line.clone());
    }
    for _ in page.len()..content {
        out.push(String::new());
    }

    out.push(format!("  {DIM}{sep}{R}"));
    let fixable_count = findings
        .iter()
        .filter(|f| f.fix.is_some() && !f.resolved)
        .count();
    let cursor_reportable = findings.get(cursor).map(needs_recipe).unwrap_or(false);
    if fixable_count > 0 && cursor_reportable {
        out.push(format!("  {DIM}↑↓ navigate   {R}{ENTER_RUN_FIX}{DIM}   r report   q / ← back{R}   {YLW}{fixable_count} fix(es) available{R}"));
    } else if fixable_count > 0 {
        out.push(format!("  {DIM}↑↓ navigate   {R}{ENTER_RUN_FIX}{DIM}   q / ← back{R}   {YLW}{fixable_count} fix(es) available{R}"));
    } else if cursor_reportable {
        out.push(format!(
            "  {DIM}↑↓ navigate   r report missing recipe   q / ← back{R}"
        ));
    } else {
        out.push(format!("  {DIM}↑↓ navigate   q / ← back{R}"));
    }
    out
}
