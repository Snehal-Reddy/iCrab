//! Workspace paths: memory/MEMORY.md, memory/YYYYMM/YYYYMMDD.md, sessions/<chat_id>.json, skills/<name>/SKILL.md, cron/jobs.json, bootstrap files.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Datelike, NaiveDate, Utc};

const MEMORY_SUMMARY_LEN: usize = 4000;
const DAILY_NOTE_SUMMARY_LEN: usize = 2000;
/// Number of recent daily note days to include in memory snippet (today + N-1 previous).
pub const RECENT_DAILY_DAYS: u32 = 3;

/// Current date in UTC as "YYYYMMDD" for daily note paths and memory context.
#[inline]
pub fn today_yyyymmdd() -> String {
    let d = Utc::now().date_naive();
    format!("{:04}{:02}{:02}", d.year(), d.month(), d.day())
}

/// Path to the skills directory under the workspace: `workspace/skills`.
#[inline]
pub fn skills_dir(workspace: &Path) -> PathBuf {
    workspace.join("skills")
}

/// Path to the sessions directory: `workspace/sessions`.
#[inline]
pub fn sessions_dir(workspace: &Path) -> PathBuf {
    workspace.join("sessions")
}

/// Safe filename from chat_id (alphanumeric + hyphen/underscore only). Falls back to "default" if empty.
#[inline]
pub fn session_file(workspace: &Path, chat_id: &str) -> PathBuf {
    let safe: String = chat_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = if safe.is_empty() {
        "default"
    } else {
        safe.as_str()
    };
    sessions_dir(workspace).join(format!("{name}.json"))
}

/// Path to the memory directory: `workspace/memory`.
#[inline]
pub fn memory_dir(workspace: &Path) -> PathBuf {
    workspace.join("memory")
}

/// Path to MEMORY.md: `workspace/memory/MEMORY.md`.
#[inline]
pub fn memory_file(workspace: &Path) -> PathBuf {
    memory_dir(workspace).join("MEMORY.md")
}

/// Path to a daily note: `workspace/memory/YYYYMM/YYYYMMDD.md`. `yyyymmdd` must be "YYYYMMDD".
#[inline]
pub fn daily_note_path(workspace: &Path, yyyymmdd: &str) -> PathBuf {
    let (yymm, _) = yyyymmdd.split_at(yyyymmdd.len().min(6));
    memory_dir(workspace)
        .join(yymm)
        .join(format!("{yyyymmdd}.md"))
}

/// Path to AGENT.md in workspace root.
#[inline]
pub fn agent_md(workspace: &Path) -> PathBuf {
    workspace.join("AGENT.md")
}

/// Path to USER.md in workspace root.
#[inline]
pub fn user_md(workspace: &Path) -> PathBuf {
    workspace.join("USER.md")
}

/// Path to IDENTITY.md in workspace root.
#[inline]
pub fn identity_md(workspace: &Path) -> PathBuf {
    workspace.join("IDENTITY.md")
}

/// Path to cron jobs file: `workspace/cron/jobs.json`.
#[inline]
pub fn cron_jobs_file(workspace: &Path) -> PathBuf {
    workspace.join("cron").join("jobs.json")
}

/// Path to the iCrab data directory: `workspace/.icrab/`.
/// Contains SQLite database and other runtime state ignored by Git.
#[inline]
pub fn icrab_dir(workspace: &Path) -> PathBuf {
    workspace.join(".icrab")
}

/// Path to the SQLite brain database: `workspace/.icrab/brain.db`.
#[inline]
pub fn brain_db_path(workspace: &Path) -> PathBuf {
    icrab_dir(workspace).join("brain.db")
}

/// Parse "YYYYMMDD" into Date. Returns None if invalid.
fn parse_yyyymmdd(s: &str) -> Option<NaiveDate> {
    if s.len() != 8 {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let m: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    NaiveDate::from_ymd_opt(y, m, d)
}

/// Dates from today going back (today_yyyymmdd, yesterday, …). At most `days` entries.
fn recent_daily_dates(today_yyyymmdd: &str, days: u32) -> Option<Vec<String>> {
    let mut date = parse_yyyymmdd(today_yyyymmdd)?;
    let mut out = Vec::with_capacity(days as usize);
    for _ in 0..days {
        out.push(format!(
            "{:04}{:02}{:02}",
            date.year(),
            date.month(),
            date.day()
        ));
        date = date.pred_opt()?;
    }
    Some(out)
}

/// Read MEMORY.md and optionally recent daily notes (today + last N days), truncated, for context.
/// When `today_yyyymmdd` is None, only MEMORY.md is read. When `recent_days` is 0, only MEMORY.md + today (if provided).
pub fn read_memory_snippet(
    workspace: &Path,
    today_yyyymmdd: Option<&str>,
    recent_days: u32,
) -> String {
    let cap =
        MEMORY_SUMMARY_LEN + (recent_days as usize).saturating_mul(DAILY_NOTE_SUMMARY_LEN + 32);
    let mut out = String::with_capacity(cap);
    let mem_path = memory_file(workspace);
    if let Ok(s) = fs::read_to_string(&mem_path) {
        let t = s.trim();
        if t.len() > MEMORY_SUMMARY_LEN {
            out.push_str(&t[..MEMORY_SUMMARY_LEN]);
            out.push_str("…\n");
        } else if !t.is_empty() {
            out.push_str(t);
            out.push('\n');
        }
    }
    let days_to_read = if recent_days == 0 {
        today_yyyymmdd
            .map(|s| vec![s.to_string()])
            .unwrap_or_default()
    } else if let Some(today) = today_yyyymmdd {
        recent_daily_dates(today, recent_days).unwrap_or_else(|| vec![today.to_string()])
    } else {
        Vec::new()
    };
    for yyyymmdd in days_to_read {
        let daily = daily_note_path(workspace, &yyyymmdd);
        if daily == mem_path {
            continue;
        }
        if let Ok(s) = fs::read_to_string(&daily) {
            let t = s.trim();
            if t.is_empty() {
                continue;
            }
            out.push_str("\n--- ");
            out.push_str(&yyyymmdd);
            out.push_str(" ---\n");
            if t.len() > DAILY_NOTE_SUMMARY_LEN {
                out.push_str(&t[..DAILY_NOTE_SUMMARY_LEN]);
                out.push_str("…");
            } else {
                out.push_str(t);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn session_file_safe_filename() {
        let w = std::path::Path::new("/ws");
        assert!(
            session_file(w, "123")
                .to_string_lossy()
                .ends_with("123.json")
        );
        assert!(session_file(w, "ab:c").to_string_lossy().contains("ab_c"));
        assert!(
            session_file(w, "")
                .to_string_lossy()
                .ends_with("default.json")
        );
    }

    #[test]
    fn daily_note_path_shape() {
        let w = std::path::Path::new("/ws");
        let p = daily_note_path(w, "20250216");
        assert!(p.to_string_lossy().contains("202502"));
        assert!(p.to_string_lossy().ends_with("20250216.md"));
    }

    #[test]
    fn read_memory_snippet_none_missing() {
        let tmp = std::env::temp_dir().join("icrab_mem_test_none");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let out = read_memory_snippet(&tmp, None, 0);
        assert!(out.is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_memory_snippet_memory_only() {
        let tmp = std::env::temp_dir().join("icrab_mem_test_mem");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("memory")).unwrap();
        fs::write(tmp.join("memory").join("MEMORY.md"), "Hello long-term.").unwrap();
        let out = read_memory_snippet(&tmp, None, 0);
        assert!(out.contains("Hello long-term"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_memory_snippet_with_daily_note() {
        let tmp = std::env::temp_dir().join("icrab_mem_test_daily");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("memory").join("202502")).unwrap();
        fs::write(tmp.join("memory").join("MEMORY.md"), "Mem.").unwrap();
        fs::write(
            tmp.join("memory").join("202502").join("20250216.md"),
            "Today note.",
        )
        .unwrap();
        let out = read_memory_snippet(&tmp, Some("20250216"), 1);
        assert!(out.contains("Mem."));
        assert!(out.contains("20250216"));
        assert!(out.contains("Today note"));
        let _ = fs::remove_dir_all(&tmp);
    }
}
