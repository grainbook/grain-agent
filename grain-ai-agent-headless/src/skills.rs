//! Disk-based skills loader. Reads `<workspace>/.claude/skills/<name>/SKILL.md`
//! files, parses their YAML-like frontmatter, and returns
//! [`grain_agent_harness::Skill`] values ready to feed into
//! [`grain_agent_harness::format_skills_for_system_prompt`].
//!
//! Mirrors `packages/coding-agent/src/core/skills.ts` from the pi reference.
//!
//! Frontmatter format (kept minimal — no `serde_yaml` dep):
//!
//! ```markdown
//! ---
//! name: my-skill
//! description: One-line summary, sent to the LLM as the skill description.
//! disable_model_invocation: false
//! ---
//!
//! Skill body goes here; the LLM reads the full file on demand. Not used by
//! the loader itself.
//! ```
//!
//! - Missing frontmatter: the file is skipped (with a stderr warning).
//! - Missing `name`: fall back to the parent directory name.
//! - Missing `description`: defaults to an empty string.
//! - `disable_model_invocation` defaults to `false`.

use std::path::{Path, PathBuf};

use grain_agent_harness::Skill;
use thiserror::Error;

/// Default directory (relative to workspace root) to search for skills.
pub const DEFAULT_SKILLS_DIR: &str = ".claude/skills";

#[derive(Debug, Error)]
pub enum SkillsError {
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Find all skills under `dir`. Each subdirectory whose top-level file is
/// `SKILL.md` (with a parseable frontmatter) becomes one [`Skill`].
///
/// Missing directory is a no-op (returns `Ok(vec![])`) — callers don't need
/// to pre-check, and the absence of `.claude/skills` is the common case.
pub fn find_skills(dir: &Path) -> Result<Vec<Skill>, SkillsError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(dir).map_err(|source| SkillsError::Io {
        path: dir.display().to_string(),
        source,
    })?;

    let mut skills: Vec<Skill> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else { continue };
        // Refuse symlinked skill directories (and symlinked SKILL.md inside
        // them): a `<dir>/.claude/skills/evil` pointing at `/etc/` lets a
        // malicious workspace's skill metadata get injected into the system
        // prompt with a path the LLM might then try to read. `file_type()`
        // on Unix returns `is_symlink()` (not `is_dir()`) for symlinks-to-
        // dirs, so this also covers the cross-platform fallback case.
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        match std::fs::symlink_metadata(&skill_md) {
            Ok(m) if m.file_type().is_symlink() => {
                eprintln!(
                    "[warn] grain-headless: skipping symlinked skill file {}",
                    skill_md.display()
                );
                continue;
            }
            Ok(m) if !m.is_file() => continue,
            Err(_) => continue,
            Ok(_) => {}
        }
        match read_skill(&skill_md, &path) {
            Ok(skill) => skills.push(skill),
            Err(e) => {
                eprintln!(
                    "[warn] grain-headless: skipping {}: {e}",
                    skill_md.display()
                );
            }
        }
    }
    // Deterministic order: alphabetical by name.
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

fn read_skill(skill_md: &Path, dir: &Path) -> Result<Skill, SkillsError> {
    let content = std::fs::read_to_string(skill_md).map_err(|source| SkillsError::Io {
        path: skill_md.display().to_string(),
        source,
    })?;
    Ok(parse_skill(&content, skill_md, dir))
}

fn parse_skill(content: &str, skill_md: &Path, dir: &Path) -> Skill {
    let mut name = String::new();
    let mut description = String::new();
    let mut disable_model_invocation = false;

    if let Some(frontmatter) = extract_frontmatter(content) {
        for line in frontmatter.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else { continue };
            let key = key.trim();
            let value = unquote(value.trim());
            match key {
                "name" => name = value,
                "description" => description = value,
                "disable_model_invocation" | "disableModelInvocation" => {
                    disable_model_invocation = matches!(
                        value.to_ascii_lowercase().as_str(),
                        "true" | "yes" | "1"
                    );
                }
                _ => {}
            }
        }
    }

    if name.is_empty() {
        name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string();
    }

    Skill {
        name,
        description,
        file_path: skill_md.to_path_buf().to_string_lossy().into_owned(),
        disable_model_invocation,
    }
}

/// Extract the body between the first pair of `---` fences. Returns `None`
/// if no frontmatter is present.
///
/// Tolerant of a leading UTF-8 BOM (`\u{FEFF}`) and CRLF line endings —
/// both are common when SKILL.md is authored on Windows or saved via
/// "UTF-8 with BOM" editors.
fn extract_frontmatter(content: &str) -> Option<&str> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let body = content.strip_prefix("---")?;
    let body = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))
        .unwrap_or(body);
    // Match either Unix or CRLF closing fence.
    let end = body
        .find("\n---")
        .or_else(|| body.find("\r\n---"))?;
    Some(&body[..end])
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Resolve the skills directory: if `override_path` is set, use it as-is
/// (relative to cwd); otherwise look under `<workspace>/.claude/skills`.
pub fn resolve_skills_dir(workspace_root: &Path, override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        p.to_path_buf()
    } else {
        workspace_root.join(DEFAULT_SKILLS_DIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, content: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn missing_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let skills = find_skills(&dir.path().join("nowhere")).unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn loads_basic_skill_with_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "rust-helper",
            "---\nname: rust-helper\ndescription: helps with Rust\n---\n\nbody",
        );
        let skills = find_skills(dir.path()).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "rust-helper");
        assert_eq!(skills[0].description, "helps with Rust");
        assert!(!skills[0].disable_model_invocation);
        assert!(skills[0].file_path.ends_with("rust-helper/SKILL.md"));
    }

    #[test]
    fn name_falls_back_to_dir_name() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "naming-test",
            "---\ndescription: no name field\n---",
        );
        let skills = find_skills(dir.path()).unwrap();
        assert_eq!(skills[0].name, "naming-test");
    }

    #[test]
    fn disable_model_invocation_truthy_values() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "a",
            "---\nname: a\ndisable_model_invocation: true\n---",
        );
        write_skill(
            dir.path(),
            "b",
            "---\nname: b\ndisableModelInvocation: yes\n---",
        );
        write_skill(
            dir.path(),
            "c",
            "---\nname: c\ndisable_model_invocation: 1\n---",
        );
        write_skill(
            dir.path(),
            "d",
            "---\nname: d\ndisable_model_invocation: false\n---",
        );
        let skills = find_skills(dir.path()).unwrap();
        let by_name: std::collections::HashMap<_, _> =
            skills.iter().map(|s| (s.name.as_str(), s)).collect();
        assert!(by_name["a"].disable_model_invocation);
        assert!(by_name["b"].disable_model_invocation);
        assert!(by_name["c"].disable_model_invocation);
        assert!(!by_name["d"].disable_model_invocation);
    }

    #[test]
    fn skips_dirs_without_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        write_skill(dir.path(), "real", "---\nname: real\n---");
        let skills = find_skills(dir.path()).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "real");
    }

    #[test]
    fn quoted_values_unwrap_correctly() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "q",
            "---\nname: \"quoted name\"\ndescription: 'single quoted desc'\n---",
        );
        let skills = find_skills(dir.path()).unwrap();
        assert_eq!(skills[0].name, "quoted name");
        assert_eq!(skills[0].description, "single quoted desc");
    }

    #[test]
    fn results_sorted_alphabetically() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "zebra", "---\nname: zebra\n---");
        write_skill(dir.path(), "alpha", "---\nname: alpha\n---");
        write_skill(dir.path(), "mango", "---\nname: mango\n---");
        let skills = find_skills(dir.path()).unwrap();
        assert_eq!(
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "mango", "zebra"]
        );
    }

    #[test]
    fn resolve_skills_dir_uses_default_when_no_override() {
        let p = resolve_skills_dir(Path::new("/work"), None);
        assert_eq!(p, Path::new("/work/.claude/skills"));
    }

    #[test]
    fn resolve_skills_dir_honors_override() {
        let p = resolve_skills_dir(Path::new("/work"), Some(Path::new("/custom")));
        assert_eq!(p, Path::new("/custom"));
    }
}
