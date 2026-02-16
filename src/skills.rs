//! Skills loader: list workspace/skills, read description from each SKILL.md, build summary for system prompt.
//!
//! **Context builder integration:** The agent context builder (e.g. `agent/context.rs`) should call
//! `skills::build_skills_summary(workspace)` when building the system prompt and inject the result
//! under a "Skills" section. The agent uses the `read_file` tool to open a skill's SKILL.md when needed.

use std::fs;
use std::io;
use std::path::Path;

use crate::workspace;

const MAX_DESC_LEN: usize = 200;
const DESCRIPTION_PREFIX: &str = "description:";

/// One skill: directory name, path for read_file, one-line description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub relative_path: String,
    pub description: String,
}

/// Errors from skills discovery or summary build.
#[derive(Debug)]
pub enum SkillsError {
    Io(io::Error),
}

impl std::error::Error for SkillsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SkillsError::Io(e) => Some(e),
        }
    }
}

impl std::fmt::Display for SkillsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillsError::Io(e) => write!(f, "skills I/O: {}", e),
        }
    }
}

impl From<io::Error> for SkillsError {
    fn from(e: io::Error) -> Self {
        SkillsError::Io(e)
    }
}

/// Truncate to at most MAX_DESC_LEN chars (no mid-char cut).
fn truncate_desc(s: &str) -> String {
    if s.len() <= MAX_DESC_LEN {
        s.to_string()
    } else {
        let mut end = MAX_DESC_LEN;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

/// Extract one-line description from SKILL.md content (no I/O).
/// Prefers the first line starting with `description:` (case-insensitive); otherwise first non-empty paragraph.
#[inline]
pub fn extract_description(content: &str) -> String {
    let mut in_paragraph = false;
    let mut paragraph_lines: Vec<&str> = Vec::new();

    for line in content.lines() {
        let t = line.trim();
        if t.len() >= DESCRIPTION_PREFIX.len()
            && t[..DESCRIPTION_PREFIX.len()].eq_ignore_ascii_case(DESCRIPTION_PREFIX)
        {
            let rest = t[DESCRIPTION_PREFIX.len()..].trim();
            if !rest.is_empty() {
                return truncate_desc(rest);
            }
        }
        if t.is_empty() {
            if in_paragraph {
                break;
            }
        } else {
            if !in_paragraph {
                in_paragraph = true;
                paragraph_lines.clear();
            }
            paragraph_lines.push(t);
        }
    }

    let paragraph: String = paragraph_lines.join(" ").trim().to_string();
    if paragraph.is_empty() {
        "(no description)".to_string()
    } else {
        truncate_desc(&paragraph)
    }
}

/// List skills under `workspace/skills`: each subdir with SKILL.md, sorted by name.
/// Missing or non-directory `workspace/skills` returns `Ok(vec![])`.
pub fn list_skills(workspace: &Path) -> Result<Vec<SkillInfo>, SkillsError> {
    let skills_root = workspace::skills_dir(workspace);
    let entries = match fs::read_dir(&skills_root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut skills = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let content = fs::read_to_string(&skill_md)?;
        let description = extract_description(&content);
        skills.push(SkillInfo {
            relative_path: format!("skills/{}/SKILL.md", name),
            name,
            description,
        });
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

fn description_suffix(desc: &str) -> &'static str {
    if desc.trim_end().ends_with(|c: char| c == '.' || c == '!' || c == '?') {
        " "
    } else {
        ". "
    }
}

/// Build the skills summary string for the system prompt: one line per skill.
/// Empty list returns `Ok(String::new())`.
pub fn build_skills_summary(workspace: &Path) -> Result<String, SkillsError> {
    let skills = list_skills(workspace)?;
    Ok(skills
        .into_iter()
        .map(|s| {
            let suffix = description_suffix(&s.description);
            format!(
                "- **{}** — {}{}Read {} to use.",
                s.name, s.description, suffix, s.relative_path
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_skills_root() -> PathBuf {
        let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir();
        let root = tmp.join(format!("icrab_skills_test_{}", n));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn extract_description_empty() {
        assert_eq!(extract_description(""), "(no description)");
    }

    #[test]
    fn extract_description_lowercase() {
        assert_eq!(
            extract_description("description: Get the weather."),
            "Get the weather."
        );
    }

    #[test]
    fn extract_description_case_insensitive() {
        assert_eq!(
            extract_description("Description: Get the weather."),
            "Get the weather."
        );
    }

    #[test]
    fn extract_description_first_paragraph() {
        assert_eq!(
            extract_description("\nGet current weather.\n\nMore text"),
            "Get current weather."
        );
    }

    #[test]
    fn extract_description_first_paragraph_single_line() {
        assert_eq!(
            extract_description("# Weather\n\nGet current weather.\n"),
            "# Weather"
        );
    }

    #[test]
    fn extract_description_truncate() {
        let long = "a".repeat(300);
        let out = extract_description(&format!("description: {}", long));
        assert!(out.len() <= MAX_DESC_LEN + 3);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn list_skills_no_dir() {
        let tmp = std::env::temp_dir();
        let missing = tmp.join("icrab_skills_missing_never_exists_12345");
        let r = list_skills(&missing).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn list_skills_empty_dir() {
        let root = temp_skills_root();
        let r = list_skills(&root).unwrap();
        assert!(r.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_skills_one_skill() {
        let root = temp_skills_root();
        let weather = root.join("skills").join("weather");
        fs::create_dir_all(&weather).unwrap();
        fs::write(
            weather.join("SKILL.md"),
            "description: Get current weather.",
        )
        .unwrap();
        let r = list_skills(&root).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "weather");
        assert_eq!(r[0].relative_path, "skills/weather/SKILL.md");
        assert_eq!(r[0].description, "Get current weather.");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_skills_two_skills_sorted() {
        let root = temp_skills_root();
        for (name, desc) in [("weather", "Get weather."), ("time", "Get time.")] {
            let dir = root.join("skills").join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("SKILL.md"), format!("description: {}", desc)).unwrap();
        }
        let r = list_skills(&root).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "time");
        assert_eq!(r[1].name, "weather");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_skills_subdir_without_skill_md_skipped() {
        let root = temp_skills_root();
        let weather = root.join("skills").join("weather");
        fs::create_dir_all(&weather).unwrap();
        fs::write(weather.join("SKILL.md"), "description: Weather.").unwrap();
        let no_skill = root.join("skills").join("no_skill");
        fs::create_dir_all(&no_skill).unwrap();
        let r = list_skills(&root).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "weather");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_skills_summary_no_skills() {
        let root = temp_skills_root();
        let s = build_skills_summary(&root).unwrap();
        assert_eq!(s, "");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_skills_summary_one_skill() {
        let root = temp_skills_root();
        let weather = root.join("skills").join("weather");
        fs::create_dir_all(&weather).unwrap();
        fs::write(
            weather.join("SKILL.md"),
            "description: Get current weather.",
        )
        .unwrap();
        let s = build_skills_summary(&root).unwrap();
        assert_eq!(
            s,
            "- **weather** — Get current weather. Read skills/weather/SKILL.md to use."
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn skills_error_display_and_source() {
        let e = SkillsError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "nope"));
        let _ = format!("{}", e);
        assert!(e.source().is_some());
    }
}
