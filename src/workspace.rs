//! Workspace paths: memory/MEMORY.md, memory/YYYYMM/YYYYMMDD.md, sessions/<chat_id>.json, skills/<name>/SKILL.md, cron/jobs.json, bootstrap files.

use std::path::{Path, PathBuf};

/// Path to the skills directory under the workspace: `workspace/skills`.
#[inline]
pub fn skills_dir(workspace: &Path) -> PathBuf {
    workspace.join("skills")
}
