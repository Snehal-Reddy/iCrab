//! Build system prompt: identity, bootstrap files, memory snippet, skills summary, tool list.
//!
//! When building the system prompt, inject the skills summary under a "Skills" section by calling
//! `crate::skills::build_skills_summary(workspace)` and appending the returned string to the prompt.
