//! Disk-based skills loader. Reads pi / Agent Skills compatible skill files,
//! parses their YAML-like frontmatter, and returns
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
//! - Missing `description`: skip the skill with a warning.
//! - `disable_model_invocation` defaults to `false`.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use grain_agent_harness::Skill;
use thiserror::Error;

/// Primary pi-native project directory (relative to workspace root).
pub const DEFAULT_SKILLS_DIR: &str = ".pi/skills";

#[derive(Debug, Error)]
pub enum SkillsError {
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Find all skills under `dir`.
///
/// Discovery mirrors pi's Agent Skills rules for an explicit directory:
/// - if a directory contains `SKILL.md`, it is a skill root and recursion
///   stops there;
/// - direct root `.md` files are loaded as individual skills;
/// - subdirectories are searched recursively for `SKILL.md`.
///
/// Missing directory is a no-op (returns `Ok(vec![])`) — callers don't need
/// to pre-check, and the absence of skills is the common case.
pub fn find_skills(dir: &Path) -> Result<Vec<Skill>, SkillsError> {
    find_skills_from_dir_internal(dir, true)
}

pub fn find_skills_in_dirs(dirs: &[PathBuf]) -> Result<Vec<Skill>, SkillsError> {
    let mut out = Vec::new();
    let mut seen_names = HashSet::new();
    let mut seen_paths = HashSet::new();
    for dir in dirs {
        let skills = find_skills(dir)?;
        for skill in skills {
            let path_key = std::fs::canonicalize(&skill.file_path)
                .unwrap_or_else(|_| PathBuf::from(&skill.file_path));
            if !seen_paths.insert(path_key) {
                continue;
            }
            if !seen_names.insert(skill.name.clone()) {
                eprintln!(
                    "[warn] grain-headless: skill name collision for {}; keeping first",
                    skill.name
                );
                continue;
            }
            out.push(skill);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn find_skills_from_dir_internal(
    dir: &Path,
    include_root_files: bool,
) -> Result<Vec<Skill>, SkillsError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(dir).map_err(|source| SkillsError::Io {
        path: dir.display().to_string(),
        source,
    })?;

    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|e| e.file_name());

    let skill_md = dir.join("SKILL.md");
    if is_regular_skill_file(&skill_md) {
        return match read_skill(&skill_md, dir)? {
            Some(skill) => Ok(vec![skill]),
            None => Ok(vec![]),
        };
    }

    let mut skills: Vec<Skill> = Vec::new();
    for entry in entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        if file_type.is_dir() {
            if entry.file_name().to_string_lossy().starts_with('.')
                || entry.file_name() == "node_modules"
            {
                continue;
            }
            match find_skills_from_dir_internal(&path, false) {
                Ok(extra) => skills.extend(extra),
                Err(e) => eprintln!("[warn] grain-headless: skills scan {}: {e}", path.display()),
            }
        } else if include_root_files
            && file_type.is_file()
            && path.extension().and_then(|e| e.to_str()) == Some("md")
        {
            match read_skill(&path, dir) {
                Ok(Some(skill)) => skills.push(skill),
                Ok(None) => {}
                Err(e) => {
                    eprintln!("[warn] grain-headless: skipping {}: {e}", path.display());
                }
            }
        }
    }
    // Deterministic order: alphabetical by name.
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

fn is_regular_skill_file(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            eprintln!(
                "[warn] grain-headless: skipping symlinked skill file {}",
                path.display()
            );
            false
        }
        Ok(m) => m.is_file(),
        Err(_) => false,
    }
}

fn read_skill(skill_md: &Path, dir: &Path) -> Result<Option<Skill>, SkillsError> {
    let content = std::fs::read_to_string(skill_md).map_err(|source| SkillsError::Io {
        path: skill_md.display().to_string(),
        source,
    })?;
    Ok(parse_skill(&content, skill_md, dir))
}

fn parse_skill(content: &str, skill_md: &Path, dir: &Path) -> Option<Skill> {
    let mut name = String::new();
    let mut description = String::new();
    let mut disable_model_invocation = false;

    if let Some(frontmatter) = extract_frontmatter(content) {
        for line in frontmatter.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = unquote(value.trim());
            match key {
                "name" => name = value,
                "description" => description = value,
                "disable-model-invocation"
                | "disable_model_invocation"
                | "disableModelInvocation" => {
                    disable_model_invocation =
                        matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1");
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

    if description.trim().is_empty() {
        eprintln!(
            "[warn] grain-headless: skipping {}: description is required",
            skill_md.display()
        );
        return None;
    }

    validate_skill_name(&name, skill_md);
    if description.len() > 1024 {
        eprintln!(
            "[warn] grain-headless: {} description exceeds 1024 characters ({})",
            skill_md.display(),
            description.len()
        );
    }

    Some(Skill {
        name,
        description,
        file_path: skill_md.to_path_buf().to_string_lossy().into_owned(),
        disable_model_invocation,
        body: content.to_string(),
    })
}

fn validate_skill_name(name: &str, skill_md: &Path) {
    if name.len() > 64 {
        eprintln!(
            "[warn] grain-headless: {} name exceeds 64 characters ({})",
            skill_md.display(),
            name.len()
        );
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        eprintln!(
            "[warn] grain-headless: {} name contains invalid characters",
            skill_md.display()
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        eprintln!(
            "[warn] grain-headless: {} name must not start or end with hyphen",
            skill_md.display()
        );
    }
    if name.contains("--") {
        eprintln!(
            "[warn] grain-headless: {} name must not contain consecutive hyphens",
            skill_md.display()
        );
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
    let end = body.find("\n---").or_else(|| body.find("\r\n---"))?;
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

/// Synthesize skill entries for project-level agent guidance files in
/// the workspace root. The model sees lightweight `<skill>` tags and
/// can read the full files on demand via the `read` tool — no bloat
/// injected into every request.
pub fn load_project_context_skills(workspace_root: &Path) -> Vec<Skill> {
    ["AGENTS.md", "CLAUDE.md"]
        .into_iter()
        .filter_map(|name| maybe_load_project_context_md(workspace_root, name))
        .collect()
}

/// Synthesize a skill entry for `<workspace>/AGENTS.md` when the file
/// exists, implementing support for the [AGENTS.md](https://agents.md/)
/// standard. Retained as a narrow helper for legacy call sites.
pub fn maybe_load_agents_md(workspace_root: &Path) -> Option<Skill> {
    maybe_load_project_context_md(workspace_root, "AGENTS.md")
}

fn maybe_load_project_context_md(workspace_root: &Path, filename: &str) -> Option<Skill> {
    let path = workspace_root.join(filename);
    if path.is_file() {
        // Refuse symlinks for the same reason we refuse symlinked skill
        // directories: a malicious workspace shouldn't be able to inject
        // arbitrary paths into the model's system prompt.
        if std::fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(true)
        {
            eprintln!(
                "[warn] grain-headless: skipping symlinked {filename} {}",
                path.display()
            );
            return None;
        }
        let digest = match std::fs::read(&path) {
            Ok(bytes) => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut hasher);
                hasher.finish()
            }
            Err(e) => {
                eprintln!(
                    "[warn] grain-headless: skipping unreadable {filename} {}: {e}",
                    path.display()
                );
                return None;
            }
        };
        Some(Skill {
            name: filename.into(),
            description: format!(
                "Project-level guidance for coding agents ({filename}, content digest {digest:016x})"
            ),
            file_path: path.to_string_lossy().into_owned(),
            disable_model_invocation: false,
            body: String::new(),
        })
    } else {
        None
    }
}

/// Resolve the primary skills directory for legacy call sites.
pub fn resolve_skills_dir(workspace_root: &Path, override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        p.to_path_buf()
    } else {
        workspace_root.join(DEFAULT_SKILLS_DIR)
    }
}

/// Resolve pi-compatible skill locations. An explicit override disables
/// default discovery and is treated as a single additive path.
pub fn resolve_skill_dirs(workspace_root: &Path, override_path: Option<&Path>) -> Vec<PathBuf> {
    resolve_skill_dirs_with_scope(workspace_root, override_path, false)
}

/// Resolve skill locations, optionally limiting discovery to the current
/// workspace. Workspace-only mode skips user-global and ancestor locations.
pub fn resolve_skill_dirs_with_scope(
    workspace_root: &Path,
    override_path: Option<&Path>,
    workspace_only: bool,
) -> Vec<PathBuf> {
    if let Some(p) = override_path {
        return vec![p.to_path_buf()];
    }

    let mut dirs = Vec::new();
    if !workspace_only && let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".pi/agent/skills"));
        dirs.push(home.join(".agents/skills"));
    }
    dirs.push(workspace_root.join(".pi/skills"));
    dirs.push(workspace_root.join(".agents/skills"));
    dirs.push(workspace_root.join(".grain/skills"));
    // Backward-compatible with this repo's earlier Claude-style default.
    dirs.push(workspace_root.join(".claude/skills"));

    if workspace_only {
        return dirs;
    }

    let mut current = Some(workspace_root);
    while let Some(dir) = current {
        let candidate = dir.join(".agents/skills");
        if !dirs.iter().any(|d| d == &candidate) {
            dirs.push(candidate);
        }
        if dir.join(".git").exists() {
            break;
        }
        current = dir.parent();
    }
    dirs
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
        assert!(skills[0].body.contains("description: helps with Rust"));
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
            "---\nname: a\ndescription: a\ndisable_model_invocation: true\n---",
        );
        write_skill(
            dir.path(),
            "b",
            "---\nname: b\ndescription: b\ndisableModelInvocation: yes\n---",
        );
        write_skill(
            dir.path(),
            "c",
            "---\nname: c\ndescription: c\ndisable-model-invocation: 1\n---",
        );
        write_skill(
            dir.path(),
            "d",
            "---\nname: d\ndescription: d\ndisable_model_invocation: false\n---",
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
        write_skill(
            dir.path(),
            "real",
            "---\nname: real\ndescription: real\n---",
        );
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
        write_skill(
            dir.path(),
            "zebra",
            "---\nname: zebra\ndescription: zebra\n---",
        );
        write_skill(
            dir.path(),
            "alpha",
            "---\nname: alpha\ndescription: alpha\n---",
        );
        write_skill(
            dir.path(),
            "mango",
            "---\nname: mango\ndescription: mango\n---",
        );
        let skills = find_skills(dir.path()).unwrap();
        assert_eq!(
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "mango", "zebra"]
        );
    }

    #[test]
    fn resolve_skills_dir_uses_default_when_no_override() {
        let p = resolve_skills_dir(Path::new("/work"), None);
        assert_eq!(p, Path::new("/work/.pi/skills"));
    }

    #[test]
    fn resolve_skills_dir_honors_override() {
        let p = resolve_skills_dir(Path::new("/work"), Some(Path::new("/custom")));
        assert_eq!(p, Path::new("/custom"));
    }

    #[test]
    fn resolve_skill_dirs_workspace_only_skips_global_and_ancestors() {
        let dirs = resolve_skill_dirs_with_scope(Path::new("/repo/sub"), None, true);

        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/repo/sub/.pi/skills"),
                PathBuf::from("/repo/sub/.agents/skills"),
                PathBuf::from("/repo/sub/.grain/skills"),
                PathBuf::from("/repo/sub/.claude/skills"),
            ]
        );
    }

    #[test]
    fn resolve_skill_dirs_workspace_only_still_honors_override() {
        let dirs =
            resolve_skill_dirs_with_scope(Path::new("/repo/sub"), Some(Path::new("/custom")), true);

        assert_eq!(dirs, vec![PathBuf::from("/custom")]);
    }

    #[test]
    fn agents_md_present_in_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(
            &path,
            "# Project

- Rule A
- Rule B
",
        )
        .unwrap();
        let skill = maybe_load_agents_md(dir.path()).expect("should detect AGENTS.md");
        assert_eq!(skill.name, "AGENTS.md");
        assert!(skill.description.contains("AGENTS.md"));
        assert_eq!(skill.file_path, path.to_string_lossy());
        assert!(!skill.disable_model_invocation);
        assert!(skill.body.is_empty());
    }

    #[test]
    fn agents_md_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(maybe_load_agents_md(dir.path()).is_none());
    }

    #[test]
    fn project_context_skills_include_agents_and_claude() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agents\n").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# Claude\n").unwrap();

        let skills = load_project_context_skills(dir.path());
        assert_eq!(skills.len(), 2);
        assert!(skills.iter().any(|s| s.name == "AGENTS.md"));
        assert!(skills.iter().any(|s| s.name == "CLAUDE.md"));
    }

    #[test]
    fn agents_md_symlink_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.md");
        std::fs::write(
            &real, "# rules
",
        )
        .unwrap();
        let link = dir.path().join("AGENTS.md");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let got = maybe_load_agents_md(dir.path());
            assert!(got.is_none(), "symlink AGENTS.md must be rejected");
        }
        #[cfg(not(unix))]
        {
            let _ = (&real, &link); // avoid unused warnings
        }
    }
}
