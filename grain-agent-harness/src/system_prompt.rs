//! System-prompt assembly fragments.
//!
//! Ports `packages/agent/src/harness/system-prompt.ts`. Skills loaded from
//! disk (or any other source) are rendered into a fixed XML block to be
//! appended onto the agent system prompt.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: String,
    #[serde(default)]
    pub disable_model_invocation: bool,
}

/// Format the model-visible portion of a skill list for system-prompt
/// inclusion. Skills with `disable_model_invocation = true` are filtered out.
pub fn format_skills_for_system_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("The following skills provide specialized instructions for specific tasks.\n");
    out.push_str("Read the full skill file when the task matches its description.\n");
    out.push_str(
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n",
    );
    out.push_str("<available_skills>\n");
    for skill in visible {
        out.push_str("  <skill>\n");
        out.push_str(&format!("    <name>{}</name>\n", escape_xml(&skill.name)));
        out.push_str(&format!(
            "    <description>{}</description>\n",
            escape_xml(&skill.description)
        ));
        out.push_str(&format!(
            "    <location>{}</location>\n",
            escape_xml(&skill.file_path)
        ));
        out.push_str("  </skill>\n");
    }
    out.push_str("</available_skills>");
    out
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
