use std::fs::File;
use std::io::{self, BufRead};
use std::path::Path;

use crate::services::{Health, Svc};
use crate::tui::{terminal_size, BOLD, CYN, DIM, GRN, R, RED, YLW};

/// Return the last `max_lines` lines from a file.
pub fn tail_file(path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(f) = File::open(path) else {
        return vec![];
    };
    let all: Vec<String> = io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .collect();
    let start = all.len().saturating_sub(max_lines);
    all[start..].to_vec()
}

pub fn build_log_view_lines(svc: &Svc, scroll: usize, follow: bool) -> Vec<String> {
    let (cols, rows) = terminal_size();
    let header = 4usize;
    let footer = 2usize;
    let content = rows.saturating_sub(header + footer);
    let sep = "─".repeat(cols.saturating_sub(4));

    let mut out = Vec::new();

    out.push(String::new());
    let mut hdr = format!("  {BOLD}{CYN}{}{R}", svc.name);
    match &svc.health {
        Health::Up => hdr.push_str(&format!("  {GRN}up{R}")),
        Health::Running => hdr.push_str(&format!("  {CYN}running{R}")),
        Health::Crashed(c) => hdr.push_str(&format!("  {RED}crashed ({c}){R}")),
        Health::Degraded(m) => hdr.push_str(&format!("  {RED}degraded ({m}){R}")),
        other => hdr.push_str(&format!("  {DIM}{}{R}", other.label_plain())),
    }
    if let Some(pid) = svc.pid {
        hdr.push_str(&format!("  {DIM}pid {pid}{R}"));
    }
    out.push(hdr);
    out.push(format!("  {DIM}{}{R}", svc.log_path.display()));
    out.push(format!("  {DIM}{sep}{R}"));

    let lines = tail_file(&svc.log_path, content + scroll + 300);
    let total = lines.len();

    let end = total.saturating_sub(if follow { 0 } else { scroll });
    let start = end.saturating_sub(content);
    let page = &lines[start..end];

    for line in page {
        out.push(format!("  {line}"));
    }
    for _ in page.len()..content {
        out.push(String::new());
    }

    out.push(format!("  {DIM}{sep}{R}"));
    let follow_label = if follow {
        format!("{GRN}following{R}")
    } else {
        format!("{YLW}paused{R}  {DIM}(f = follow){R}")
    };
    out.push(format!(
        "  {DIM}q/← back   ↑↓ scroll   PgUp/PgDn fast   d diagnose{R}   {follow_label}"
    ));
    out
}
