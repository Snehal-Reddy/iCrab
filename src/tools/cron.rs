//! Cron tool: add, list, remove, enable, disable; store in workspace/cron/jobs.json.
//! Cron expression parser (5-field) and CronStore shared with cron_runner.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc};

use crate::tools::context::ToolCtx;
use crate::tools::registry::{BoxFuture, Tool};
use crate::tools::result::ToolResult;
use crate::workspace;

// --- Data types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub label: Option<String>,
    pub message: String,
    pub action: JobAction,
    pub schedule: Schedule,
    pub enabled: bool,
    pub chat_id: i64,
    pub created_at: u64,
    pub last_run: Option<u64>,
    pub next_run: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobAction {
    Agent,
    Direct,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Schedule {
    Once { at_unix: u64 },
    Interval { every_seconds: u64 },
    Cron { expr: String },
}

#[derive(Debug)]
pub enum CronError {
    Io(String),
    Parse(String),
    Validation(String),
}

impl std::fmt::Display for CronError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CronError::Io(s) => write!(f, "cron io: {}", s),
            CronError::Parse(s) => write!(f, "cron parse: {}", s),
            CronError::Validation(s) => write!(f, "cron validation: {}", s),
        }
    }
}

impl std::error::Error for CronError {}

// --- Cron expression ---

pub struct CronExpr {
    pub minutes: Vec<u8>,
    pub hours: Vec<u8>,
    pub doms: Vec<u8>,
    pub months: Vec<u8>,
    pub dows: Vec<u8>,
}

fn parse_field(token: &str, min: u8, max: u8) -> Result<Vec<u8>, CronError> {
    let mut out: Vec<u8> = Vec::new();
    for part in token.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part == "*" {
            for v in min..=max {
                out.push(v);
            }
            continue;
        }
        if let Some(rest) = part.strip_prefix("*/") {
            let step: u8 = rest
                .parse()
                .map_err(|_| CronError::Validation("invalid step".into()))?;
            if step == 0 {
                return Err(CronError::Validation("step must be positive".into()));
            }
            let mut v = min;
            while v <= max {
                out.push(v);
                v = v.saturating_add(step);
            }
            continue;
        }
        if let Some(range) = part.split_once('-') {
            let start: u8 = range
                .0
                .trim()
                .parse()
                .map_err(|_| CronError::Validation("invalid range start".into()))?;
            let end: u8 = range
                .1
                .trim()
                .split('/')
                .next()
                .unwrap_or(range.1.trim())
                .parse()
                .map_err(|_| CronError::Validation("invalid range end".into()))?;
            if start > end {
                return Err(CronError::Validation("range start > end".into()));
            }
            if start < min || end > max {
                return Err(CronError::Validation("range out of bounds".into()));
            }
            let step: u8 = range
                .1
                .trim()
                .split('/')
                .nth(1)
                .map(|s| s.parse().unwrap_or(1))
                .unwrap_or(1);
            let mut v = start;
            while v <= end {
                out.push(v);
                v = v.saturating_add(step);
            }
            continue;
        }
        let single: u8 = part
            .parse()
            .map_err(|_| CronError::Validation("invalid number".into()))?;
        if single < min || single > max {
            return Err(CronError::Validation("value out of range".into()));
        }
        out.push(single);
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        return Err(CronError::Validation("empty field".into()));
    }
    Ok(out)
}

pub fn parse_cron_expr(expr: &str) -> Result<CronExpr, CronError> {
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    if tokens.len() != 5 {
        return Err(CronError::Validation(
            "cron expression must have exactly 5 fields (minute hour dom month dow)".into(),
        ));
    }
    let minutes = parse_field(tokens[0], 0, 59)?;
    let hours = parse_field(tokens[1], 0, 23)?;
    let doms = parse_field(tokens[2], 1, 31)?;
    let months = parse_field(tokens[3], 1, 12)?;
    let dows = parse_field(tokens[4], 0, 6)?;
    Ok(CronExpr {
        minutes,
        hours,
        doms,
        months,
        dows,
    })
}

const LIMIT_YEARS: i32 = 4;

pub fn next_match(expr: &CronExpr, after_unix: u64) -> Option<u64> {
    let start_secs = (after_unix / 60 + 1) * 60;
    let start_secs = start_secs.min(i64::MAX as u64) as i64;
    let mut dt = match DateTime::from_timestamp(start_secs, 0) {
        Some(d) => d,
        None => return None,
    };
    let limit = dt.year() + LIMIT_YEARS;

    while dt.year() <= limit {
        let month_u8 = dt.month() as u8;
        if !expr.months.contains(&month_u8) {
            dt = next_matching_month(dt, expr)?;
            continue;
        }
        let dom = dt.day() as u8;
        let dow = dt.weekday().num_days_from_sunday() as u8;
        if !expr.doms.contains(&dom) || !expr.dows.contains(&dow) {
            dt = dt
                .date_naive()
                .succ_opt()?
                .and_hms_opt(0, 0, 0)?
                .and_utc();
            continue;
        }
        let hour = dt.hour() as u8;
        if !expr.hours.contains(&hour) {
            match expr.hours.iter().find(|&&h| h >= hour) {
                Some(&h) => {
                    dt = dt
                        .date_naive()
                        .and_hms_opt(h as u32, 0, 0)?
                        .and_utc();
                }
                None => {
                    dt = dt
                        .date_naive()
                        .succ_opt()?
                        .and_hms_opt(0, 0, 0)?
                        .and_utc();
                }
            }
            continue;
        }
        let minute = dt.minute() as u8;
        if !expr.minutes.contains(&minute) {
            match expr.minutes.iter().find(|&&m| m >= minute) {
                Some(&m) => {
                    dt = dt
                        .date_naive()
                        .and_hms_opt(hour as u32, m as u32, 0)?
                        .and_utc();
                }
                None => {
                    let (next_date, next_hour) = next_hour_in_expr(dt, expr);
                    dt = next_date
                        .and_hms_opt(next_hour as u32, expr.minutes[0] as u32, 0)?
                        .and_utc();
                }
            }
            continue;
        }
        return Some(dt.timestamp() as u64);
    }
    None
}

fn next_matching_month(dt: DateTime<Utc>, expr: &CronExpr) -> Option<DateTime<Utc>> {
    let mut y = dt.year();
    let mut m = dt.month() as u8;
    for _ in 0..24 {
        if expr.months.contains(&m) {
            let date = NaiveDate::from_ymd_opt(y, m as u32, 1)?;
            return Some(date.and_hms_opt(0, 0, 0)?.and_utc());
        }
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    None
}

fn next_hour_in_expr(dt: DateTime<Utc>, expr: &CronExpr) -> (NaiveDate, u8) {
    let mut date = dt.date_naive();
    let mut hour = dt.hour() as u8;
    loop {
        if let Some(&h) = expr.hours.iter().find(|&&h| h > hour) {
            return (date, h);
        }
        date = match date.succ_opt() {
            Some(d) => d,
            None => return (date, 23),
        };
        hour = 0;
        if let Some(&h) = expr.hours.first() {
            return (date, h);
        }
    }
}

impl Schedule {
    pub fn next_fire_after(&self, after_unix: u64) -> Option<u64> {
        match self {
            Schedule::Once { at_unix } => {
                if *at_unix > after_unix {
                    Some(*at_unix)
                } else {
                    None
                }
            }
            Schedule::Interval { every_seconds } => Some(after_unix + every_seconds),
            Schedule::Cron { expr } => parse_cron_expr(expr)
                .ok()
                .and_then(|e| next_match(&e, after_unix)),
        }
    }
}

// --- CronStore ---

pub struct CronStore {
    jobs: RwLock<Vec<CronJob>>,
    jobs_path: std::path::PathBuf,
    next_id: AtomicU64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse delay string (e.g. "30m", "2h", "1d") into seconds. Units: s, m, h, d, w.
fn parse_delay(input: &str) -> Result<u64, CronError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(CronError::Validation("delay string is empty".into()));
    }
    let (num_str, unit) = if input
        .chars()
        .last()
        .map_or(false, |c| c.is_ascii_alphabetic())
    {
        let split = input.len() - 1;
        (&input[..split], &input[split..])
    } else {
        (input, "m")
    };
    let n: u64 = num_str
        .trim()
        .parse()
        .map_err(|_| CronError::Validation("invalid delay number".into()))?;
    let multiplier: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 604_800,
        _ => {
            return Err(CronError::Validation(
                "unknown delay unit, expected s/m/h/d/w".into(),
            ));
        }
    };
    n.checked_mul(multiplier)
        .ok_or_else(|| CronError::Validation("delay value too large".into()))
}

impl CronStore {
    fn save_inner(jobs: &[CronJob], path: &Path) -> Result<(), CronError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CronError::Io(e.to_string()))?;
        }
        let json =
            serde_json::to_string_pretty(jobs).map_err(|e| CronError::Parse(e.to_string()))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &json).map_err(|e| CronError::Io(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| CronError::Io(e.to_string()))
    }

    pub fn load(workspace: &Path) -> Result<Self, CronError> {
        let jobs_path = workspace::cron_jobs_file(workspace);
        let (jobs, next_id) = match std::fs::read_to_string(&jobs_path) {
            Ok(s) => {
                let file: Vec<CronJob> =
                    serde_json::from_str(&s).map_err(|e| CronError::Parse(e.to_string()))?;
                let max_id = file
                    .iter()
                    .filter_map(|j| {
                        j.id.strip_prefix("job-")
                            .and_then(|n| n.parse::<u64>().ok())
                    })
                    .max()
                    .unwrap_or(0);
                (file, max_id + 1)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), 1),
            Err(e) => return Err(CronError::Io(e.to_string())),
        };
        Ok(Self {
            jobs: RwLock::new(jobs),
            jobs_path,
            next_id: AtomicU64::new(next_id),
        })
    }

    pub fn empty(workspace: &Path) -> Self {
        Self {
            jobs: RwLock::new(Vec::new()),
            jobs_path: workspace::cron_jobs_file(workspace),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn add(
        &self,
        label: Option<String>,
        message: String,
        action: JobAction,
        schedule: Schedule,
        chat_id: i64,
    ) -> Result<CronJob, CronError> {
        if let Schedule::Interval { every_seconds } = &schedule {
            if *every_seconds < 60 {
                return Err(CronError::Validation(
                    "interval must be at least 60 seconds".into(),
                ));
            }
        }
        let now = unix_now();
        let next_run = match &schedule {
            Schedule::Once { at_unix } => {
                if *at_unix <= now {
                    return Err(CronError::Validation(
                        "Scheduled time must be in the future".into(),
                    ));
                }
                Some(*at_unix)
            }
            _ => schedule.next_fire_after(now),
        };
        if matches!(&schedule, Schedule::Cron { .. }) && next_run.is_none() {
            return Err(CronError::Validation(
                "cron expression has no upcoming matches".into(),
            ));
        }
        let id = format!("job-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let job = CronJob {
            id: id.clone(),
            label,
            message: message.clone(),
            action,
            schedule: schedule.clone(),
            enabled: true,
            chat_id,
            created_at: now,
            last_run: None,
            next_run,
        };
        {
            let mut guard = self.jobs.write().expect("cron lock");
            guard.push(job.clone());
            Self::save_inner(&guard, &self.jobs_path)?;
        }
        Ok(job)
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut guard = self.jobs.write().expect("cron lock");
        if let Some(pos) = guard.iter().position(|j| j.id == id) {
            guard.remove(pos);
            let _ = Self::save_inner(&guard, &self.jobs_path);
            true
        } else {
            false
        }
    }

    pub fn enable(&self, id: &str) -> bool {
        let now = unix_now();
        let mut guard = self.jobs.write().expect("cron lock");
        if let Some(j) = guard.iter_mut().find(|x| x.id == id) {
            j.enabled = true;
            j.next_run = j.schedule.next_fire_after(now);
            let _ = Self::save_inner(&guard, &self.jobs_path);
            true
        } else {
            false
        }
    }

    pub fn disable(&self, id: &str) -> bool {
        let mut guard = self.jobs.write().expect("cron lock");
        if let Some(j) = guard.iter_mut().find(|x| x.id == id) {
            j.enabled = false;
            j.next_run = None;
            let _ = Self::save_inner(&guard, &self.jobs_path);
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> Vec<CronJob> {
        self.jobs.read().expect("cron lock").clone()
    }

    pub fn get(&self, id: &str) -> Option<CronJob> {
        self.jobs
            .read()
            .expect("cron lock")
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    pub fn find_due(&self, now: u64) -> Vec<CronJob> {
        self.jobs
            .read()
            .expect("cron lock")
            .iter()
            .filter(|j| j.enabled && j.next_run.map_or(false, |n| n <= now))
            .cloned()
            .collect()
    }

    pub fn mark_fired(&self, id: &str, now: u64) {
        let mut guard = self.jobs.write().expect("cron lock");
        if let Some(j) = guard.iter_mut().find(|x| x.id == id) {
            j.last_run = Some(now);
            j.next_run = match &j.schedule {
                Schedule::Once { .. } => {
                    j.enabled = false;
                    None
                }
                Schedule::Interval { every_seconds } => Some(now + every_seconds),
                Schedule::Cron { .. } => j.schedule.next_fire_after(now),
            };
            let _ = Self::save_inner(&guard, &self.jobs_path);
        }
    }
}

// --- CronTool ---

pub struct CronTool {
    store: Arc<CronStore>,
}

impl CronTool {
    #[inline]
    pub fn new(store: Arc<CronStore>) -> Self {
        Self { store }
    }
}

impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Manage scheduled jobs: add, list, remove, enable, disable. Jobs fire on schedule—either running the agent with a message or sending directly to Telegram. When both dom and dow are restricted, the job fires only when both match (AND semantics)."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "list", "remove", "enable", "disable"],
                    "description": "Action to perform"
                },
                "id": {
                    "type": "string",
                    "description": "Job ID (for remove/enable/disable)"
                },
                "message": {
                    "type": "string",
                    "description": "Message text for the job (for add)"
                },
                "schedule_type": {
                    "type": "string",
                    "enum": ["once", "interval", "cron"],
                    "description": "Schedule type (for add)"
                },
                "at_unix": {
                    "type": "integer",
                    "description": "Unix timestamp to fire (for schedule_type=once). Use either at_unix or delay, not both."
                },
                "delay": {
                    "type": "string",
                    "description": "Delay from now for one-shot (for schedule_type=once). E.g. '30m', '2h', '1d', '1w'. Use either delay or at_unix, not both."
                },
                "every_seconds": {
                    "type": "integer",
                    "description": "Interval in seconds (for schedule_type=interval, min 60)"
                },
                "cron_expr": {
                    "type": "string",
                    "description": "5-field cron expression: 'minute hour dom month dow' (for schedule_type=cron). Supports *, N, N-M, N,M, */N, N-M/S."
                },
                "job_action": {
                    "type": "string",
                    "enum": ["agent", "direct"],
                    "description": "How to execute: 'agent' runs agent loop, 'direct' sends message to chat. Default: direct"
                },
                "label": {
                    "type": "string",
                    "description": "Optional human-readable label"
                }
            },
            "required": ["action"]
        })
    }

    fn execute<'a>(&'a self, ctx: &'a ToolCtx, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let store = Arc::clone(&self.store);
        let args = args.clone();
        let ctx = ctx.clone();

        Box::pin(async move {
            let action = match args.get("action").and_then(Value::as_str) {
                Some(a) => a,
                _ => return ToolResult::error("missing 'action' argument"),
            };
            match action {
                "add" => {
                    let message = args
                        .get("message")
                        .and_then(Value::as_str)
                        .map(String::from);
                    let message = match message {
                        Some(m) if !m.is_empty() => m,
                        _ => return ToolResult::error("add requires non-empty 'message'"),
                    };
                    let schedule_type = args.get("schedule_type").and_then(Value::as_str);
                    let schedule = match schedule_type {
                        Some("once") => {
                            let at_unix_opt = args.get("at_unix").and_then(Value::as_i64);
                            let delay_opt = args.get("delay").and_then(Value::as_str);
                            let at_unix = match (at_unix_opt, delay_opt) {
                                (Some(t), None) => t as u64,
                                (None, Some(d)) => {
                                    let secs = match parse_delay(d) {
                                        Ok(s) => s,
                                        Err(e) => return ToolResult::error(e.to_string()),
                                    };
                                    let now = unix_now();
                                    now.saturating_add(secs)
                                }
                                (None, None) => {
                                    return ToolResult::error(
                                        "once requires either 'at_unix' or 'delay' (e.g. '30m', '2h')",
                                    );
                                }
                                (Some(_), Some(_)) => {
                                    return ToolResult::error(
                                        "once accepts either 'at_unix' or 'delay', not both",
                                    );
                                }
                            };
                            Schedule::Once { at_unix }
                        }
                        Some("interval") => {
                            let every = match args.get("every_seconds").and_then(Value::as_i64) {
                                Some(x) => x,
                                None => {
                                    return ToolResult::error(
                                        "interval requires 'every_seconds' (min 60)",
                                    );
                                }
                            };
                            if every < 60 {
                                return ToolResult::error("every_seconds must be at least 60");
                            }
                            Schedule::Interval {
                                every_seconds: every as u64,
                            }
                        }
                        Some("cron") => {
                            let expr = match args.get("cron_expr").and_then(Value::as_str) {
                                Some(s) => s,
                                None => return ToolResult::error("cron requires 'cron_expr'"),
                            };
                            if parse_cron_expr(expr).is_err() {
                                return ToolResult::error("invalid cron expression");
                            }
                            Schedule::Cron {
                                expr: expr.to_string(),
                            }
                        }
                        _ => {
                            return ToolResult::error(
                                "add requires 'schedule_type': once, interval, or cron",
                            );
                        }
                    };
                    let job_action = match args.get("job_action").and_then(Value::as_str) {
                        Some("agent") => JobAction::Agent,
                        _ => JobAction::Direct,
                    };
                    let label = args.get("label").and_then(Value::as_str).map(String::from);
                    let chat_id = match ctx.chat_id {
                        Some(id) => id,
                        None => {
                            return ToolResult::error("cron add requires chat_id (current chat)");
                        }
                    };
                    match store.add(label, message, job_action, schedule, chat_id) {
                        Ok(job) => ToolResult::ok(format!(
                            "Added job {} ({}): next_run={:?}",
                            job.id,
                            job.label.as_deref().unwrap_or("(no label)"),
                            job.next_run
                        )),
                        Err(e) => ToolResult::error(e.to_string()),
                    }
                }
                "list" => {
                    let jobs = store.list();
                    if jobs.is_empty() {
                        return ToolResult::ok("No scheduled jobs.");
                    }
                    let lines: Vec<String> = jobs
                        .iter()
                        .map(|j| {
                            let msg_preview = if j.message.len() > 40 {
                                format!("{}…", &j.message[..40])
                            } else {
                                j.message.clone()
                            };
                            format!(
                                "{} | {} | enabled={} | next_run={:?} | {}",
                                j.id,
                                j.label.as_deref().unwrap_or("(no label)"),
                                j.enabled,
                                j.next_run,
                                msg_preview
                            )
                        })
                        .collect();
                    ToolResult::ok(lines.join("\n"))
                }
                "remove" => {
                    let id = args.get("id").and_then(Value::as_str).unwrap_or("");
                    if id.is_empty() {
                        return ToolResult::error("remove requires 'id'");
                    }
                    let ok = store.remove(id);
                    ToolResult::ok(if ok { "Removed." } else { "Job not found." })
                }
                "enable" => {
                    let id = args.get("id").and_then(Value::as_str).unwrap_or("");
                    if id.is_empty() {
                        return ToolResult::error("enable requires 'id'");
                    }
                    let ok = store.enable(id);
                    ToolResult::ok(if ok { "Enabled." } else { "Job not found." })
                }
                "disable" => {
                    let id = args.get("id").and_then(Value::as_str).unwrap_or("");
                    if id.is_empty() {
                        return ToolResult::error("disable requires 'id'");
                    }
                    let ok = store.disable(id);
                    ToolResult::ok(if ok { "Disabled." } else { "Job not found." })
                }
                _ => ToolResult::error("action must be: add, list, remove, enable, disable"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_next_fire() {
        let s = Schedule::Once { at_unix: 1000 };
        assert_eq!(s.next_fire_after(500), Some(1000));
    }

    #[test]
    fn once_past() {
        let s = Schedule::Once { at_unix: 1000 };
        assert_eq!(s.next_fire_after(2000), None);
    }

    #[test]
    fn interval_next_fire() {
        let s = Schedule::Interval { every_seconds: 300 };
        assert_eq!(s.next_fire_after(1000), Some(1300));
    }

    #[test]
    fn parse_every_minute() {
        let e = parse_cron_expr("* * * * *").unwrap();
        assert_eq!(e.minutes.len(), 60);
        assert_eq!(e.hours.len(), 24);
    }

    #[test]
    fn parse_single_values() {
        let e = parse_cron_expr("0 9 1 1 0").unwrap();
        assert_eq!(e.minutes, vec![0]);
        assert_eq!(e.hours, vec![9]);
        assert_eq!(e.doms, vec![1]);
        assert_eq!(e.months, vec![1]);
        assert_eq!(e.dows, vec![0]);
    }

    #[test]
    fn parse_comma_list() {
        let e = parse_cron_expr("0,30 9,17 * * *").unwrap();
        assert_eq!(e.minutes, vec![0, 30]);
        assert_eq!(e.hours, vec![9, 17]);
    }

    #[test]
    fn parse_range() {
        let e = parse_cron_expr("0 9 * * 1-5").unwrap();
        assert_eq!(e.dows, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_step_wildcard() {
        let e = parse_cron_expr("*/15 * * * *").unwrap();
        assert_eq!(e.minutes, vec![0, 15, 30, 45]);
    }

    #[test]
    fn parse_step_range() {
        let e = parse_cron_expr("0-30/10 * * * *").unwrap();
        assert_eq!(e.minutes, vec![0, 10, 20, 30]);
    }

    #[test]
    fn parse_too_few_fields() {
        assert!(parse_cron_expr("* * *").is_err());
    }

    #[test]
    fn parse_too_many_fields() {
        assert!(parse_cron_expr("* * * * * *").is_err());
    }

    #[test]
    fn parse_out_of_range() {
        assert!(parse_cron_expr("60 * * * *").is_err());
    }

    #[test]
    fn parse_invalid_range() {
        assert!(parse_cron_expr("5-2 * * * *").is_err());
    }

    #[test]
    fn parse_empty() {
        assert!(parse_cron_expr("").is_err());
    }

    #[test]
    fn cron_next_fire() {
        let s = Schedule::Cron {
            expr: "0 9 * * *".to_string(),
        };
        let ref_time = 1739707200u64;
        let next = s.next_fire_after(ref_time);
        assert!(next.is_some());
    }

    #[test]
    fn cron_invalid_expr() {
        let s = Schedule::Cron {
            expr: "bad".to_string(),
        };
        assert_eq!(s.next_fire_after(1000), None);
    }

    #[test]
    fn add_returns_job_with_id() {
        let dir = std::env::temp_dir().join("icrab_cron_test_add");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = CronStore::empty(&dir);
        let job = store
            .add(
                None,
                "hello".into(),
                JobAction::Direct,
                Schedule::Once {
                    at_unix: 9999999999,
                },
                123,
            )
            .unwrap();
        assert!(job.id.starts_with("job-"));
        assert!(job.enabled);
        assert!(job.next_run.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_existing() {
        let dir = std::env::temp_dir().join("icrab_cron_test_remove");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = CronStore::empty(&dir);
        store
            .add(
                None,
                "x".into(),
                JobAction::Direct,
                Schedule::Once {
                    at_unix: 9999999999,
                },
                1,
            )
            .unwrap();
        assert!(store.remove("job-1"));
        assert!(store.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_due_filters() {
        let dir = std::env::temp_dir().join("icrab_cron_test_due");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = CronStore::empty(&dir);
        let base = unix_now();
        store
            .add(
                None,
                "due".into(),
                JobAction::Direct,
                Schedule::Once {
                    at_unix: base + 100,
                },
                1,
            )
            .unwrap();
        store
            .add(
                None,
                "later".into(),
                JobAction::Direct,
                Schedule::Once {
                    at_unix: base + 10_000,
                },
                1,
            )
            .unwrap();
        let due = store.find_due(base + 500);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].message, "due");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_fired_once_disables() {
        let dir = std::env::temp_dir().join("icrab_cron_test_fired_once");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = CronStore::empty(&dir);
        let base = unix_now();
        store
            .add(
                None,
                "x".into(),
                JobAction::Direct,
                Schedule::Once {
                    at_unix: base + 100,
                },
                1,
            )
            .unwrap();
        store.mark_fired("job-1", base + 100);
        let j = store.get("job-1").unwrap();
        assert!(!j.enabled);
        assert!(j.next_run.is_none());
        assert_eq!(j.last_run, Some(base + 100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_once_past_returns_error() {
        let dir = std::env::temp_dir().join("icrab_cron_test_past");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = CronStore::empty(&dir);
        let now = unix_now();
        let err = store
            .add(
                None,
                "x".into(),
                JobAction::Direct,
                Schedule::Once {
                    at_unix: now.saturating_sub(1),
                },
                1,
            )
            .unwrap_err();
        assert!(err.to_string().contains("future"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_delay_accepts_units() {
        assert_eq!(parse_delay("30s").unwrap(), 30);
        assert_eq!(parse_delay("5m").unwrap(), 300);
        assert_eq!(parse_delay("2h").unwrap(), 7200);
        assert_eq!(parse_delay("1d").unwrap(), 86400);
        assert_eq!(parse_delay("1w").unwrap(), 604_800);
        assert!(parse_delay("x").is_err());
        assert!(parse_delay("30x").is_err());
    }
}

#[cfg(test)]
mod tool_tests {
    use super::*;
    use crate::tools::context::ToolCtx;

    fn empty_ctx(chat_id: Option<i64>) -> ToolCtx {
        ToolCtx {
            workspace: std::env::temp_dir(),
            restrict_to_workspace: true,
            chat_id,
            channel: None,
            outbound_tx: None,
        }
    }

    #[tokio::test]
    async fn tool_name_and_desc() {
        let dir = std::env::temp_dir().join("icrab_cron_tool_name");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CronStore::empty(&dir));
        let tool = CronTool::new(store);
        assert_eq!(tool.name(), "cron");
        assert!(tool.description().contains("schedule"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cron_tool_list_empty() {
        let dir = std::env::temp_dir().join("icrab_cron_tool_list");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CronStore::empty(&dir));
        let tool = CronTool::new(store);
        let ctx = empty_ctx(Some(1));
        let args = serde_json::json!({ "action": "list" });
        let res = tool.execute(&ctx, &args).await;
        assert!(!res.is_error);
        assert!(res.for_llm.contains("No scheduled jobs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cron_tool_add_success() {
        let dir = std::env::temp_dir().join("icrab_cron_tool_add");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CronStore::empty(&dir));
        let tool = CronTool::new(store);
        let ctx = empty_ctx(Some(42));
        let args = serde_json::json!({
            "action": "add",
            "message": "Hello",
            "schedule_type": "once",
            "at_unix": 2_000_000_000
        });
        let res = tool.execute(&ctx, &args).await;
        assert!(!res.is_error);
        assert!(res.for_llm.contains("job-"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cron_tool_add_missing_chat_id() {
        let dir = std::env::temp_dir().join("icrab_cron_tool_no_chat");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CronStore::empty(&dir));
        let tool = CronTool::new(store);
        let ctx = empty_ctx(None);
        let args = serde_json::json!({
            "action": "add",
            "message": "Hi",
            "schedule_type": "once",
            "at_unix": 2_000_000_000
        });
        let res = tool.execute(&ctx, &args).await;
        assert!(res.is_error);
        assert!(res.for_llm.contains("chat_id"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cron_tool_add_once_with_delay_success() {
        let dir = std::env::temp_dir().join("icrab_cron_tool_delay");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CronStore::empty(&dir));
        let tool = CronTool::new(Arc::clone(&store));
        let ctx = empty_ctx(Some(1));
        let args = serde_json::json!({
            "action": "add",
            "message": "Remind me in 5 minutes",
            "schedule_type": "once",
            "delay": "5m"
        });
        let res = tool.execute(&ctx, &args).await;
        assert!(!res.is_error, "{}", res.for_llm);
        assert!(res.for_llm.contains("job-"));
        let jobs = store.list();
        assert_eq!(jobs.len(), 1);
        let next_run = jobs[0].next_run.unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            next_run >= now + 299 && next_run <= now + 301,
            "next_run should be ~now+300s, got next_run={} now={}",
            next_run,
            now
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cron_tool_add_once_neither_at_unix_nor_delay_returns_error() {
        let dir = std::env::temp_dir().join("icrab_cron_tool_once_missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CronStore::empty(&dir));
        let tool = CronTool::new(store);
        let ctx = empty_ctx(Some(1));
        let args = serde_json::json!({
            "action": "add",
            "message": "Hi",
            "schedule_type": "once"
        });
        let res = tool.execute(&ctx, &args).await;
        assert!(res.is_error);
        assert!(
            res.for_llm.contains("at_unix") || res.for_llm.contains("delay"),
            "error should mention at_unix or delay: {}",
            res.for_llm
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cron_tool_add_once_both_at_unix_and_delay_returns_error() {
        let dir = std::env::temp_dir().join("icrab_cron_tool_once_both");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CronStore::empty(&dir));
        let tool = CronTool::new(store);
        let ctx = empty_ctx(Some(1));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let args = serde_json::json!({
            "action": "add",
            "message": "Hi",
            "schedule_type": "once",
            "at_unix": now + 3600,
            "delay": "1h"
        });
        let res = tool.execute(&ctx, &args).await;
        assert!(res.is_error);
        assert!(
            res.for_llm.contains("not both") || res.for_llm.contains("either"),
            "error should say not both: {}",
            res.for_llm
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cron_tool_add_once_past_at_unix_returns_error() {
        let dir = std::env::temp_dir().join("icrab_cron_tool_past");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CronStore::empty(&dir));
        let tool = CronTool::new(store);
        let ctx = empty_ctx(Some(1));
        let args = serde_json::json!({
            "action": "add",
            "message": "Too late",
            "schedule_type": "once",
            "at_unix": 1
        });
        let res = tool.execute(&ctx, &args).await;
        assert!(res.is_error);
        assert!(
            res.for_llm.to_lowercase().contains("future"),
            "error should mention future: {}",
            res.for_llm
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
